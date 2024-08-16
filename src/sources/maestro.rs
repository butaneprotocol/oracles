use anyhow::{anyhow, Result};
use futures::{future::BoxFuture, FutureExt};
use reqwest::Client;
use serde::Deserialize;
use std::{env, sync::Arc, time::Duration};
use tokio::{task::JoinSet, time::sleep};
use tracing::{warn, Instrument};

use crate::{
    config::{MaestroTokenConfig, OracleConfig},
    sources::source::{PriceInfo, PriceSink},
};

use super::source::Source;

fn ohlc_url(config: &MaestroTokenConfig) -> String {
    format!(
        "https://mainnet.gomaestro-api.org/v1/markets/dexs/ohlc/{}/{}-{}",
        config.dex, config.unit, config.token,
    )
}

#[derive(Clone)]
pub struct MaestroSource {
    api_key: Arc<String>,
    client: Arc<Client>,
    tokens: Arc<Vec<MaestroTokenConfig>>,
}

impl Source for MaestroSource {
    fn name(&self) -> String {
        "Maestro".into()
    }

    fn tokens(&self) -> Vec<String> {
        self.tokens.iter().map(|t| t.token.to_string()).collect()
    }

    fn query<'a>(&'a self, sink: &'a PriceSink) -> BoxFuture<Result<()>> {
        self.query_impl(sink).boxed()
    }
}

impl MaestroSource {
    pub fn new(config: &OracleConfig) -> Result<Option<Self>> {
        let Ok(api_key) = env::var("MAESTRO_API_KEY") else {
            return Ok(None);
        };
        let api_key = Arc::new(api_key);
        let client = Arc::new(Client::builder().build()?);
        let tokens = Arc::new(config.maestro.tokens.clone());
        Ok(Some(Self {
            api_key,
            client,
            tokens,
        }))
    }

    async fn query_impl(&self, sink: &PriceSink) -> Result<()> {
        loop {
            let mut set = JoinSet::new();
            for token in self.tokens.iter().cloned() {
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

    async fn query_one(&self, config: MaestroTokenConfig, sink: PriceSink) -> Result<()> {
        let response = self
            .client
            .get(ohlc_url(&config))
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
                config.token
            ));
        }

        sink.send(PriceInfo {
            token: config.token.to_string(),
            unit: config.unit.to_string(),
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
