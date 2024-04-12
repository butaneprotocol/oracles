pub mod consensus_aggregator;
pub mod signer;
pub mod single_aggregator;

use anyhow::Result;
use consensus_aggregator::ConsensusSignatureAggregator;
use minicbor::Encoder;
use pallas_primitives::conway::PlutusData;
use rust_decimal::prelude::ToPrimitive;
use serde::Serialize;
pub use single_aggregator::SingleSignatureAggregator;
use tokio::{
    select,
    sync::{mpsc, watch::Receiver},
};
use tracing::{warn, Instrument};

use crate::{
    networking::Network,
    price_feed::{PriceFeedEntry, SignedPriceFeedEntry},
    raft::RaftLeader,
};

use self::signer::SignerMessage;

pub struct SignatureAggregator {
    implementation: SignatureAggregatorImplementation,
    signed_price_source: mpsc::Receiver<Vec<SignedPriceFeedEntry>>,
    payload_sink: mpsc::Sender<String>,
}
enum SignatureAggregatorImplementation {
    Single(SingleSignatureAggregator),
    Consensus(ConsensusSignatureAggregator),
}

impl SignatureAggregator {
    pub fn single(
        price_source: Receiver<Vec<PriceFeedEntry>>,
        leader_source: Receiver<RaftLeader>,
        payload_sink: mpsc::Sender<String>,
    ) -> Result<Self> {
        let (signed_price_sink, signed_price_source) = mpsc::channel(10);
        let aggregator =
            SingleSignatureAggregator::new(price_source, leader_source, signed_price_sink)?;
        Ok(SignatureAggregator {
            implementation: SignatureAggregatorImplementation::Single(aggregator),
            signed_price_source,
            payload_sink,
        })
    }

    pub fn consensus(
        id: String,
        network: Network,
        message_source: mpsc::Receiver<SignerMessage>,
        price_source: Receiver<Vec<PriceFeedEntry>>,
        leader_source: Receiver<RaftLeader>,
        payload_sink: mpsc::Sender<String>,
    ) -> Result<Self> {
        let (signed_price_sink, signed_price_source) = mpsc::channel(10);
        let aggregator = ConsensusSignatureAggregator::new(
            id,
            network,
            message_source,
            price_source,
            leader_source,
            signed_price_sink,
        )?;
        Ok(SignatureAggregator {
            implementation: SignatureAggregatorImplementation::Consensus(aggregator),
            signed_price_source,
            payload_sink,
        })
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
                let serializable_response: Vec<_> = signed_prices
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
                let payload = serde_json::to_string(&serializable_response)
                    .expect("serialization should be infallible");
                if let Err(error) = self.payload_sink.send(payload).await {
                    warn!("Could not send payload: {}", error);
                }
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
struct SerializablePayloadEntry {
    pub synthetic: String,
    pub price: f64,
    pub payload: String,
}
