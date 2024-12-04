use std::{collections::BTreeMap, str::FromStr, time::Duration};

use anyhow::{anyhow, Result};
use futures::{future::BoxFuture, FutureExt, SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde::Deserialize;
use tokio::time::timeout;
use tokio_websockets::{ClientBuilder, Message};
use tracing::{trace, warn};

use crate::{
    config::{BinanceTokenConfig, OracleConfig},
    sources::source::{PriceInfo, PriceSink},
};

use super::source::Source;

const BASE_URL: &str = "wss://fstream.binance.com/stream";

pub struct BinanceSource {
    streams: BTreeMap<String, BinanceTokenConfig>,
}

impl Source for BinanceSource {
    fn name(&self) -> String {
        "Binance".into()
    }

    fn tokens(&self) -> Vec<String> {
        self.streams.values().map(|c| c.token.clone()).collect()
    }

    fn query<'a>(&'a self, sink: &'a PriceSink) -> BoxFuture<'a, Result<()>> {
        self.query_impl(sink).boxed()
    }
}

impl BinanceSource {
    pub fn new(config: &OracleConfig) -> Self {
        let streams = config
            .binance
            .tokens
            .iter()
            .map(|t| (t.stream.clone(), t.clone()))
            .collect();
        Self { streams }
    }

    async fn query_impl(&self, sink: &PriceSink) -> Result<()> {
        let streams = self
            .streams
            .keys()
            .map(|k| k.as_str())
            .collect::<Vec<&str>>()
            .join("/");
        let url = format!("{BASE_URL}?streams={streams}");
        let uri = url.try_into()?;
        let (mut stream, _) = ClientBuilder::from_uri(uri).connect().await?;
        trace!("Connected to binance!");

        let connection_timeout = Duration::from_secs(60);
        while let Ok(Some(res)) = timeout(connection_timeout, stream.next()).await {
            let message = match res {
                Ok(msg) => msg,
                Err(error) => {
                    warn!("Error from binance: {:?}", error);
                    continue;
                }
            };
            if let Some(contents) = message.as_text() {
                if let Err(err) = self.process_binance_message(contents, sink) {
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

    fn process_binance_message(&self, contents: &str, sink: &PriceSink) -> Result<()> {
        let message: BinanceMarkPriceMessage = serde_json::from_str(contents)?;
        let Some(stream) = self.streams.get(&message.stream) else {
            return Err(anyhow!("Unrecognized currency {}", message.stream));
        };
        let token = &stream.token;
        let value = Decimal::from_str(&message.data.price)?;
        let volume = Decimal::from_str(&message.data.volume_base)?;

        sink.send(PriceInfo {
            token: token.to_string(),
            unit: stream.unit.to_string(),
            value,
            reliability: volume,
        })?;

        Ok(())
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
