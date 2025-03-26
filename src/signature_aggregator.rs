pub mod consensus_aggregator;
pub mod price_comparator;
pub mod price_instrumentation;
pub mod signer;
pub mod single_aggregator;

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::{Duration, SystemTime},
};

use anyhow::Result;
use consensus_aggregator::ConsensusSignatureAggregator;
use dashmap::DashMap;
use minicbor::CborLen;
use serde::Serialize;
pub use single_aggregator::SingleSignatureAggregator;
use tokio::{
    select,
    sync::{mpsc, watch},
    time::sleep,
};
use tracing::{debug, info_span, warn};

use crate::{
    config::OracleConfig,
    network::{Network, NodeId},
    price_feed::{
        cbor_encode_in_list, GenericPriceFeed, IntervalBoundType, PriceData, Signed, SignedEntries,
        SyntheticPriceFeed,
    },
    raft::{RaftClient, RaftLeader},
};

pub struct SignatureAggregator {
    id: NodeId,
    implementation: SignatureAggregatorImplementation,
    feeds: Vec<String>,
    raft_client: RaftClient,
    signed_entries_source: mpsc::Receiver<(NodeId, SignedEntries)>,
    payload_sink: watch::Sender<Payload>,
}
enum SignatureAggregatorImplementation {
    Single(SingleSignatureAggregator),
    Consensus(ConsensusSignatureAggregator),
}

impl SignatureAggregator {
    pub fn single(
        config: &OracleConfig,
        raft_client: RaftClient,
        price_source: watch::Receiver<PriceData>,
        leader_source: watch::Receiver<RaftLeader>,
    ) -> Result<(Self, watch::Receiver<Payload>)> {
        SignatureAggregator::new(config, raft_client, |signed_entries_sink| {
            let implementation = SingleSignatureAggregator::new(
                config,
                price_source,
                leader_source,
                signed_entries_sink,
            )?;
            Ok(SignatureAggregatorImplementation::Single(implementation))
        })
    }

    pub fn consensus(
        config: &OracleConfig,
        network: &mut Network,
        raft_client: RaftClient,
        price_source: watch::Receiver<PriceData>,
        leader_source: watch::Receiver<RaftLeader>,
    ) -> Result<(Self, watch::Receiver<Payload>)> {
        SignatureAggregator::new(config, raft_client, |signed_entries_sink| {
            let implementation = ConsensusSignatureAggregator::new(
                config,
                network.signer_channel(),
                price_source,
                leader_source,
                signed_entries_sink,
            )?;
            Ok(SignatureAggregatorImplementation::Consensus(implementation))
        })
    }

    fn new<F>(
        config: &OracleConfig,
        raft_client: RaftClient,
        factory: F,
    ) -> Result<(Self, watch::Receiver<Payload>)>
    where
        F: FnOnce(
            mpsc::Sender<(NodeId, SignedEntries)>,
        ) -> Result<SignatureAggregatorImplementation>,
    {
        let mut feeds = vec![];
        for synthetic in &config.synthetics {
            feeds.push(synthetic.name.clone());
        }
        for currency in &config.feeds.currencies {
            feeds.push(format!("USD/{currency}#RAW"));
            feeds.push(format!("{currency}/USD#RAW"));
            feeds.push(format!("USD/{currency}#GEMA"));
            feeds.push(format!("{currency}/USD#GEMA"));
        }
        let (signed_entries_sink, signed_entries_source) = mpsc::channel(10);
        let (payload_sink, mut payload_source) = watch::channel(Payload {
            publisher: config.id.clone(),
            timestamp: SystemTime::UNIX_EPOCH,
            synthetics: vec![],
            generics: vec![],
        });
        payload_source.mark_unchanged();

        let implementation = factory(signed_entries_sink)?;
        let aggregator = SignatureAggregator {
            id: config.id.clone(),
            implementation,
            feeds,
            raft_client,
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

        let payload_ages: DashMap<String, (SystemTime, Option<SystemTime>)> = self
            .feeds
            .iter()
            .map(|feed| {
                let now = SystemTime::now();
                let later = Some(now + Duration::from_secs(60));
                (feed.clone(), (now, later))
            })
            .collect();
        let payload_ages = Arc::new(payload_ages);

        let ages = payload_ages.clone();
        let monitor_task = async move {
            loop {
                for entry in ages.iter() {
                    let synthetic = entry.key().clone();
                    let (last_updated, valid_until) = *entry.value();
                    drop(entry);

                    let is_valid: u64 = if valid_until.is_none_or(|t| SystemTime::now() < t) {
                        1
                    } else {
                        0
                    };
                    if let Ok(age) = SystemTime::now().duration_since(last_updated) {
                        if let Ok(age_in_millis) = u64::try_from(age.as_millis()) {
                            debug!(
                                histogram.price_feed_age = age_in_millis,
                                histogram.price_feed_is_valid = is_valid,
                                synthetic,
                                "price feed metrics"
                            );
                        }
                    }
                }
                sleep(Duration::from_secs(1)).await;
            }
        };

        let id = self.id;
        let feeds = self.feeds;
        let raft_client = self.raft_client;
        let mut payload_source = self.signed_entries_source;
        let publish_task = async move {
            // map of feed to most recent entry produced for that feed
            let mut latest_entries: BTreeMap<String, PayloadEntry> = BTreeMap::new();

            // map of feed name to number of times it's failed in a row
            let mut all_failures: BTreeMap<String, usize> =
                feeds.iter().map(|s| (s.clone(), 0)).collect();

            while let Some((publisher, payload)) = payload_source.recv().await {
                let span = info_span!("publish_entries");
                let mut updated = BTreeSet::new();
                // update the payload age monitor now that we have a new payload
                let synthetics = payload.synthetics.into_iter().map(|s| {
                    PayloadEntry::Synthetic(SyntheticPayloadEntry {
                        timestamp: s.timestamp.unwrap_or(payload.timestamp),
                        synthetic: s.feed.data.synthetic.clone(),
                        price: s.price,
                        payload: s.feed,
                    })
                });
                let generics = payload.generics.into_iter().map(|i| {
                    PayloadEntry::Generic(GenericPayloadEntry {
                        timestamp: i.timestamp,
                        name: i.feed.data.name.clone(),
                        payload: i.feed,
                    })
                });
                for entry in synthetics.chain(generics) {
                    let feed = entry.feed();
                    if latest_entries
                        .get(&feed)
                        .is_some_and(|e| e.timestamp() >= entry.timestamp())
                    {
                        // This isn't actually a NEW payload...
                        continue;
                    }
                    updated.insert(feed.clone());
                    if let PayloadEntry::Synthetic(synth) = &entry {
                        let price_feed_size = synth.payload.cbor_len(&mut ()) as u64;
                        span.in_scope(|| {
                            debug!(
                                histogram.price_feed_size = price_feed_size,
                                synthetic = synth.synthetic,
                                "price feed size metrics"
                            );
                        });
                    }
                    payload_ages.insert(feed.clone(), (entry.timestamp(), entry.valid_until()));
                    latest_entries.insert(feed, entry);
                }

                if publisher == id {
                    // count the number of times we failed to publish some synthetic
                    let mut something_failed_too_often = false;
                    for (feed, failures) in all_failures.iter_mut() {
                        if updated.contains(feed) {
                            *failures = 0;
                        } else {
                            *failures += 1;
                            span.in_scope(|| {
                                warn!(
                                    feed,
                                    failures, "Could not publish new price feed for synthetic"
                                );
                            });
                            if *failures >= 5 {
                                something_failed_too_often = true;
                            }
                        }
                    }
                    // If we failed to publish some synthetic 5x in a row, we should stop publishing
                    if something_failed_too_often {
                        span.in_scope(|| {
                            warn!("Too many failures, we are no longer fit to be leader");
                        });
                        raft_client.abdicate();
                    }
                } else {
                    for failures in all_failures.values_mut() {
                        *failures = 0;
                    }
                }

                let mut payload = Payload {
                    publisher,
                    timestamp: payload.timestamp,
                    synthetics: vec![],
                    generics: vec![],
                };
                for entry in latest_entries.values().cloned() {
                    match entry {
                        PayloadEntry::Synthetic(entry) => payload.synthetics.push(entry),
                        PayloadEntry::Generic(entry) => payload.generics.push(entry),
                    }
                }
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

#[derive(Clone)]
enum PayloadEntry {
    Synthetic(SyntheticPayloadEntry),
    Generic(GenericPayloadEntry),
}
impl PayloadEntry {
    fn feed(&self) -> String {
        match self {
            Self::Synthetic(entry) => entry.synthetic.clone(),
            Self::Generic(entry) => entry.name.clone(),
        }
    }

    fn timestamp(&self) -> SystemTime {
        match self {
            Self::Synthetic(entry) => entry.timestamp,
            Self::Generic(entry) => entry.timestamp,
        }
    }

    fn valid_until(&self) -> Option<SystemTime> {
        match self {
            Self::Synthetic(entry) => Some(find_end_of_entry_validity(entry)),
            Self::Generic(_) => None,
        }
    }
}

fn find_end_of_entry_validity(entry: &SyntheticPayloadEntry) -> SystemTime {
    let upper_bound = entry.payload.data.validity.upper_bound;
    match upper_bound.bound_type {
        IntervalBoundType::NegativeInfinity => SystemTime::UNIX_EPOCH,
        IntervalBoundType::PositiveInfinity => SystemTime::now() + Duration::from_secs(60 * 525600),
        IntervalBoundType::Finite(unix_timestamp) => {
            SystemTime::UNIX_EPOCH + Duration::from_millis(unix_timestamp)
        }
    }
}

#[derive(Serialize, Clone)]
pub struct SyntheticPayloadEntry {
    pub timestamp: SystemTime,
    pub synthetic: String,
    pub price: f64,
    #[serde(serialize_with = "cbor_encode_in_list")]
    pub payload: Signed<SyntheticPriceFeed>,
}

#[derive(Serialize, Clone)]
pub struct GenericPayloadEntry {
    pub timestamp: SystemTime,
    pub name: String,
    #[serde(serialize_with = "cbor_encode_in_list")]
    pub payload: Signed<GenericPriceFeed>,
}

#[derive(Serialize, Clone)]
pub struct Payload {
    pub publisher: NodeId,
    pub timestamp: SystemTime,
    pub generics: Vec<GenericPayloadEntry>,
    pub synthetics: Vec<SyntheticPayloadEntry>,
}
