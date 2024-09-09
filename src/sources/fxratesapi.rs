use std::{collections::BTreeMap, env, str::FromStr, time::Duration};

use anyhow::{anyhow, Result};
use chrono::Utc;
use cron::Schedule;
use futures::{future::BoxFuture, FutureExt};
use num_traits::Inv;
use reqwest::Client;
use rust_decimal::Decimal;
use serde::Deserialize;
use tokio::time::sleep;
use tracing::warn;

use crate::{
    config::OracleConfig,
    sources::source::{PriceInfo, PriceSink, Source},
};

const URL: &str = "https://api.fxratesapi.com/latest";

pub struct FxRatesApiSource {
    api_key: String,
    client: Client,
    schedule: Schedule,
    currencies: Vec<String>,
    base: String,
}

impl Source for FxRatesApiSource {
    fn name(&self) -> String {
        "FXRatesAPI".into()
    }
    fn max_time_without_updates(&self) -> Duration {
        let mut runs = self.schedule.upcoming(Utc);
        let next_run = runs.next().unwrap();
        let next_next_run = runs.next().unwrap();
        (next_next_run - next_run).to_std().unwrap()
    }
    fn tokens(&self) -> Vec<String> {
        self.currencies.clone()
    }
    fn query<'a>(&'a self, sink: &'a PriceSink) -> BoxFuture<Result<()>> {
        self.query_impl(sink).boxed()
    }
}

impl FxRatesApiSource {
    pub fn new(config: &OracleConfig) -> Result<Option<Self>> {
        let Ok(api_key) = env::var("FXRATESAPI_API_KEY") else {
            return Ok(None);
        };
        let client = Client::builder().build()?;
        let schedule = Schedule::from_str(&config.fxratesapi.cron)?;
        let currencies = config.fxratesapi.currencies.clone();
        let base = config.fxratesapi.base.clone();
        Ok(Some(Self {
            api_key,
            client,
            schedule,
            currencies,
            base,
        }))
    }

    async fn query_impl(&self, sink: &PriceSink) -> Result<()> {
        loop {
            self.read_prices(sink).await?;

            let next_run = self
                .schedule
                .upcoming(Utc)
                .next()
                .expect("time keeps on ticking");
            let time_until_next_run = (next_run - Utc::now()).to_std()?;

            sleep(time_until_next_run).await;
        }
    }

    async fn read_prices(&self, sink: &PriceSink) -> Result<()> {
        let response = self
            .client
            .get(URL)
            .query(&[
                ("amount", "1"),
                ("api_key", &self.api_key),
                ("base", &self.base),
                ("currencies", &self.currencies.join(",")),
            ])
            .timeout(Duration::from_secs(10))
            .send()
            .await?;
        let contents = response.text().await?;
        let payload = match serde_json::from_str(&contents)? {
            FxRatesApiResponse::Success(success) => success,
            FxRatesApiResponse::Error(error) => {
                return Err(anyhow!(
                    "FXRatesAPI error {}: {}",
                    error.error,
                    error.description
                ));
            }
        };

        for currency in &self.currencies {
            let Some(&value) = payload.rates.get(currency) else {
                warn!("No value found for {currency}");
                continue;
            };
            if value == 0.0 {
                warn!("FXRatesAPI reported value of {currency} as zero, ignoring");
                continue;
            }
            if value.is_infinite() {
                warn!("FXRatesAPI reported value of {currency} as infinity, ignoring");
                continue;
            }
            sink.send(PriceInfo {
                token: currency.clone(),
                unit: self.base.clone(),
                value: value.inv().try_into()?,
                reliability: Decimal::ONE,
            })?;
        }

        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum FxRatesApiResponse {
    Success(FxRatesApiSuccessResponse),
    Error(FxRatesApiErrorResponse),
}

#[derive(Deserialize)]
struct FxRatesApiSuccessResponse {
    rates: BTreeMap<String, f64>,
}

#[derive(Deserialize)]
struct FxRatesApiErrorResponse {
    error: String,
    description: String,
}
