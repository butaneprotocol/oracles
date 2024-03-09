use std::{env, time::Duration};

use anyhow::Result;
use minicbor::{
    data::{Int, Tag},
    encode, Encode, Encoder,
};
use pallas_crypto::key::ed25519::SecretKey;
use rust_decimal::prelude::ToPrimitive;
use serde::Serialize;
use tokio::{
    sync::{mpsc::Sender, watch::Receiver},
    time::sleep,
};

use crate::price_aggregator::PriceFeed;

const CBOR_VARIANT_0: u64 = 121;

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
        let variant_0: Tag = Tag::new(CBOR_VARIANT_0);

        let mut results = vec![];
        for datum in data {
            let price_feed_bytes = {
                let mut encoder = Encoder::new(vec![]);

                // start the PriceFeed array
                encoder.tag(variant_0)?.begin_array()?;

                encoder.begin_array()?;
                for collateral in datum.collateral_prices {
                    encoder.int(collateral.into())?;
                }
                encoder.end()?;

                encoder.bytes(datum.synthetic.as_bytes())?;

                encoder.int(datum.denominator.into())?;

                encoder.encode(Validity {
                    lower_bound: IntervalBound {
                        bound_type: IntervalBoundType::NegativeInfinity,
                        is_inclusive: false,
                    },
                    upper_bound: IntervalBound {
                        bound_type: IntervalBoundType::PositiveInfinity,
                        is_inclusive: true,
                    },
                })?;

                // end the PriceFeed array
                encoder.end()?;

                encoder.into_writer()
            };
            let signature = self.key.sign(&price_feed_bytes);
            let payload = {
                let mut encoder = Encoder::new(vec![]);

                // begin payload
                encoder.begin_array()?;

                // begin feed
                encoder.tag(variant_0)?.begin_array()?;
                // write PriceFeed
                encoder.writer_mut().extend_from_slice(&price_feed_bytes);
                // write signature
                encoder.bytes(signature.as_ref())?;
                // end feed
                encoder.end()?;

                // end payload
                encoder.end()?;

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

struct Validity {
    lower_bound: IntervalBound,
    upper_bound: IntervalBound,
}
impl<C> Encode<C> for Validity {
    fn encode<W: encode::Write>(
        &self,
        e: &mut Encoder<W>,
        _ctx: &mut C,
    ) -> Result<(), encode::Error<W::Error>> {
        e.tag(Tag::new(CBOR_VARIANT_0))?.begin_array()?;
        e.encode(&self.lower_bound)?;
        e.encode(&self.upper_bound)?;
        e.end()?;
        Ok(())
    }
}

struct IntervalBound {
    bound_type: IntervalBoundType,
    is_inclusive: bool,
}
impl<C> Encode<C> for IntervalBound {
    fn encode<W: encode::Write>(
        &self,
        e: &mut Encoder<W>,
        _ctx: &mut C,
    ) -> Result<(), encode::Error<W::Error>> {
        e.tag(Tag::new(CBOR_VARIANT_0))?.begin_array()?;
        e.encode(&self.bound_type)?;
        cbor_variant(e, if self.is_inclusive { 1 } else { 0 }, None)?;
        e.end()?;
        Ok(())
    }
}

enum IntervalBoundType {
    NegativeInfinity,
    #[allow(unused)]
    Finite(u64),
    PositiveInfinity,
}
impl<C> Encode<C> for IntervalBoundType {
    fn encode<W: encode::Write>(
        &self,
        e: &mut Encoder<W>,
        _ctx: &mut C,
    ) -> Result<(), encode::Error<W::Error>> {
        match &self {
            Self::NegativeInfinity => cbor_variant(e, 0, None),
            Self::Finite(val) => cbor_variant(e, 1, Some((*val).into())),
            Self::PositiveInfinity => cbor_variant(e, 2, None),
        }
    }
}

fn cbor_variant<W: encode::Write>(
    e: &mut Encoder<W>,
    variant: u64,
    value: Option<Int>,
) -> Result<(), encode::Error<W::Error>> {
    e.tag(Tag::new(CBOR_VARIANT_0 + variant))?;
    match value {
        Some(int) => {
            e.begin_array()?.int(int)?.end()?;
        }
        None => {
            e.array(0)?;
        }
    }
    Ok(())
}
