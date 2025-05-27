use anyhow::{Result, anyhow};
use futures::{FutureExt, future::BoxFuture};
use reqwest::Client;
use rust_decimal::Decimal;
use serde::Deserialize;
use std::{env, sync::Arc, time::Duration};
use tokio::{task::JoinSet, time::sleep};
use tracing::{Level, warn};

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

    fn query<'a>(&'a self, sink: &'a PriceSink) -> BoxFuture<'a, Result<()>> {
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
            self.query_maestro(sink).await?;
            sleep(Duration::from_secs(3)).await;
        }
    }

    #[tracing::instrument(err(Debug, level = Level::WARN), skip_all)]
    async fn query_maestro(&self, sink: &PriceSink) -> Result<()> {
        let mut set = JoinSet::new();
        for token in self.tokens.iter().cloned() {
            let other = self.clone();
            let sink = sink.clone();
            set.spawn(async move { other.query_one(token, sink).await });
        }
        while let Some(join_result) = set.join_next().await {
            match join_result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    warn!("error from maestro: {:?}", error);
                }
                Err(join_error) => {
                    return Err(anyhow!("Panic while querying maestro! {:?}", join_error));
                }
            }
        }
        Ok(())
    }

    #[tracing::instrument(err(Debug, level = Level::WARN), skip_all, fields(token = config.token))]
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

        sink.send(PriceInfo {
            token: config.token.to_string(),
            unit: config.unit.to_string(),
            value: res.try_into()?,
            reliability: Decimal::ONE,
        })?;
        Ok(())
    }
}

#[derive(Deserialize)]
struct MaestroOHLCMessage {
    coin_a_open: f64,
    coin_a_close: f64,
}
