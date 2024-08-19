use std::{sync::Arc, time::Duration};

use anyhow::{anyhow, Result};
use futures::{future::BoxFuture, FutureExt};
use kupon::MatchOptions;
use rust_decimal::Decimal;
use tokio::time::sleep;
use tracing::{warn, Level};

use crate::config::{HydratedPool, OracleConfig};

use super::{
    kupo::{wait_for_sync, MaxConcurrencyFutureSet},
    source::{PriceInfo, PriceSink, Source},
};

pub struct SpectrumSource {
    client: Arc<kupon::Client>,
    credential: String,
    max_concurrency: usize,
    pools: Vec<Arc<HydratedPool>>,
}

impl Source for SpectrumSource {
    fn name(&self) -> String {
        "Spectrum".into()
    }

    fn tokens(&self) -> Vec<String> {
        self.pools.iter().map(|p| p.pool.token.clone()).collect()
    }

    fn query<'a>(&'a self, sink: &'a PriceSink) -> BoxFuture<Result<()>> {
        self.query_impl(sink).boxed()
    }
}

impl SpectrumSource {
    pub fn new(config: &OracleConfig) -> Result<Self> {
        let spectrum_config = &config.spectrum;
        let client = kupon::Builder::with_endpoint(&spectrum_config.kupo_address)
            .with_retries(spectrum_config.retries)
            .build()?;
        Ok(Self {
            client: Arc::new(client),
            credential: spectrum_config.credential.clone(),
            max_concurrency: spectrum_config.max_concurrency,
            pools: config
                .hydrate_pools(&spectrum_config.pools)
                .into_iter()
                .map(Arc::new)
                .collect(),
        })
    }

    async fn query_impl(&self, sink: &PriceSink) -> Result<()> {
        loop {
            self.query_spectrum(sink).await?;
            sleep(Duration::from_secs(3)).await;
        }
    }

    #[tracing::instrument(err(Debug, level = Level::WARN), skip_all)]
    async fn query_spectrum(&self, sink: &PriceSink) -> Result<()> {
        wait_for_sync(&self.client).await;

        let mut set = MaxConcurrencyFutureSet::new(self.max_concurrency);
        for pool in &self.pools {
            let client = self.client.clone();
            let sink = sink.clone();
            let pool = pool.clone();
            let options = MatchOptions::default()
                .credential(&self.credential)
                .asset_id(&pool.pool.asset_id)
                .only_unspent();

            set.push(async move {
                let mut result = client.matches(&options).await?;
                if result.is_empty() {
                    return Err(anyhow!("pool not found for {}", pool.pool.token));
                }
                if result.len() > 1 {
                    return Err(anyhow!("more than one pool found for {}", pool.pool.token));
                }
                let matc = result.remove(0);
                let token_value = match &pool.token_asset_id {
                    Some(token) => matc.value.assets[token],
                    None => matc.value.coins,
                };
                let unit_value = match &pool.unit_asset_id {
                    Some(token) => matc.value.assets[token],
                    None => matc.value.coins,
                };
                if unit_value == 0 {
                    return Err(anyhow!(
                        "Spectrum reported value of {} as zero, ignoring",
                        pool.pool.token
                    ));
                }
                if token_value == 0 {
                    return Err(anyhow!(
                        "Spectrum reported value of {} as infinite, ignoring",
                        pool.pool.token
                    ));
                }
                let value = Decimal::new(unit_value as i64, pool.unit_digits)
                    / Decimal::new(token_value as i64, pool.token_digits);
                let tvl = Decimal::new(token_value as i64 * 2, 0);

                sink.send(PriceInfo {
                    token: pool.pool.token.clone(),
                    unit: pool.pool.unit.clone(),
                    value,
                    reliability: tvl,
                })?;
                Ok(())
            });
        }

        while let Some(res) = set.next().await {
            match res {
                Err(error) => {
                    // the task ran, but returned an error
                    warn!("error querying spectrum: {}", error);
                }
                Ok(()) => {
                    // all is well
                }
            }
        }

        Ok(())
    }
}
