use std::{
    env,
    time::{Duration, SystemTime},
};

use anyhow::Result;
use pallas_crypto::key::ed25519::SecretKey;
use rust_decimal::prelude::ToPrimitive;
use tokio::{
    sync::{mpsc, watch},
    time::sleep,
};
use tracing::warn;

use crate::{
    config::OracleConfig,
    network::NodeId,
    price_feed::{serialize, PriceData, Signed, SignedEntries, SyntheticEntry, SyntheticPriceData},
    raft::RaftLeader,
};

#[derive(Clone)]
pub struct SingleSignatureAggregator {
    id: NodeId,
    key: SecretKey,
    price_source: watch::Receiver<PriceData>,
    leader_source: watch::Receiver<RaftLeader>,
    signed_entries_sink: mpsc::Sender<(NodeId, SignedEntries)>,
    round_duration: Duration,
}

impl SingleSignatureAggregator {
    pub fn new(
        config: &OracleConfig,
        price_source: watch::Receiver<PriceData>,
        leader_source: watch::Receiver<RaftLeader>,
        signed_entries_sink: mpsc::Sender<(NodeId, SignedEntries)>,
    ) -> Result<Self> {
        Ok(Self {
            id: config.id.clone(),
            key: decode_key()?,
            price_source,
            leader_source,
            signed_entries_sink,
            round_duration: config.round_duration,
        })
    }

    pub async fn run(mut self) {
        loop {
            sleep(self.round_duration).await;
            if !matches!(*self.leader_source.borrow(), RaftLeader::Myself) {
                continue;
            }

            let prices = {
                let price_feed_ref = self.price_source.borrow_and_update();
                price_feed_ref.synthetics.clone()
            };

            let now = SystemTime::now();
            let payload_entries = prices
                .into_iter()
                .map(|p| self.sign_price_feed(p, now))
                .collect();
            let payload = SignedEntries {
                timestamp: SystemTime::now(),
                synthetics: payload_entries,
                generics: vec![], // TODO
            };

            if let Err(error) = self
                .signed_entries_sink
                .send((self.id.clone(), payload))
                .await
            {
                warn!("Could not send signed prices: {}", error);
            }
        }
    }

    fn sign_price_feed(&self, data: SyntheticPriceData, timestamp: SystemTime) -> SyntheticEntry {
        let price_feed_bytes = serialize(&data.feed);
        let signature = self.key.sign(price_feed_bytes);
        SyntheticEntry {
            price: data.price.to_f64().expect("Could not convert decimal"),
            feed: Signed {
                data: data.feed.clone(),
                signature: signature.as_ref().to_vec(),
            },
            timestamp: Some(timestamp),
        }
    }
}

fn decode_key() -> Result<SecretKey> {
    let raw_key = env::var("ORACLE_KEY")?;
    let (hrp, key) = bech32::decode(&raw_key)?;
    assert_eq!(hrp.as_str(), "ed25519_sk");
    let value: [u8; 32] = key.try_into().expect("Key was wrong size");
    Ok(value.into())
}
