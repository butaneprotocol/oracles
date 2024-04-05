use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::{anyhow, Result};
use dashmap::DashMap;
use futures::{future::BoxFuture, FutureExt};
use kupon::{Match, MatchOptions};
use rust_decimal::Decimal;
use tokio::time::sleep;

use crate::{
    apis::source::PriceInfo,
    config::{SundaeSwapKupoConfig, SundaeSwapPool},
};

use super::source::{PriceSink, Source};

#[derive(Clone)]
pub struct KupoSource {
    client: Arc<kupon::Client>,
    config: Arc<SundaeSwapKupoConfig>,
    pools: DashMap<String, SundaeSwapPool>,
}

impl Source for KupoSource {
    fn name(&self) -> String {
        "Kupo".into()
    }

    fn tokens(&self) -> Vec<String> {
        self.pools.iter().map(|e| e.key().to_string()).collect()
    }

    fn query<'a>(&'a self, sink: &'a PriceSink) -> BoxFuture<Result<()>> {
        self.query_impl(sink).boxed()
    }
}

impl KupoSource {
    pub fn new(config: &SundaeSwapKupoConfig) -> Result<Self> {
        let client = kupon::Builder::with_endpoint("http://localhost:1442").build()?;
        Ok(Self {
            client: Arc::new(client),
            config: Arc::new(config.clone()),
            pools: config
                .pools
                .iter()
                .map(|t| (t.pool_asset_id.clone(), t.clone()))
                .collect(),
        })
    }

    async fn query_impl(&self, sink: &PriceSink) -> Result<()> {
        let options = MatchOptions::default()
            .address(&self.config.address)
            .only_unspent();
        for mut pool in self
            .client
            .matches(&options)
            .await?
            .into_iter()
            .flat_map(|m| self.parse_pool(m))
        {
            let Some(config) = self.pools.get(&pool.pool_asset_id) else {
                continue;
            };

            let Some(unit_value) = pool.assets.remove(&config.unit_asset_id) else {
                return Err(anyhow!(
                    "Couldn't find quantity of {} in SundaeSwap pool {}",
                    config.unit,
                    pool.pool_asset_id
                ));
            };

            let Some(token_value) = pool.assets.into_values().next() else {
                return Err(anyhow!("Couldn't find price for {}", config.token));
            };

            let value = Decimal::new(unit_value as i64, 0) / Decimal::new(token_value as i64, 0);
            sink.send(PriceInfo {
                token: config.token.clone(),
                unit: config.unit.clone(),
                value,
            })?;
        }
        sleep(Duration::from_secs(3)).await;
        Ok(())
    }

    fn parse_pool(&self, matc: Match) -> Option<SundaePool> {
        let mut pool_asset_id = None;
        let mut assets = HashMap::new();
        assets.insert(None, matc.value.coins);
        for (asset_id, value) in matc.value.assets {
            if asset_id.policy_id == self.config.policy_id {
                pool_asset_id = Some(asset_id.to_hex())
            } else {
                assets.insert(Some(asset_id.to_hex()), value);
            }
        }

        // exotic pools have two tokens as well as a little ada, and we should ignore the ada
        if assets.len() > 2 {
            assets.remove(&None);
        }

        pool_asset_id.map(|pool_asset_id| SundaePool {
            pool_asset_id,
            assets,
        })
    }
}

#[derive(Debug)]
struct SundaePool {
    pool_asset_id: String,
    assets: HashMap<Option<String>, u64>,
}
