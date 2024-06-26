use std::{sync::Arc, time::Duration};

use anyhow::{anyhow, Result};
use futures::{future::BoxFuture, FutureExt};
use kupon::MatchOptions;
use rust_decimal::Decimal;
use tokio::time::sleep;
use tracing::{warn, Instrument};

use crate::config::{HydratedPool, OracleConfig};

use super::{
    kupo::{wait_for_sync, MaxConcurrencyFutureSet},
    source::{PriceInfo, PriceSink, Source},
};

pub struct MinswapSource {
    client: Arc<kupon::Client>,
    credential: String,
    max_concurrency: usize,
    pools: Vec<Arc<HydratedPool>>,
}

impl Source for MinswapSource {
    fn name(&self) -> String {
        "Minswap".into()
    }

    fn tokens(&self) -> Vec<String> {
        self.pools.iter().map(|p| p.pool.token.clone()).collect()
    }

    fn query<'a>(&'a self, sink: &'a PriceSink) -> BoxFuture<Result<()>> {
        self.query_impl(sink).boxed()
    }
}

impl MinswapSource {
    pub fn new(config: &OracleConfig) -> Result<Self> {
        let minswap_config = &config.minswap;
        let client = kupon::Builder::with_endpoint(&minswap_config.kupo_address)
            .with_retries(minswap_config.retries)
            .build()?;
        Ok(Self {
            client: Arc::new(client),
            credential: minswap_config.credential.clone(),
            max_concurrency: minswap_config.max_concurrency,
            pools: config
                .hydrate_pools(&minswap_config.pools)
                .into_iter()
                .map(Arc::new)
                .collect(),
        })
    }

    async fn query_impl(&self, sink: &PriceSink) -> Result<()> {
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

            set.push(
                async move {
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
                            "Minswap reported value of {} as zero, ignoring",
                            pool.pool.token
                        ));
                    }
                    if token_value == 0 {
                        return Err(anyhow!(
                            "Minswap reported value of {} as infinite, ignoring",
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
                }
                .in_current_span(),
            );
        }

        while let Some(res) = set.next().await {
            match res {
                Err(error) => {
                    // the task ran, but returned an error
                    warn!("error querying minswap: {}", error);
                }
                Ok(()) => {
                    // all is well
                }
            }
        }

        sleep(Duration::from_secs(3)).await;

        Ok(())
    }
}
