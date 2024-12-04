use std::{collections::BTreeMap, str::FromStr, time::Duration};

use anyhow::{anyhow, Result};
use futures::{future::BoxFuture, FutureExt, SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use tokio::time::timeout;
use tokio_websockets::{ClientBuilder, Message};
use tracing::{trace, warn};

use crate::config::{CoinbaseTokenConfig, OracleConfig};

use super::source::{PriceInfo, PriceSink, Source};

const URL: &str = "wss://ws-feed.exchange.coinbase.com";

pub struct CoinbaseSource {
    products: BTreeMap<String, CoinbaseTokenConfig>,
}

impl Source for CoinbaseSource {
    fn name(&self) -> String {
        "Coinbase".into()
    }

    fn tokens(&self) -> Vec<String> {
        self.products.values().map(|p| p.token.clone()).collect()
    }

    fn query<'a>(&'a self, sink: &'a PriceSink) -> BoxFuture<'a, Result<()>> {
        self.query_impl(sink).boxed()
    }
}

impl CoinbaseSource {
    pub fn new(config: &OracleConfig) -> Self {
        let products = config
            .coinbase
            .tokens
            .iter()
            .map(|t| (t.product_id.clone(), t.clone()))
            .collect();
        Self { products }
    }

    async fn query_impl(&self, sink: &PriceSink) -> Result<()> {
        trace!("Connecting to coinbase");
        let connection_timeout = Duration::from_secs(60);
        let uri = URL.try_into()?;

        let (mut stream, _) =
            timeout(connection_timeout, ClientBuilder::from_uri(uri).connect()).await??;

        let request = CoinbaseRequest::Subscribe {
            product_ids: self.products.keys().cloned().collect(),
            channels: vec!["ticker".into()],
        };
        timeout(connection_timeout, stream.send(request.try_into()?)).await??;

        let Some(first_result) = timeout(connection_timeout, stream.next()).await? else {
            return Err(anyhow!("Channel closed without sending a response"));
        };
        let CoinbaseResponse::Subscriptions {} = first_result?.try_into()? else {
            return Err(anyhow!("Did not receive expected first response"));
        };

        while let Ok(Some(result)) = timeout(connection_timeout, stream.next()).await {
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

            sink.send(price_info)?;
        }

        Err(anyhow!("Connection to coinbase closed"))
    }

    fn parse_message(&self, message: Message) -> Result<PriceInfo> {
        let response: CoinbaseResponse = message.clone().try_into()?;
        let CoinbaseResponse::Ticker {
            product_id,
            price,
            volume_24h,
        } = response
        else {
            return Err(anyhow!("Unexpected response from coinbase: {:?}", response));
        };
        let Some(product) = self.products.get(&product_id) else {
            return Err(anyhow!("Unrecognized price to match: {}", product_id));
        };
        let value = Decimal::from_str(&price)?;
        let volume = Decimal::from_str(&volume_24h)?;
        Ok(PriceInfo {
            token: product.token.clone(),
            unit: product.unit.clone(),
            value,
            reliability: volume,
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
    Subscriptions {},
    Ticker {
        product_id: String,
        price: String,
        volume_24h: String,
    },
}

impl TryFrom<Message> for CoinbaseResponse {
    type Error = anyhow::Error;

    fn try_from(value: Message) -> Result<Self> {
        match value.as_text() {
            Some(msg) => Ok(serde_json::from_str(msg)?),
            None => Err(anyhow::anyhow!("Unexpected response: {:?}", value)),
        }
    }
}
