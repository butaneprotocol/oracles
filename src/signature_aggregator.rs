pub mod consensus_aggregator;
pub mod price_comparator;
pub mod signer;
pub mod single_aggregator;

use std::time::{Duration, SystemTime};

use anyhow::Result;
use consensus_aggregator::ConsensusSignatureAggregator;
use minicbor::Encoder;
use pallas_primitives::conway::PlutusData;
use serde::Serialize;
pub use single_aggregator::SingleSignatureAggregator;
use tokio::{
    select,
    sync::{mpsc, watch},
    time::sleep,
};
use tracing::debug;

use crate::{
    config::OracleConfig,
    network::{Network, NodeId},
    price_feed::{IntervalBoundType, PriceFeedEntry, SignedEntries},
    raft::RaftLeader,
};

pub struct SignatureAggregator {
    implementation: SignatureAggregatorImplementation,
    signed_entries_source: mpsc::Receiver<(NodeId, SignedEntries)>,
    payload_sink: watch::Sender<Payload>,
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
    ) -> Result<(Self, watch::Receiver<Payload>)> {
        let (signed_entries_sink, signed_entries_source) = mpsc::channel(10);
        let (payload_sink, mut payload_source) = watch::channel(Payload {
            publisher: id.clone(),
            timestamp: SystemTime::now(),
            entries: vec![],
        });
        payload_source.mark_unchanged();

        let aggregator =
            SingleSignatureAggregator::new(id, price_source, leader_source, signed_entries_sink)?;
        let aggregator = SignatureAggregator {
            implementation: SignatureAggregatorImplementation::Single(aggregator),
            signed_entries_source,
            payload_sink,
        };
        Ok((aggregator, payload_source))
    }

    pub fn consensus(
        config: &OracleConfig,
        network: &mut Network,
        price_source: watch::Receiver<Vec<PriceFeedEntry>>,
        leader_source: watch::Receiver<RaftLeader>,
    ) -> Result<(Self, watch::Receiver<Payload>)> {
        let (signed_entries_sink, signed_entries_source) = mpsc::channel(10);
        let (payload_sink, mut payload_source) = watch::channel(Payload {
            publisher: network.id.clone(),
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
            signed_entries_sink,
        )?;
        let aggregator = SignatureAggregator {
            implementation: SignatureAggregatorImplementation::Consensus(aggregator),
            signed_entries_source,
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
        };

        let (payload_age_sink, payload_age_source) = {
            let now = SystemTime::now();
            let later = now + Duration::from_secs(60);
            watch::channel((now, later))
        };

        let monitor_task = async move {
            loop {
                let (last_updated, valid_until) = *payload_age_source.borrow();
                let is_valid: u64 = if SystemTime::now() < valid_until {
                    1
                } else {
                    0
                };
                if let Ok(age) = SystemTime::now().duration_since(last_updated) {
                    if let Ok(age_in_millis) = u64::try_from(age.as_millis()) {
                        debug!(
                            histogram.payload_age = age_in_millis,
                            histogram.payload_is_valid = is_valid,
                            "payload metrics"
                        );
                    }
                }
                sleep(Duration::from_secs(1)).await;
            }
        };

        let mut payload_source = self.signed_entries_source;
        let publish_task = async move {
            while let Some((publisher, payload)) = payload_source.recv().await {
                // update the payload age monitor now that we have a new payload
                let last_updated = payload.timestamp;
                let valid_to = find_end_of_payload_validity(&payload);
                payload_age_sink.send_replace((last_updated, valid_to));

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
                        PayloadEntry {
                            synthetic: entry.data.data.synthetic.clone(),
                            price: entry.price,
                            payload: hex::encode(payload),
                        }
                    })
                    .collect();
                let payload = Payload {
                    publisher,
                    timestamp: payload.timestamp,
                    entries,
                };
                self.payload_sink.send_replace(payload);
            }
        };

        select! {
            res = run_task => res,
            res = monitor_task => res,
            res = publish_task => res,
        }
    }
}

fn find_end_of_payload_validity(payload: &SignedEntries) -> SystemTime {
    let mut valid_to = None;
    for price_feed in &payload.entries {
        let upper_bound = price_feed.data.data.validity.upper_bound;
        match upper_bound.bound_type {
            IntervalBoundType::NegativeInfinity => {
                return SystemTime::UNIX_EPOCH;
            }
            IntervalBoundType::PositiveInfinity => {
                continue;
            }
            IntervalBoundType::Finite(unix_timestamp) => {
                let payload_valid_to =
                    SystemTime::UNIX_EPOCH + Duration::from_millis(unix_timestamp);
                if valid_to.is_some_and(|v| v < payload_valid_to) {
                    continue;
                }
                valid_to.replace(payload_valid_to);
            }
        }
    }
    valid_to.unwrap_or(SystemTime::UNIX_EPOCH)
}

#[derive(Serialize, Clone)]
pub struct PayloadEntry {
    pub synthetic: String,
    pub price: f64,
    pub payload: String,
}

#[derive(Serialize, Clone)]
pub struct Payload {
    pub publisher: NodeId,
    pub timestamp: SystemTime,
    pub entries: Vec<PayloadEntry>,
}
