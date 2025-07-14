use std::{sync::Arc, time::Duration};

use anyhow::{Result, anyhow};
use futures::{FutureExt, future::BoxFuture};
use rust_decimal::Decimal;
use tokio::time::sleep;
use tracing::{Level, warn};

use crate::config::{HydratedPool, OracleConfig};

use super::{
    kupo::{MaxConcurrencyFutureSet, get_asset_value, wait_for_sync},
    source::{PriceInfo, PriceSink, Source},
};

pub struct MinswapSource {
    client: Arc<kupon::Client>,
    max_concurrency: usize,
    pools: Vec<HydratedPool>,
}

impl Source for MinswapSource {
    fn name(&self) -> String {
        "Minswap".into()
    }

    fn tokens(&self) -> Vec<String> {
        self.pools.iter().map(|p| p.pool.token.clone()).collect()
    }

    fn query<'a>(&'a self, sink: &'a PriceSink) -> BoxFuture<'a, Result<()>> {
        self.query_impl(sink).boxed()
    }
}

impl MinswapSource {
    pub fn new(config: &OracleConfig) -> Result<Self> {
        let minswap_config = config.minswap.as_ref()
            .ok_or_else(|| anyhow!("Minswap configuration not found"))?;
        let client = config
            .kupo_with_overrides(&minswap_config.kupo)
            .new_client()?;
        Ok(Self {
            client: Arc::new(client),
            max_concurrency: minswap_config.max_concurrency,
            pools: config.hydrate_pools(&minswap_config.pools),
        })
    }

    async fn query_impl(&self, sink: &PriceSink) -> Result<()> {
        loop {
            self.query_minswap(sink).await?;
            sleep(Duration::from_secs(3)).await;
        }
    }

    #[tracing::instrument(err(Debug, level = Level::WARN), skip_all)]
    async fn query_minswap(&self, sink: &PriceSink) -> Result<()> {
        wait_for_sync(&self.client).await;

        let mut set = MaxConcurrencyFutureSet::new(self.max_concurrency);
        for pool in &self.pools {
            let client = self.client.clone();
            let pool = pool.clone();
            let options = pool.pool.kupo_query();

            set.push(async move {
                let mut result = client.matches(&options).await?;
                if result.is_empty() {
                    return Err(anyhow!("pool not found for {}", pool.pool.token));
                }
                if result.len() > 1 {
                    return Err(anyhow!("more than one pool found for {}", pool.pool.token));
                }
                let matc = result.remove(0);
                let Some(token_value) = get_asset_value(&matc, &pool.token_asset_id) else {
                    return Err(anyhow!(
                        "no value found for asset {:?}",
                        pool.token_asset_id
                    ));
                };
                let Some(unit_value) = get_asset_value(&matc, &pool.unit_asset_id) else {
                    return Err(anyhow!("no value found for asset {:?}", pool.unit_asset_id));
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
                let tvl = Decimal::new(unit_value as i64 * 2, 0);

                sink.send_named(
                    PriceInfo {
                        token: pool.pool.token.clone(),
                        unit: pool.pool.unit.clone(),
                        value,
                        reliability: tvl,
                    },
                    &pool.pool.asset_id,
                )?;
                Ok(())
            });
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

        Ok(())
    }
}
