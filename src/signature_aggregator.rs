pub mod consensus_aggregator;
pub mod signer;
pub mod single_aggregator;

use std::time::SystemTime;

use anyhow::Result;
use consensus_aggregator::ConsensusSignatureAggregator;
use minicbor::Encoder;
use pallas_primitives::conway::PlutusData;
use serde::Serialize;
pub use single_aggregator::SingleSignatureAggregator;
use tokio::{
    select,
    sync::{mpsc, watch},
};
use tracing::Instrument;

use crate::{
    config::OracleConfig,
    network::{Network, NodeId},
    price_feed::{Payload, PriceFeedEntry},
    raft::RaftLeader,
};

pub struct SignatureAggregator {
    implementation: SignatureAggregatorImplementation,
    payload_source: mpsc::Receiver<(NodeId, Payload)>,
    serializable_payload_sink: watch::Sender<SerializablePayload>,
}
enum SignatureAggregatorImplementation {
    Single(SingleSignatureAggregator),
    Consensus(ConsensusSignatureAggregator),
}

impl SignatureAggregator {
    pub fn single(
        id: &NodeId,
        price_source: watch::Receiver<Vec<PriceFeedEntry>>,
        leader_source: watch::Receiver<RaftLeader>,
    ) -> Result<(Self, watch::Receiver<SerializablePayload>)> {
        let (payload_sink, payload_source) = mpsc::channel(10);
        let (serializable_payload_sink, mut serializable_payload_source) =
            watch::channel(SerializablePayload {
                publisher: id.clone(),
                timestamp: SystemTime::now(),
                entries: vec![],
            });
        serializable_payload_source.mark_unchanged();

        let aggregator =
            SingleSignatureAggregator::new(id, price_source, leader_source, payload_sink)?;
        let aggregator = SignatureAggregator {
            implementation: SignatureAggregatorImplementation::Single(aggregator),
            payload_source,
            serializable_payload_sink,
        };
        Ok((aggregator, serializable_payload_source))
    }

    pub fn consensus(
        config: &OracleConfig,
        network: &mut Network,
        price_source: watch::Receiver<Vec<PriceFeedEntry>>,
        leader_source: watch::Receiver<RaftLeader>,
    ) -> Result<(Self, watch::Receiver<SerializablePayload>)> {
        let (payload_sink, payload_source) = mpsc::channel(10);
        let (serializable_payload_sink, mut serializable_payload_source) =
            watch::channel(SerializablePayload {
                publisher: network.id.clone(),
                timestamp: SystemTime::now(),
                entries: vec![],
            });
        serializable_payload_source.mark_unchanged();

        let aggregator = ConsensusSignatureAggregator::new(
            config,
            network.id.clone(),
            network.signer_channel(),
            price_source,
            leader_source,
            payload_sink,
        )?;
        let aggregator = SignatureAggregator {
            implementation: SignatureAggregatorImplementation::Consensus(aggregator),
            payload_source,
            serializable_payload_sink,
        };
        Ok((aggregator, serializable_payload_source))
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

        let mut payload_source = self.payload_source;
        let publish_task = async move {
            while let Some((publisher, payload)) = payload_source.recv().await {
                let entries = payload
                    .entries
                    .iter()
                    .map(|entry| {
                        let payload = {
                            let data = PlutusData::Array(vec![(&entry.data).into()]);
                            let mut encoder = Encoder::new(vec![]);
                            encoder.encode(data).expect("encoding is infallible");
                            encoder.into_writer()
                        };
                        SerializablePayloadEntry {
                            synthetic: entry.data.data.synthetic.clone(),
                            price: entry.price,
                            payload: hex::encode(payload),
                        }
                    })
                    .collect();
                let payload = SerializablePayload {
                    publisher,
                    timestamp: payload.timestamp,
                    entries,
                };
                self.serializable_payload_sink.send_replace(payload);
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
pub struct SerializablePayload {
    pub publisher: NodeId,
    pub timestamp: SystemTime,
    pub entries: Vec<SerializablePayloadEntry>,
}
