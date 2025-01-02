use std::{collections::BTreeMap, str::FromStr as _, time::Duration};

use anyhow::{bail, Result};
use futures::{future::BoxFuture, stream::FuturesUnordered, FutureExt, StreamExt};
use reqwest::Client;
use rust_decimal::Decimal;
use serde::Deserialize;
use tokio::time::sleep;
use tracing::Level;

use crate::config::{OkxTokenConfig, OracleConfig};

use super::source::{PriceInfo, PriceSink, Source};

pub struct OkxSource {
    indexes: BTreeMap<String, OkxTokenConfig>,
    client: Client,
}

impl Source for OkxSource {
    fn name(&self) -> String {
        "OKX".into()
    }

    fn tokens(&self) -> Vec<String> {
        self.indexes.values().map(|i| i.token.clone()).collect()
    }

    fn query<'a>(&'a self, sink: &'a PriceSink) -> BoxFuture<'a, Result<()>> {
        self.query_impl(sink).boxed()
    }
}

impl OkxSource {
    pub fn new(config: &OracleConfig) -> Result<Self> {
        let indexes = config
            .okx
            .tokens
            .iter()
            .map(|t| (t.index.clone(), t.clone()))
            .collect();
        let client = Client::builder().build()?;
        Ok(Self { indexes, client })
    }

    async fn query_impl(&self, sink: &PriceSink) -> Result<()> {
        loop {
            self.query_okx(sink).await;
            sleep(Duration::from_secs(3)).await;
        }
    }

    async fn query_okx(&self, sink: &PriceSink) {
        let mut futures = FuturesUnordered::new();
        for index in self.indexes.values() {
            futures.push(self.query_okx_index(sink, index));
        }
        while let Some(result) = futures.next().await {
            // tracing already logged the error, don't worry about it
            let _ = result;
        }
    }

    #[tracing::instrument(err(Debug, level = Level::WARN), skip_all, fields(index = config.index))]
    async fn query_okx_index(&self, sink: &PriceSink, config: &OkxTokenConfig) -> Result<()> {
        let response = self
            .client
            .get(TICKER_URL)
            .query(&[("instId", &config.index)])
            .header("Accept", "application/json")
            .timeout(Duration::from_secs(5))
            .send()
            .await?;
        let contents = response.text().await?;
        let payload: OkxResponse<OkxIndexTicker> = serde_json::from_str(&contents)?;
        if payload.data.is_empty() {
            bail!("{}: {}", payload.code, payload.msg);
        }
        let index = &payload.data[0];
        let value = Decimal::from_str(&index.latest_price)?;
        sink.send(PriceInfo {
            token: config.token.clone(),
            unit: config.unit.clone(),
            value,
            reliability: Decimal::ONE,
        })?;

        Ok(())
    }
}

#[derive(Deserialize)]
struct OkxResponse<T> {
    code: String,
    msg: String,
    data: Vec<T>,
}

#[derive(Deserialize)]
struct OkxIndexTicker {
    #[serde(rename = "idxPx")]
    latest_price: String,
}

const TICKER_URL: &str = "https://www.okx.com/api/v5/market/index-tickers";
