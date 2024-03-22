use std::{env, time::Duration};

use anyhow::Result;
use minicbor::Encoder;
use pallas_crypto::key::ed25519::SecretKey;
use pallas_primitives::conway::PlutusData;
use rust_decimal::prelude::ToPrimitive;
use serde::Serialize;
use tokio::{
    sync::{mpsc::Sender, watch::Receiver},
    time::sleep,
};
use tracing::warn;

use crate::{
    price_feed::{PriceFeedEntry, SignedPriceFeed},
    raft::RaftLeader,
};

pub mod new;

#[derive(Clone)]
pub struct SingleSignatureAggregator {
    key: SecretKey,
    price_feed: Receiver<Vec<PriceFeedEntry>>,
    leader_feed: Receiver<RaftLeader>,
    payload_sink: Sender<String>,
}

impl SingleSignatureAggregator {
    pub fn new(
        price_rx: Receiver<Vec<PriceFeedEntry>>,
        leader_rx: Receiver<RaftLeader>,
        tx: Sender<String>,
    ) -> Result<Self> {
        Ok(Self {
            key: decode_key()?,
            price_feed: price_rx,
            leader_feed: leader_rx,
            payload_sink: tx,
        })
    }

    pub async fn run(&mut self) {
        loop {
            sleep(Duration::from_secs(5)).await;
            if !matches!(*self.leader_feed.borrow(), RaftLeader::Myself) {
                continue;
            }

            let price_feed: Vec<PriceFeedEntry> = {
                let price_feed_ref = self.price_feed.borrow_and_update();
                price_feed_ref
                    .iter()
                    .cloned()
                    .collect::<Vec<PriceFeedEntry>>()
            };

            let payload = match self.serialize_payload(price_feed) {
                Ok(str) => str,
                Err(error) => {
                    warn!("Could not serialize payload: {}", error);
                    continue;
                }
            };
            if let Err(error) = self.payload_sink.send(payload).await {
                warn!("Could not send payload: {}", error);
            }
        }
    }

    fn serialize_payload(&self, data: Vec<PriceFeedEntry>) -> Result<String> {
        let mut results = vec![];
        for datum in data {
            let synthetic = datum.data.synthetic.clone();
            let price_feed: PlutusData = (&datum.data).into();
            let price_feed_bytes = {
                let mut encoder = Encoder::new(vec![]);
                encoder.encode(&price_feed)?;
                encoder.into_writer()
            };
            let signature = self.key.sign(&price_feed_bytes);
            let signed_price_feed = SignedPriceFeed {
                data: price_feed,
                signature: signature.as_ref().to_vec(),
            };
            let payload = {
                let data = PlutusData::Array(vec![(&signed_price_feed).into()]);
                let mut encoder = Encoder::new(vec![]);
                encoder.encode(data)?;
                encoder.into_writer()
            };

            let top_level_payload = TopLevelPayload {
                synthetic,
                price: datum.price.to_f64().expect("Count not convert decimal"),
                payload: hex::encode(payload),
            };
            results.push(top_level_payload);
        }
        Ok(serde_json::to_string(&results)?)
    }
}

fn decode_key() -> Result<SecretKey> {
    let raw_key = env::var("ORACLE_KEY")?;
    let (hrp, key) = bech32::decode(&raw_key)?;
    assert_eq!(hrp.as_str(), "ed25519_sk");
    let value: [u8; 32] = key.try_into().expect("Key was wrong size");
    Ok(value.into())
}

#[derive(Serialize)]
struct TopLevelPayload {
    pub synthetic: String,
    pub price: f64,
    pub payload: String,
}
