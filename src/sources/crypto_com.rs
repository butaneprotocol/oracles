use std::{collections::HashMap, str::FromStr as _, time::Duration};

use crate::{
    config::{CryptoComTokenConfig, OracleConfig},
    sources::source::PriceInfo,
};
use anyhow::{bail, Context, Result};
use futures::{future::BoxFuture, FutureExt, SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use tokio::time::timeout;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::warn;

use super::source::{PriceSink, Source};

const URL: &str = "wss://stream.crypto.com/exchange/v1/market";

pub struct CryptoComSource {
    streams: HashMap<String, CryptoComTokenConfig>,
}

impl Source for CryptoComSource {
    fn name(&self) -> String {
        "Crypto.com".into()
    }

    fn tokens(&self) -> Vec<String> {
        self.streams.values().map(|s| s.token.clone()).collect()
    }

    fn query<'a>(&'a self, sink: &'a PriceSink) -> BoxFuture<'a, Result<()>> {
        self.query_impl(sink).boxed()
    }
}

impl CryptoComSource {
    pub fn new(config: &OracleConfig) -> Self {
        let streams = config
            .crypto_com
            .tokens
            .iter()
            .map(|t| (t.stream.clone(), t.clone()))
            .collect();
        Self { streams }
    }
    async fn query_impl(&self, sink: &PriceSink) -> Result<()> {
        let connection_timeout = Duration::from_secs(60);

        let (mut stream, _) = timeout(connection_timeout, connect_async(URL)).await??;
        let request = CryptoComRequest {
            id: -1,
            method: "subscribe".into(),
            params: Some(CryptoComRequestParameters {
                channels: self.streams.keys().cloned().collect(),
            }),
        };
        timeout(connection_timeout, stream.send(request.try_into()?)).await??;

        while let Ok(Some(result)) = timeout(connection_timeout, stream.next()).await {
            let message = result.context("Websocket error querying crypto.com")?;
            if !message.is_text() {
                continue;
            }
            let response: CryptoComResponse = serde_json::from_str(&message.into_text()?)?;
            if response.code != 0 {
                warn!(
                    "Crypto.com reported error {}: {}",
                    response.code,
                    response.message.as_deref().unwrap_or("(no message)")
                );
                continue;
            }
            if response.method == "public/heartbeat" {
                let response = CryptoComRequest {
                    id: response.id,
                    method: "public/respond-heartbeat".into(),
                    params: None,
                };
                timeout(connection_timeout, stream.send(response.try_into()?)).await??;
                continue;
            }
            if response.method != "subscribe" {
                warn!(
                    "Crypto.com returned unexpected response {}",
                    response.method
                );
                continue;
            }
            let Some(result) = response.result else {
                warn!("Crypto.com response was missing result");
                continue;
            };
            let Some(stream) = self.streams.get(&result.subscription) else {
                warn!(
                    subscription = result.subscription,
                    "Crypto.com returned unexpected response"
                );
                continue;
            };
            if result.data.len() != 1 {
                warn!(
                    "Crypto.com response had {} data, 1 expected",
                    result.data.len()
                );
                continue;
            }
            let data = &result.data[0];
            let value = Decimal::from_str(&data.best_bid_price)?;
            let volume = Decimal::from_str(&data.volume_24h)?;
            sink.send(PriceInfo {
                token: stream.token.clone(),
                unit: stream.unit.clone(),
                value,
                reliability: volume,
            })?;
        }

        bail!("Connection to crypto.com closed")
    }
}

#[derive(Serialize)]
struct CryptoComRequest {
    id: i64,
    method: String,
    params: Option<CryptoComRequestParameters>,
}

impl TryFrom<CryptoComRequest> for Message {
    type Error = anyhow::Error;

    fn try_from(value: CryptoComRequest) -> std::result::Result<Self, Self::Error> {
        Ok(Self::text(serde_json::to_string(&value)?))
    }
}

#[derive(Serialize)]
struct CryptoComRequestParameters {
    channels: Vec<String>,
}

#[derive(Deserialize)]
struct CryptoComResponse {
    id: i64,
    method: String,
    code: u64,
    message: Option<String>,
    result: Option<CryptoComResponseResult>,
}

#[derive(Deserialize)]
struct CryptoComResponseResult {
    subscription: String,
    data: Vec<CryptoComResponseData>,
}

#[derive(Deserialize)]
struct CryptoComResponseData {
    #[serde(rename = "b")]
    best_bid_price: String,
    #[serde(rename = "v")]
    volume_24h: String,
}
