use anyhow::Result;
use reqwest::Client;
use serde::Deserialize;
use std::{env, sync::Arc, time::Duration};
use tokio::{task::JoinSet, time::sleep};
use tracing::warn;

use crate::{
    apis::source::{Origin, PriceInfo, PriceSink},
    token::Token,
};

// TODO: currencies shouldn't be hard-coded
const TOKENS: [&str; 6] = ["LENFI", "iUSD", "MIN", "SNEK", "ENCS", "DJED"];

#[derive(Clone)]
pub struct MaestroSource {
    api_key: Arc<String>,
    client: Arc<Client>,
}

fn ohlc_url(cnt: &str) -> String {
    format!(
        "https://mainnet.gomaestro-api.org/v1/markets/dexs/ohlc/minswap/ADA-{}",
        cnt
    )
}

impl MaestroSource {
    pub fn new() -> Result<Self> {
        let api_key = Arc::new(env::var("MAESTRO_API_KEY")?);
        let client = Arc::new(Client::builder().build()?);
        Ok(Self { api_key, client })
    }

    pub async fn query(&self, sink: PriceSink) {
        loop {
            let mut set = JoinSet::new();
            for token in TOKENS {
                let other = self.clone();
                let sink = sink.clone();
                set.spawn(async move { other.query_one(token, sink).await });
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
            .query(&[("limit", "1"), ("resolution", "1m")])
            .header("Accept", "application/json")
            .header("api-key", self.api_key.as_str())
            .timeout(Duration::from_secs(2))
            .send()
            .await?;
        let contents = response.text().await?;
        let messages: [MaestroOHLCMessage; 1] = serde_json::from_str(&contents)?;
        let res = (messages[0].coin_a_open + messages[0].coin_a_close) / 2.;

        sink.unbounded_send(PriceInfo {
            origin: Origin::Maestro,
            token: Token::value_of(token).unwrap(),
            value: res.try_into()?,
            relative_to: Token::ADA,
        })?;
        Ok(())
    }
}

#[derive(Deserialize)]
struct MaestroOHLCMessage {
    coin_a_open: f64,
    coin_a_close: f64,
}
