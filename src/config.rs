use std::{collections::HashMap, time::Duration};

use anyhow::Result;
use config::{Config, Environment, File};
use kupon::AssetId;
use rust_decimal::Decimal;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct OracleConfig {
    pub port: u16,
    pub health_port: u16,
    pub consensus: bool,
    pub peers: Vec<PeerConfig>,
    pub heartbeat_ms: u64,
    pub timeout_ms: u64,
    pub synthetics: Vec<SyntheticConfig>,
    pub collateral: Vec<CollateralConfig>,
    pub sundaeswap_kupo: SundaeSwapKupoConfig,
    pub minswap: MinswapConfig,
}

impl OracleConfig {
    pub fn heartbeat(&self) -> Duration {
        Duration::from_millis(self.heartbeat_ms)
    }

    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms)
    }

    pub fn hydrate_pools(&self, pools: &[Pool]) -> Vec<HydratedPool> {
        let asset_ids = self.build_asset_id_lookup();
        pools
            .iter()
            .map(|p| {
                let token_asset_id = Self::find_asset_id(&p.token, &asset_ids);
                let unit_asset_id = Self::find_asset_id(&p.unit, &asset_ids);
                HydratedPool {
                    pool: p.clone(),
                    token_asset_id,
                    unit_asset_id,
                }
            })
            .collect()
    }

    fn build_asset_id_lookup(&self) -> HashMap<&String, &String> {
        let mut result = HashMap::new();
        for collateral in &self.collateral {
            if let Some(asset_id) = &collateral.asset_id {
                result.insert(&collateral.name, asset_id);
            }
        }
        result
    }

    fn find_asset_id(token: &String, asset_ids: &HashMap<&String, &String>) -> Option<AssetId> {
        if token == "ADA" {
            return None;
        }
        let asset_id = asset_ids
            .get(token)
            .unwrap_or_else(|| panic!("Unrecognized token {}", token));
        Some(AssetId::from_hex(asset_id))
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct PeerConfig {
    pub address: String,
}

#[derive(Debug, Deserialize)]
pub struct SyntheticConfig {
    pub name: String,
    pub price: Decimal,
    pub digits: u32,
    pub collateral: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct CollateralConfig {
    pub name: String,
    pub asset_id: Option<String>,
    pub price: Decimal,
    pub digits: u32,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SundaeSwapKupoConfig {
    pub kupo_address: String,
    pub address: String,
    pub policy_id: String,
    pub pools: Vec<SundaeSwapPool>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Pool {
    pub token: String,
    pub unit: String,
    pub asset_id: String,
}

pub struct HydratedPool {
    pub pool: Pool,
    pub token_asset_id: Option<AssetId>,
    pub unit_asset_id: Option<AssetId>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SundaeSwapPool {
    pub token: String,
    pub unit: String,
    pub unit_asset_id: Option<String>,
    pub pool_asset_id: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct MinswapConfig {
    pub kupo_address: String,
    pub credential: String,
    pub pools: Vec<Pool>,
}

pub fn load_config(config_file: &str) -> Result<OracleConfig> {
    let huh = Config::builder()
        .add_source(File::with_name("config.base.yaml"))
        .add_source(File::with_name(config_file).required(false))
        .add_source(Environment::with_prefix("ORACLE_"))
        .build()?;
    Ok(huh.try_deserialize()?)
}
