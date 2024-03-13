use std::str::FromStr;

use anyhow::{anyhow, Result};
use futures::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{trace, warn};

use super::source::{Origin, PriceInfo, PriceSink};

const URL: &str = "wss://ws-feed.exchange.coinbase.com";

#[derive(Default)]
pub struct CoinbaseSource;

impl CoinbaseSource {
    pub fn new() -> Self {
        Self
    }

    pub async fn query(&self, sink: PriceSink) {
        loop {
            if let Err(error) = self.run_query(sink.clone()).await {
                warn!("Error querying coinbase, retrying: {}", error);
            }
        }
    }

    async fn run_query(&self, mut sink: PriceSink) -> Result<()> {
        let (mut stream, _) = connect_async(URL).await?;

        trace!("Connecting to coinbase");
        let request = CoinbaseRequest::Subscribe {
            product_ids: vec![
                "ADA-USD".into(),
                "BTC-USD".into(),
                "MATIC-USD".into(),
                "SOL-USD".into(),
            ],
            channels: vec!["ticker".into()],
        };
        stream.send(request.try_into()?).await?;

        while let Some(result) = stream.next().await {
            // on stream error, just try reconnecting
            let message =
                result.map_err(|e| anyhow!("Websocket error querying coinbase: {}", e))?;

            let price_info = match self.parse_message(message) {
                Ok(pi) => pi,
                Err(err) => {
                    warn!("{}", err);
                    continue;
                }
            };

            sink.send(price_info).await?;
        }

        Ok(())
    }

    fn parse_message(&self, message: Message) -> Result<PriceInfo> {
        let response: CoinbaseResponse = message.clone().try_into()?;
        let CoinbaseResponse::Ticker { product_id, price } = response else {
            return Err(anyhow!("Unexpected response from coinbase: {:?}", response));
        };
        let mut value = Decimal::from_str(&price)?;
        let (token, relative_to) = match product_id.as_str() {
            "ADA-USD" => ("ADA", "USD"),
            "BTC-USD" => ("BTCb", "USD"),
            "MATIC-USD" => ("MATICb", "USD"),
            "SOL-USD" => {
                value = Decimal::ONE / value;
                ("SOLp", "USD")
            }
            _ => {
                return Err(anyhow!("Unrecognized price to match: {}", product_id));
            }
        };
        Ok(PriceInfo {
            origin: Origin::Coinbase,
            token: token.into(),
            value,
            relative_to: relative_to.into(),
        })
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum CoinbaseRequest {
    Subscribe {
        product_ids: Vec<String>,
        channels: Vec<String>,
    },
}

impl TryFrom<CoinbaseRequest> for Message {
    type Error = anyhow::Error;

    fn try_from(value: CoinbaseRequest) -> Result<Self> {
        Ok(Self::text(serde_json::to_string(&value)?))
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum CoinbaseResponse {
    #[allow(unused)]
    Error {
        message: String,
    },
    Ticker {
        product_id: String,
        price: String,
    },
}

impl TryFrom<Message> for CoinbaseResponse {
    type Error = anyhow::Error;

    fn try_from(value: Message) -> Result<Self> {
        match value {
            Message::Text(msg) => Ok(serde_json::from_str(&msg)?),
            x => Err(anyhow::anyhow!("Unexpected response: {:?}", x)),
        }
    }
}
