pub mod consensus_aggregator;
pub mod signer;
pub mod single_aggregator;

use std::time::SystemTime;

use anyhow::Result;
use consensus_aggregator::ConsensusSignatureAggregator;
use minicbor::Encoder;
use pallas_primitives::conway::PlutusData;
use rust_decimal::prelude::ToPrimitive;
use serde::Serialize;
pub use single_aggregator::SingleSignatureAggregator;
use tokio::{
    select,
    sync::{mpsc, watch},
};
use tracing::Instrument;

use crate::{
    config::OracleConfig,
    network::Network,
    price_feed::{PriceFeedEntry, SignedPriceFeedEntry},
    raft::RaftLeader,
};

pub struct SignatureAggregator {
    implementation: SignatureAggregatorImplementation,
    signed_price_source: mpsc::Receiver<Vec<SignedPriceFeedEntry>>,
    payload_sink: watch::Sender<SignedPayload>,
}
enum SignatureAggregatorImplementation {
    Single(SingleSignatureAggregator),
    Consensus(ConsensusSignatureAggregator),
}

impl SignatureAggregator {
    pub fn single(
        price_source: watch::Receiver<Vec<PriceFeedEntry>>,
        leader_source: watch::Receiver<RaftLeader>,
    ) -> Result<(Self, watch::Receiver<SignedPayload>)> {
        let (signed_price_sink, signed_price_source) = mpsc::channel(10);
        let (payload_sink, mut payload_source) = watch::channel(SignedPayload {
            timestamp: SystemTime::now(),
            entries: vec![],
        });
        payload_source.mark_unchanged();

        let aggregator =
            SingleSignatureAggregator::new(price_source, leader_source, signed_price_sink)?;
        let aggregator = SignatureAggregator {
            implementation: SignatureAggregatorImplementation::Single(aggregator),
            signed_price_source,
            payload_sink,
        };
        Ok((aggregator, payload_source))
    }

    pub fn consensus(
        config: &OracleConfig,
        network: &mut Network,
        price_source: watch::Receiver<Vec<PriceFeedEntry>>,
        leader_source: watch::Receiver<RaftLeader>,
    ) -> Result<(Self, watch::Receiver<SignedPayload>)> {
        let (signed_price_sink, signed_price_source) = mpsc::channel(10);
        let (payload_sink, mut payload_source) = watch::channel(SignedPayload {
            timestamp: SystemTime::now(),
            entries: vec![],
        });
        payload_source.mark_unchanged();

        let aggregator = ConsensusSignatureAggregator::new(
            config,
            network.id.clone(),
            network.signer_channel(),
            price_source,
            leader_source,
            signed_price_sink,
        )?;
        let aggregator = SignatureAggregator {
            implementation: SignatureAggregatorImplementation::Consensus(aggregator),
            signed_price_source,
            payload_sink,
        };
        Ok((aggregator, payload_source))
    }

    pub async fn run(self) {
        let implementation = self.implementation;
        let run_task = async move {
            match implementation {
                SignatureAggregatorImplementation::Single(s) => s.run().await,
                SignatureAggregatorImplementation::Consensus(c) => c.run().await,
            }
        }
        .in_current_span();

        let mut signed_price_source = self.signed_price_source;
        let publish_task = async move {
            while let Some(signed_prices) = signed_price_source.recv().await {
                let entries = signed_prices
                    .iter()
                    .map(|price| {
                        let payload = {
                            let data = PlutusData::Array(vec![(&price.data).into()]);
                            let mut encoder = Encoder::new(vec![]);
                            encoder.encode(data).expect("encoding is infallible");
                            encoder.into_writer()
                        };
                        SerializablePayloadEntry {
                            synthetic: price.data.data.synthetic.clone(),
                            price: price.price.to_f64().expect("Could not convert decimal"),
                            payload: hex::encode(payload),
                        }
                    })
                    .collect();
                let payload = SignedPayload {
                    timestamp: SystemTime::now(),
                    entries,
                };
                self.payload_sink.send_replace(payload);
            }
        }
        .in_current_span();

        select! {
            res = run_task => res,
            res = publish_task => res,
        }
    }
}

#[derive(Serialize)]
pub struct SerializablePayloadEntry {
    pub synthetic: String,
    pub price: f64,
    pub payload: String,
}

#[derive(Serialize)]
pub struct SignedPayload {
    pub timestamp: SystemTime,
    pub entries: Vec<SerializablePayloadEntry>,
}
