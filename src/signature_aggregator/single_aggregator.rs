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
    network::NodeId,
    price_feed::{serialize, Payload, PayloadEntry, PriceFeedEntry, SignedPriceFeed},
    raft::RaftLeader,
};

#[derive(Clone)]
pub struct SingleSignatureAggregator {
    id: NodeId,
    key: SecretKey,
    price_source: watch::Receiver<Vec<PriceFeedEntry>>,
    leader_source: watch::Receiver<RaftLeader>,
    payload_sink: mpsc::Sender<(NodeId, Payload)>,
}

impl SingleSignatureAggregator {
    pub fn new(
        id: &NodeId,
        price_source: watch::Receiver<Vec<PriceFeedEntry>>,
        leader_source: watch::Receiver<RaftLeader>,
        payload_sink: mpsc::Sender<(NodeId, Payload)>,
    ) -> Result<Self> {
        Ok(Self {
            id: id.clone(),
            key: decode_key()?,
            price_source,
            leader_source,
            payload_sink,
        })
    }

    pub async fn run(mut self) {
        loop {
            sleep(Duration::from_secs(5)).await;
            if !matches!(*self.leader_source.borrow(), RaftLeader::Myself) {
                continue;
            }

            let prices: Vec<PriceFeedEntry> = {
                let price_feed_ref = self.price_source.borrow_and_update();
                price_feed_ref
                    .iter()
                    .cloned()
                    .collect::<Vec<PriceFeedEntry>>()
            };

            let payload_entries = prices
                .into_iter()
                .map(|p| self.sign_price_feed(p))
                .collect();
            let payload = Payload {
                timestamp: SystemTime::now(),
                entries: payload_entries,
            };

            if let Err(error) = self.payload_sink.send((self.id.clone(), payload)).await {
                warn!("Could not send signed prices: {}", error);
            }
        }
    }

    fn sign_price_feed(&self, data: PriceFeedEntry) -> PayloadEntry {
        let price_feed_bytes = serialize(&data.data);
        let signature = self.key.sign(price_feed_bytes);
        PayloadEntry {
            price: data.price.to_f64().expect("Could not convert decimal"),
            data: SignedPriceFeed {
                data: data.data.clone(),
                signature: signature.as_ref().to_vec(),
            },
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
