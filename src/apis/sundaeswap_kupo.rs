use std::{sync::Arc, time::Duration};

use anyhow::Result;
use dashmap::DashMap;
use futures::{future::BoxFuture, FutureExt};
use kupon::{AssetId, MatchOptions};
use rust_decimal::Decimal;
use tokio::time::sleep;

use crate::{
    apis::source::PriceInfo,
    config::{HydratedPool, OracleConfig, SundaeSwapKupoConfig},
};

use super::source::{PriceSink, Source};

#[derive(Clone)]
pub struct SundaeSwapKupoSource {
    client: Arc<kupon::Client>,
    config: Arc<SundaeSwapKupoConfig>,
    pools: DashMap<AssetId, HydratedPool>,
}

impl Source for SundaeSwapKupoSource {
    fn name(&self) -> String {
        "SundaeSwap Kupo".into()
    }

    fn tokens(&self) -> Vec<String> {
        self.pools.iter().map(|e| e.pool.token.clone()).collect()
    }

    fn query<'a>(&'a self, sink: &'a PriceSink) -> BoxFuture<Result<()>> {
        self.query_impl(sink).boxed()
    }
}

impl SundaeSwapKupoSource {
    pub fn new(config: &OracleConfig) -> Result<Self> {
        let sundae_config = &config.sundaeswap_kupo;
        let client = kupon::Builder::with_endpoint(&sundae_config.kupo_address).build()?;
        Ok(Self {
            client: Arc::new(client),
            config: Arc::new(sundae_config.clone()),
            pools: config
                .hydrate_pools(&sundae_config.pools)
                .into_iter()
                .map(|t| (AssetId::from_hex(&t.pool.asset_id), t))
                .collect(),
        })
    }

    async fn query_impl(&self, sink: &PriceSink) -> Result<()> {
        let options = MatchOptions::default()
            .address(&self.config.address)
            .only_unspent();
        for matc in self.client.matches(&options).await? {
            let Some(pool_asset_id) = matc
                .value
                .assets
                .keys()
                .find(|aid| aid.policy_id == self.config.policy_id)
            else {
                continue;
            };
            let Some(pool) = self.pools.get(pool_asset_id) else {
                continue;
            };

            let token_value = match &pool.token_asset_id {
                Some(token) => matc.value.assets[token],
                None => matc.value.coins,
            };

            let unit_value = match &pool.unit_asset_id {
                Some(token) => matc.value.assets[token],
                None => matc.value.coins,
            };

            let value = Decimal::new(unit_value as i64, 0) / Decimal::new(token_value as i64, 0);
            sink.send(PriceInfo {
                token: pool.pool.token.clone(),
                unit: pool.pool.unit.clone(),
                value,
            })?;
        }
        sleep(Duration::from_secs(3)).await;
        Ok(())
    }
}
