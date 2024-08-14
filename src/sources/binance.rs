use std::str::FromStr;

use anyhow::{anyhow, Result};
use futures::{future::BoxFuture, FutureExt, SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde::Deserialize;
use tokio_websockets::{ClientBuilder, Message};
use tracing::{trace, warn};

use crate::sources::source::{PriceInfo, PriceSink};

use super::source::Source;

// TODO: currencies shouldn't be hard-coded?
const URL: &str = "wss://fstream.binance.com/stream?streams=btcusdt@ticker/adausdt@ticker/solusdt@ticker/maticusdt@ticker";

#[derive(Default)]
pub struct BinanceSource;

impl Source for BinanceSource {
    fn name(&self) -> String {
        "Binance".into()
    }

    fn tokens(&self) -> Vec<String> {
        vec!["ADA".into(), "BTCb".into(), "SOLp".into(), "MATICb".into()]
    }

    fn query<'a>(&'a self, sink: &'a PriceSink) -> BoxFuture<Result<()>> {
        self.query_impl(sink).boxed()
    }
}

impl BinanceSource {
    pub fn new() -> Self {
        Self
    }

    async fn query_impl(&self, sink: &PriceSink) -> Result<()> {
        let uri = URL.try_into()?;
        let (mut stream, _) = ClientBuilder::from_uri(uri).connect().await?;
        trace!("Connected to binance!");

        while let Some(res) = stream.next().await {
            let message = match res {
                Ok(msg) => msg,
                Err(error) => {
                    warn!("Error from binance: {:?}", error);
                    continue;
                }
            };
            if let Some(contents) = message.as_text() {
                if let Err(err) = process_binance_message(contents, sink) {
                    warn!("Unexpected error updating binance data: {:?}", err);
                }
            } else if message.is_ping() {
                let data = message.into_payload();
                trace!("Ping received from binance: {:?}", data);
                if let Err(err) = stream.send(Message::pong(data)).await {
                    warn!("Unexpected error replying to binance ping: {}", err);
                }
            } else {
                warn!("Unexpected response from binance: {:?}", message);
            }
        }

        Err(anyhow!("Connection to binance closed"))
    }
}

#[derive(Deserialize)]
struct BinanceMarkPriceMessage {
    stream: String,
    data: BinanceMarkPriceMessageData,
}
#[derive(Deserialize)]
struct BinanceMarkPriceMessageData {
    #[serde(rename(deserialize = "c"))]
    price: String,
    #[serde(rename(deserialize = "v"))]
    volume_base: String,
}

fn process_binance_message(contents: &str, sink: &PriceSink) -> Result<()> {
    let message: BinanceMarkPriceMessage = serde_json::from_str(contents)?;

    let currency = match message.stream.find("usdt") {
        Some(index) => &message.stream[0..index],
        None => return Err(anyhow!("Malformed stream {}", message.stream)),
    };
    let mut value = Decimal::from_str(&message.data.price)?;
    if value.is_zero() {
        warn!("Binance reported value of {} as zero, ignoring", currency);
        return Ok(());
    }
    let token = match currency {
        "btc" => "BTCb",
        "ada" => "ADA",
        "sol" => {
            value = Decimal::ONE / value;
            "SOLp"
        }
        "matic" => "MATICb",
        _ => return Err(anyhow!("Unrecognized currency {}", message.stream)),
    };
    let volume = Decimal::from_str(&message.data.volume_base)?;

    sink.send(PriceInfo {
        token: token.to_string(),
        unit: "USD".to_string(),
        value,
        reliability: volume,
    })?;

    Ok(())
}
