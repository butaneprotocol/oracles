use std::str::FromStr;

use anyhow::{anyhow, Result};
use futures::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde::Deserialize;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{trace, warn};

use crate::apis::source::{Origin, PriceInfo, PriceSink};

// TODO: currencies shouldn't be hard-coded?
const URL: &str = "wss://fstream.binance.com/stream?streams=btcusdt@markPrice/adausdt@markPrice/solusdt@markPrice/maticusdt@markPrice";

#[derive(Default)]
pub struct BinanceSource;

impl BinanceSource {
    pub fn new() -> Self {
        Self
    }

    pub async fn query(self, sink: PriceSink) {
        loop {
            let Ok((mut stream, _)) = connect_async(URL).await else {
                warn!("Could not connect to binance, retrying...");
                continue;
            };
            trace!("Connected to binance!");
            while let Some(res) = stream.next().await {
                match res {
                    Ok(Message::Text(contents)) => {
                        if let Err(err) = process_binance_message(contents, &sink) {
                            warn!("Unexpected error updating binance data: {:?}", err);
                        }
                    }
                    Ok(Message::Ping(data)) => {
                        trace!("Ping received from binance: {:?}", data);
                        if let Err(err) = stream.send(Message::Pong(data)).await {
                            warn!("Unexpected error replying to binance ping: {}", err);
                        }
                    }
                    Ok(message) => {
                        warn!("Unexpected response from binance: {:?}", message);
                    }
                    Err(error) => {
                        warn!("Error from binance: {:?}", error);
                    }
                }
            }
            trace!("Connection to binance closed. Reconnecting...")
        }
    }
}

#[derive(Deserialize)]
struct BinanceMarkPriceMessage {
    stream: String,
    data: BinanceMarkPriceMessageData,
}
#[derive(Deserialize)]
struct BinanceMarkPriceMessageData {
    #[serde(rename(deserialize = "p"))]
    price: String,
}

fn process_binance_message(contents: String, sink: &PriceSink) -> Result<()> {
    let message: BinanceMarkPriceMessage = serde_json::from_str(&contents)?;

    let currency = match message.stream.find("usdt") {
        Some(index) => &message.stream[0..index],
        None => return Err(anyhow!("Malformed stream {}", message.stream)),
    };
    let mut value = Decimal::from_str(&message.data.price)?;
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

    sink.unbounded_send(PriceInfo {
        origin: Origin::Binance,
        token: token.to_string(),
        value,
        relative_to: "USD".to_string(),
    })?;

    Ok(())
}
