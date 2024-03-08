use std::{env, time::Duration};

use anyhow::Result;
use pallas_crypto::key::ed25519::SecretKey;
use rust_decimal::Decimal;
use serde::Serialize;
use tokio::{sync::{mpsc::Sender, watch::Receiver}, time::sleep};

use crate::price_aggregator::PriceFeed;

#[derive(Clone)]
pub struct SingleSignatureAggregator {
    key: SecretKey,
    price_feed: Receiver<Vec<PriceFeed>>,
    payload_sink: Sender<String>,
}

impl SingleSignatureAggregator {
    pub fn new(rx: Receiver<Vec<PriceFeed>>, tx: Sender<String>) -> Result<Self> {
        Ok(Self {
            key: decode_key()?,
            price_feed: rx,
            payload_sink: tx,
        })
    }

    pub async fn run(&mut self) {
        println!("{:x?}", self.key.public_key());
        loop {
            sleep(Duration::from_secs(5)).await;
            let price_feed: Vec<PriceFeed> = {
                let price_feed_ref = self.price_feed.borrow_and_update();
                price_feed_ref.iter().cloned().collect::<Vec<PriceFeed>>()
            };

            let payload = self.serialize_payload(price_feed).expect("should work");

            println!("{}", payload)
        }
    }

    fn serialize_payload(&self, data: Vec<PriceFeed>) -> Result<String> {
        let mut results = vec![];
        for datum in data {
            let price_feed = PriceFeedPayload {
                collateral_prices: datum.collateral_prices,
                synthetic: datum.synthetic.clone(),
                denominator: datum.denominator,
                validity: Validity {
                    lower_bound: IntervalBound {
                        bound_type: IntervalBoundType::NegativeInfinity,
                        is_inclusive: false
                    },
                    upper_bound: IntervalBound {
                        bound_type: IntervalBoundType::PositiveInfinity,
                        is_inclusive: false
                    }
                },
            };
            let serialized_price_feed = serde_cbor::to_vec(&price_feed)?;
            let signature = self.key.sign(serialized_price_feed);
            let signature_bytes = signature.as_ref();
            let payload = serde_cbor::to_vec(&(price_feed, signature_bytes))?;
            let payload = hex::encode(payload);
    
            //let float_price: f64 = datum.price.into();
            let top_level_payload = TopLevelPayload {
                synthetic: datum.synthetic,
                price: datum.price,
                payload
            };
            results.push(top_level_payload);
        };

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
    pub price: Decimal,
    pub payload: String,
}

#[derive(Serialize)]
struct PriceFeedPayload {
    pub collateral_prices: Vec<u64>,
    pub synthetic: String,
    pub denominator: u64,
    pub validity: Validity,
}

#[derive(Serialize)]
struct Validity {
    lower_bound: IntervalBound,
    upper_bound: IntervalBound,
}

#[derive(Serialize)]
struct IntervalBound {
    bound_type: IntervalBoundType,
    is_inclusive: bool,
}

#[derive(Serialize)]
#[allow(unused)]
enum IntervalBoundType {
    NegativeInfinity,
    Finite(u64),
    PositiveInfinity,
}