use std::{sync::Arc, time::Duration};

use anyhow::{anyhow, Result};
use futures::{future::BoxFuture, FutureExt};
use kupon::MatchOptions;
use rust_decimal::Decimal;
use tokio::{task::JoinSet, time::sleep};
use tracing::{warn, Instrument};

use crate::config::{HydratedPool, OracleConfig};

use super::source::{PriceInfo, PriceSink, Source};

pub struct MinswapSource {
    client: Arc<kupon::Client>,
    credential: String,
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
        let client = kupon::Builder::with_endpoint(&minswap_config.kupo_address).build()?;
        Ok(Self {
            client: Arc::new(client),
            credential: minswap_config.credential.clone(),
            pools: config
                .hydrate_pools(&minswap_config.pools)
                .into_iter()
                .map(Arc::new)
                .collect(),
        })
    }

    async fn query_impl(&self, sink: &PriceSink) -> Result<()> {
        let mut set = JoinSet::new();
        for pool in &self.pools {
            let client = self.client.clone();
            let sink = sink.clone();
            let pool = pool.clone();
            let options = MatchOptions::default()
                .credential(&self.credential)
                .asset_id(&pool.pool.asset_id)
                .only_unspent();

            set.spawn(
                async move {
                    let mut result = client.matches(&options).await?;
                    if result.len() != 1 {
                        return Err(anyhow!("more than one result returned"));
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

        while let Some(res) = set.join_next().await {
            match res {
                Err(error) => {
                    // the task was cancelled or panicked
                    warn!("error running minswap query: {}", error);
                }
                Ok(Err(error)) => {
                    // the task ran, but returned an error
                    warn!("error querying minswap: {}", error);
                }
                Ok(Ok(())) => {
                    // all is well
                }
            }
        }

        sleep(Duration::from_secs(3)).await;

        Ok(())
    }
}
