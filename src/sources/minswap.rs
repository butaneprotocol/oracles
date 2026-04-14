use std::{sync::Arc, time::Duration};

use anyhow::{Result, anyhow};
use futures::{FutureExt, future::BoxFuture};
use kupon::AssetId;
use pallas_primitives::PlutusData;
use rust_decimal::Decimal;
use tokio::time::sleep;
use tracing::{Level, warn};

use crate::config::{Asset, HydratedPool, OracleConfig};

use super::{
    kupo::{MaxConcurrencyFutureSet, find_match, get_asset_value, wait_for_sync},
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
        let minswap_config = &config.minswap;
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
                let result = client.matches(&options).await?;
                let mut matches = vec![];
                for m in result {
                    let Some(hash) = &m.datum else {
                        continue;
                    };
                    let Some(data) = client.datum(&hash.hash).await? else {
                        continue;
                    };
                    if is_for_assets(&data, &pool) {
                        matches.push(m);
                    }
                }
                let matc = find_match(matches, &pool)?;
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

fn is_for_assets(data: &str, pool: &HydratedPool) -> bool {
    let v1 = pool
        .pool
        .credential
        .as_ref()
        .is_some_and(|c| c == "e1317b152faac13426e6a83e06ff88a4d62cce3c1634ab0a5ec13309/*");
    let Some((a, b)) = extract_assets(data, v1) else {
        return false;
    };
    (a == pool.token_asset_id && b == pool.unit_asset_id)
        || (a == pool.unit_asset_id && b == pool.token_asset_id)
}

fn extract_assets(data: &str, v1: bool) -> Option<(Asset, Asset)> {
    let bytes = hex::decode(data).ok()?;
    let decoded: PlutusData = minicbor::decode(&bytes).ok()?;
    let PlutusData::Constr(constr) = decoded else {
        return None;
    };
    let (a_index, b_index) = if v1 { (0, 1) } else { (1, 2) };
    Some((
        extract_asset(constr.fields.get(a_index)?)?,
        extract_asset(constr.fields.get(b_index)?)?,
    ))
}

fn extract_asset(asset: &PlutusData) -> Option<Asset> {
    let PlutusData::Constr(constr) = asset else {
        return None;
    };
    if constr.fields.len() > 2 {
        return None;
    }
    let (PlutusData::BoundedBytes(policy_id), PlutusData::BoundedBytes(asset_name)) =
        (constr.fields.first()?, constr.fields.get(1)?)
    else {
        return None;
    };
    if policy_id.is_empty() && asset_name.is_empty() {
        Some(Asset::Ada)
    } else {
        Some(Asset::Native(AssetId {
            policy_id: policy_id.to_string(),
            asset_name: if asset_name.is_empty() {
                None
            } else {
                Some(asset_name.to_string())
            },
        }))
    }
}
