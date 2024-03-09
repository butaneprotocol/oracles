use std::{env, time::Duration};

use anyhow::Result;
use minicbor::Encoder;
use num_bigint::BigUint;
use pallas_crypto::key::ed25519::SecretKey;
use pallas_primitives::conway::{BigInt, Constr, PlutusData};
use rust_decimal::prelude::ToPrimitive;
use serde::Serialize;
use tokio::{
    sync::{mpsc::Sender, watch::Receiver},
    time::sleep,
};
use tracing::warn;

use crate::price_aggregator::PriceFeed;

const CBOR_VARIANT_0: u64 = 121;

#[derive(Clone)]
pub struct SingleSignatureAggregator {
    key: SecretKey,
    price_feed: Receiver<Vec<PriceFeed>>,
    leader_feed: Receiver<bool>,
    payload_sink: Sender<String>,
}

impl SingleSignatureAggregator {
    pub fn new(
        price_rx: Receiver<Vec<PriceFeed>>,
        leader_rx: Receiver<bool>,
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
            if !*self.leader_feed.borrow() {
                continue;
            }

            let price_feed: Vec<PriceFeed> = {
                let price_feed_ref = self.price_feed.borrow_and_update();
                price_feed_ref.iter().cloned().collect::<Vec<PriceFeed>>()
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

    fn serialize_payload(&self, data: Vec<PriceFeed>) -> Result<String> {
        let mut results = vec![];
        for datum in data {
            let price_feed = encode_struct(vec![
                // collateral_prices
                PlutusData::Array(datum.collateral_prices.iter().map(encode_bigint).collect()),
                // synthetic
                PlutusData::BoundedBytes(datum.synthetic.as_bytes().to_vec().into()),
                // denominator
                encode_bigint(&datum.denominator),
                // validity
                Validity {
                    lower_bound: IntervalBound {
                        bound_type: IntervalBoundType::NegativeInfinity,
                        is_inclusive: false,
                    },
                    upper_bound: IntervalBound {
                        bound_type: IntervalBoundType::PositiveInfinity,
                        is_inclusive: false,
                    },
                }
                .into(),
            ]);
            let price_feed_bytes = {
                let mut encoder = Encoder::new(vec![]);
                encoder.encode(&price_feed)?;
                encoder.into_writer()
            };
            let signature = self.key.sign(&price_feed_bytes);
            let payload = {
                let data = PlutusData::Array(vec![encode_struct(vec![
                    price_feed,
                    PlutusData::BoundedBytes(signature.as_ref().to_vec().into()),
                ])]);
                let mut encoder = Encoder::new(vec![]);
                encoder.encode(data)?;
                encoder.into_writer()
            };

            let top_level_payload = TopLevelPayload {
                synthetic: datum.synthetic,
                price: datum.price.to_f64().expect("Count not convert decimal"),
                payload: hex::encode(payload),
            };
            results.push(top_level_payload);
        }
        Ok(serde_json::to_string(&results)?)
    }
}

fn encode_bigint(value: &BigUint) -> PlutusData {
    PlutusData::BigInt(match value.to_i64() {
        Some(val) => BigInt::Int(val.into()),
        None => BigInt::BigUInt(value.to_bytes_be().into()),
    })
}

fn encode_struct(fields: Vec<PlutusData>) -> PlutusData {
    PlutusData::Constr(Constr {
        tag: CBOR_VARIANT_0,
        any_constructor: None,
        fields,
    })
}

fn encode_enum(variant: u64, value: Option<PlutusData>) -> PlutusData {
    PlutusData::Constr(Constr {
        tag: CBOR_VARIANT_0 + variant,
        any_constructor: None,
        fields: if let Some(val) = value {
            vec![val]
        } else {
            vec![]
        },
    })
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

#[derive(Clone, Copy)]
struct Validity {
    lower_bound: IntervalBound,
    upper_bound: IntervalBound,
}
impl From<Validity> for PlutusData {
    fn from(val: Validity) -> Self {
        encode_struct(vec![val.lower_bound.into(), val.upper_bound.into()])
    }
}

#[derive(Clone, Copy)]
struct IntervalBound {
    bound_type: IntervalBoundType,
    is_inclusive: bool,
}
impl From<IntervalBound> for PlutusData {
    fn from(val: IntervalBound) -> Self {
        encode_struct(vec![
            val.bound_type.into(),
            encode_enum(if val.is_inclusive { 1 } else { 0 }, None),
        ])
    }
}

#[derive(Clone, Copy)]
enum IntervalBoundType {
    NegativeInfinity,
    #[allow(unused)]
    Finite(u64),
    PositiveInfinity,
}
impl From<IntervalBoundType> for PlutusData {
    fn from(val: IntervalBoundType) -> Self {
        match val {
            IntervalBoundType::NegativeInfinity => encode_enum(0, None),
            IntervalBoundType::Finite(val) => encode_enum(1, Some(encode_bigint(&val.into()))),
            IntervalBoundType::PositiveInfinity => encode_enum(2, None),
        }
    }
}
