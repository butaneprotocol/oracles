use std::str::FromStr;

use anyhow::{anyhow, Result};
use futures::StreamExt;
use rust_decimal::Decimal;
use serde::Deserialize;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{trace, warn};

use crate::{
    apis::source::{Origin, PriceInfo, PriceSink},
    token::Token,
};

// TODO: currencies shouldn't be hard-coded
const URL: &str = "wss://fstream.binance.com/stream?streams=btcusdt@markPrice/adausdt@markPrice";

pub struct BinanceSource;

impl BinanceSource {
    pub fn new() -> Self {
        Self
    }

    pub async fn query(self, sink: PriceSink) {
        loop {
            let (mut stream, _) = connect_async(URL).await.unwrap();
            trace!("Connected to binance!");
            while let Some(res) = stream.next().await {
                match res {
                    Ok(Message::Text(contents)) => {
                        if let Err(err) = process_binance_message(contents, &sink) {
                            warn!("Unexpected error updating binance data: {:?}", err);
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

    let currency = &message.stream[0..3];
    let price = &message.data.price;

    sink.unbounded_send(PriceInfo {
        origin: Origin::Binance,
        token: Token::value_of(currency)
            .ok_or_else(|| anyhow!("Unrecognized currency {}", currency))?,
        value: Decimal::from_str(price)?,
        relative_to: Token::USDT,
    })?;

    Ok(())
}
