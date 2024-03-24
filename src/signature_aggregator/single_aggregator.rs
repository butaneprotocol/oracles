use std::{env, time::Duration};

use anyhow::Result;
use minicbor::Encoder;
use pallas_crypto::key::ed25519::SecretKey;
use pallas_primitives::conway::PlutusData;
use tokio::{
    sync::{mpsc::Sender, watch::Receiver},
    time::sleep,
};
use tracing::warn;

use crate::{
    price_feed::{PriceFeedEntry, SignedPriceFeed, SignedPriceFeedEntry},
    raft::RaftLeader,
};

#[derive(Clone)]
pub struct SingleSignatureAggregator {
    key: SecretKey,
    price_source: Receiver<Vec<PriceFeedEntry>>,
    leader_source: Receiver<RaftLeader>,
    signed_price_sink: Sender<Vec<SignedPriceFeedEntry>>,
}

impl SingleSignatureAggregator {
    pub fn new(
        price_source: Receiver<Vec<PriceFeedEntry>>,
        leader_source: Receiver<RaftLeader>,
        signed_price_sink: Sender<Vec<SignedPriceFeedEntry>>,
    ) -> Result<Self> {
        Ok(Self {
            key: decode_key()?,
            price_source,
            leader_source,
            signed_price_sink,
        })
    }

    pub async fn run(&mut self) {
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

            let signed_prices = prices
                .into_iter()
                .map(|p| self.sign_price_feed(p))
                .collect();

            if let Err(error) = self.signed_price_sink.send(signed_prices).await {
                warn!("Could not send signed prices: {}", error);
            }
        }
    }

    fn sign_price_feed(&self, data: PriceFeedEntry) -> SignedPriceFeedEntry {
        let price_feed: PlutusData = (&data.data).into();
        let price_feed_bytes = {
            let mut encoder = Encoder::new(vec![]);
            encoder.encode(&price_feed).expect("encoding is infallible");
            encoder.into_writer()
        };
        let signature = self.key.sign(price_feed_bytes);
        SignedPriceFeedEntry {
            price: data.price,
            data: SignedPriceFeed {
                data: data.data,
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
