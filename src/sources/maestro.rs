use anyhow::{anyhow, Result};
use futures::{future::BoxFuture, FutureExt};
use reqwest::Client;
use serde::Deserialize;
use std::{env, sync::Arc, time::Duration};
use tokio::{task::JoinSet, time::sleep};
use tracing::{warn, Instrument};

use crate::sources::source::{PriceInfo, PriceSink};

use super::source::Source;

// TODO: currencies shouldn't be hard-coded
const TOKENS: [&str; 6] = ["LENFI", "iUSD", "MIN", "SNEK", "ENCS", "DJED"];

fn ohlc_url(cnt: &str) -> String {
    format!(
        "https://mainnet.gomaestro-api.org/v1/markets/dexs/ohlc/minswap/ADA-{}",
        cnt
    )
}

#[derive(Clone)]
pub struct MaestroSource {
    api_key: Arc<String>,
    client: Arc<Client>,
}

impl Source for MaestroSource {
    fn name(&self) -> String {
        "Maestro".into()
    }

    fn tokens(&self) -> Vec<String> {
        TOKENS.iter().map(|t| t.to_string()).collect()
    }

    fn query<'a>(&'a self, sink: &'a PriceSink) -> BoxFuture<Result<()>> {
        self.query_impl(sink).boxed()
    }
}

impl MaestroSource {
    pub fn new() -> Result<Option<Self>> {
        let Ok(api_key) = env::var("MAESTRO_API_KEY") else {
            return Ok(None);
        };
        let api_key = Arc::new(api_key);
        let client = Arc::new(Client::builder().build()?);
        Ok(Some(Self { api_key, client }))
    }

    async fn query_impl(&self, sink: &PriceSink) -> Result<()> {
        loop {
            let mut set = JoinSet::new();
            for token in TOKENS {
                let other = self.clone();
                let sink = sink.clone();
                set.spawn(async move { other.query_one(token, sink).await }.in_current_span());
            }
            while let Some(Ok(result)) = set.join_next().await {
                if let Err(error) = result {
                    warn!("Error from maestro: {:?}", error);
                }
            }

            sleep(Duration::from_secs(3)).await;
        }
    }

    async fn query_one(&self, token: &str, sink: PriceSink) -> Result<()> {
        let response = self
            .client
            .get(ohlc_url(token))
            .query(&[("limit", "1"), ("resolution", "1d")])
            .header("Accept", "application/json")
            .header("api-key", self.api_key.as_str())
            .timeout(Duration::from_secs(10))
            .send()
            .await?;
        let contents = response.text().await?;
        let messages: [MaestroOHLCMessage; 1] = serde_json::from_str(&contents)?;
        let res = (messages[0].coin_a_open + messages[0].coin_a_close) / 2.;
        let volume = messages[0].coin_a_volume;

        if res == 0.0 {
            return Err(anyhow!(
                "Maestro reported value of {} as zero, ignoring",
                token
            ));
        }

        sink.send(PriceInfo {
            token: token.to_string(),
            unit: "ADA".to_string(),
            value: res.try_into()?,
            reliability: volume.try_into()?,
        })?;
        Ok(())
    }
}

#[derive(Deserialize)]
struct MaestroOHLCMessage {
    coin_a_open: f64,
    coin_a_close: f64,
    coin_a_volume: f64,
}
