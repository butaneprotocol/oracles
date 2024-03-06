use anyhow::Result;
use reqwest::Client;
use serde::Deserialize;
use std::{env, sync::Arc, time::Duration};
use tokio::task::JoinSet;
use tracing::warn;

const TOKENS: [&str; 2] = ["LENFI", "iUSD"];
const ADA_MULTIPLIER: f64 = 0.5235;

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

    pub async fn query(&self) {
        loop {
            let mut set = JoinSet::new();
            for token in TOKENS {
                let other = self.clone();
                set.spawn(async move { other.query_one(token).await });
            }
            while let Some(Ok(result)) = set.join_next().await {
                match result {
                    Ok((token, price)) => {
                        println!("{} is at {}", token, price);
                    }
                    Err(error) => {
                        warn!("Error from maestro: {:?}", error);
                    }
                }
            }
        }
    }

    async fn query_one(&self, token: &str) -> Result<(String, f64)> {
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
        println!("{}", contents);
        let messages: [MaestroOHLCMessage; 1] = serde_json::from_str(&contents)?;
        let res = ADA_MULTIPLIER * (messages[0].coin_a_open + messages[0].coin_a_close) / 2.;
        Ok((token.to_string(), res))
    }
}

#[derive(Deserialize)]
struct MaestroOHLCMessage {
    coin_a_open: f64,
    coin_a_close: f64,
}
