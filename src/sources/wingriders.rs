use std::{sync::Arc, time::Duration};

use anyhow::{anyhow, Result};
use futures::{future::BoxFuture, FutureExt};
use rust_decimal::Decimal;
use tokio::time::sleep;
use tracing::{warn, Level};

use crate::config::{HydratedPool, OracleConfig};

use super::{
    kupo::{get_asset_value, wait_for_sync, MaxConcurrencyFutureSet},
    source::{PriceInfo, PriceSink, Source},
};

pub struct WingRidersSource {
    client: Arc<kupon::Client>,
    max_concurrency: usize,
    pools: Vec<HydratedPool>,
}

impl Source for WingRidersSource {
    fn name(&self) -> String {
        "WingRiders".into()
    }

    fn tokens(&self) -> Vec<String> {
        self.pools.iter().map(|p| p.pool.token.clone()).collect()
    }

    fn query<'a>(&'a self, sink: &'a PriceSink) -> BoxFuture<'a, Result<()>> {
        self.query_impl(sink).boxed()
    }
}

impl WingRidersSource {
    pub fn new(config: &OracleConfig) -> Result<Self> {
        let wingriders_config = &config.wingriders;
        let client = kupon::Builder::with_endpoint(&wingriders_config.kupo_address)
            .with_retries(wingriders_config.retries)
            .with_timeout(Duration::from_millis(wingriders_config.timeout_ms))
            .build()?;
        Ok(Self {
            client: Arc::new(client),
            max_concurrency: wingriders_config.max_concurrency,
            pools: config.hydrate_pools(&wingriders_config.pools),
        })
    }

    async fn query_impl(&self, sink: &PriceSink) -> Result<()> {
        loop {
            self.query_wingriders(sink).await?;
            sleep(Duration::from_secs(3)).await;
        }
    }

    #[tracing::instrument(err(Debug, level = Level::WARN), skip_all)]
    async fn query_wingriders(&self, sink: &PriceSink) -> Result<()> {
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
                        "WingRiders reported value of {} as zero, ignoring",
                        pool.pool.token
                    ));
                }
                if token_value == 0 {
                    return Err(anyhow!(
                        "WingRiders reported value of {} as infinite, ignoring",
                        pool.pool.token
                    ));
                }
                let value = Decimal::new(unit_value as i64, pool.unit_digits)
                    / Decimal::new(token_value as i64, pool.token_digits);
                let tvl = Decimal::new(token_value as i64 * 2, 0);

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
                    warn!("error querying wingriders: {}", error);
                }
                Ok(()) => {
                    // all is well
                }
            }
        }

        Ok(())
    }
}
