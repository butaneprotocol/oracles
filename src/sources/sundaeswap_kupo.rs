use std::{sync::Arc, time::Duration};

use anyhow::{Result, anyhow};
use futures::{FutureExt, future::BoxFuture};
use pallas_primitives::conway::{BigInt, PlutusData};
use rust_decimal::Decimal;
use tokio::time::sleep;
use tracing::{Level, warn};

use crate::{
    config::{HydratedPool, OracleConfig},
    sources::source::PriceInfo,
};

use super::{
    kupo::{MaxConcurrencyFutureSet, get_asset_value_minus_tx_fee, wait_for_sync},
    source::{PriceSink, Source},
};

#[derive(Clone)]
pub struct SundaeSwapKupoSource {
    client: Arc<kupon::Client>,
    max_concurrency: usize,
    pools: Vec<HydratedPool>,
}

impl Source for SundaeSwapKupoSource {
    fn name(&self) -> String {
        "SundaeSwap Kupo".into()
    }

    fn tokens(&self) -> Vec<String> {
        self.pools.iter().map(|e| e.pool.token.clone()).collect()
    }

    fn query<'a>(&'a self, sink: &'a PriceSink) -> BoxFuture<'a, Result<()>> {
        self.query_impl(sink).boxed()
    }
}

impl SundaeSwapKupoSource {
    pub fn new(config: &OracleConfig) -> Result<Self> {
        let sundae_config = config.sundaeswap.as_ref()
            .ok_or_else(|| anyhow!("SundaeSwap configuration not found"))?;
        let client = config
            .kupo_with_overrides(&sundae_config.kupo)
            .new_client()?;
        Ok(Self {
            client: Arc::new(client),
            max_concurrency: sundae_config.max_concurrency,
            pools: config.hydrate_pools(&sundae_config.pools),
        })
    }

    async fn query_impl(&self, sink: &PriceSink) -> Result<()> {
        loop {
            self.query_sundaeswap(sink).await?;
            sleep(Duration::from_secs(3)).await;
        }
    }

    #[tracing::instrument(err(Debug, level = Level::WARN), skip_all)]
    async fn query_sundaeswap(&self, sink: &PriceSink) -> Result<()> {
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
                let Some(hash) = &matc.datum else {
                    return Err(anyhow!("no datum attached to result"));
                };
                let Some(data) = client.datum(&hash.hash).await? else {
                    return Err(anyhow!("could not get datum for sundae token"));
                };
                let tx_fee = match pool.pool.credential.as_deref() {
                    Some("4020e7fc2de75a0729c3cc3af715b34d98381e0cdbcfa99c950bc3ac/*") => 2_000_000,
                    _ => extract_tx_fee(&data)?,
                };

                let Some(token_value) =
                    get_asset_value_minus_tx_fee(&matc, &pool.token_asset_id, tx_fee)
                else {
                    return Err(anyhow!(
                        "no value found for asset {:?}",
                        pool.token_asset_id
                    ));
                };
                let Some(unit_value) =
                    get_asset_value_minus_tx_fee(&matc, &pool.unit_asset_id, tx_fee)
                else {
                    return Err(anyhow!("no value found for asset {:?}", pool.unit_asset_id));
                };

                if unit_value == 0 {
                    return Err(anyhow!(
                        "SundaeSwap reported value of {} as zero, ignoring",
                        pool.pool.token
                    ));
                }
                if token_value == 0 {
                    return Err(anyhow!(
                        "SundaeSwap reported value of {} as infinite, ignoring",
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
                    warn!("error querying sundaeswap: {}", error);
                }
                Ok(()) => {
                    // all is well
                }
            }
        }

        Ok(())
    }
}

const TX_FEE_FIELD_INDEX: usize = 7;
// sundaeswap tracks the tx fee (in lovelace) inside of the tx's datum
fn extract_tx_fee(data: &str) -> Result<u64> {
    let bytes = hex::decode(data)?;
    let decoded: PlutusData = minicbor::decode(&bytes)?;
    let PlutusData::Constr(constr) = decoded else {
        return Err(anyhow!("datum in unexpected format"));
    };
    let Some(PlutusData::BigInt(x)) = constr.fields.get(TX_FEE_FIELD_INDEX) else {
        return Err(anyhow!("datum missing ada cost"));
    };
    let value = match x {
        BigInt::Int(int) => u64::try_from(int.0)?,
        BigInt::BigUInt(bytes) | BigInt::BigNInt(bytes) => {
            let mut bytes: Vec<u8> = bytes.clone().into();
            // stored as big-endian
            if bytes.len() > 8 && bytes[0..bytes.len() - 8].iter().any(|b| *b != 0) {
                return Err(anyhow!("ada cost is too high"));
            }
            bytes.reverse();
            bytes.resize(8, 0);
            u64::from_le_bytes(bytes.try_into().unwrap())
        }
    };
    Ok(value)
}
