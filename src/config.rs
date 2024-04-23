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
    pub logs: LogConfig,
    pub synthetics: Vec<SyntheticConfig>,
    pub collateral: Vec<CollateralConfig>,
    pub bybit: ByBitConfig,
    pub sundaeswap: SundaeSwapConfig,
    pub minswap: MinswapConfig,
    pub spectrum: SpectrumConfig,
}

#[derive(Debug, Deserialize)]
pub struct LogConfig {
    pub json: bool,
    pub level: String,
}

impl OracleConfig {
    pub fn heartbeat(&self) -> Duration {
        Duration::from_millis(self.heartbeat_ms)
    }

    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms)
    }

    pub fn hydrate_pools(&self, pools: &[Pool]) -> Vec<HydratedPool> {
        let assets = self.build_asset_lookup();
        pools
            .iter()
            .map(|p| {
                let token_asset = assets[&p.token];
                let token_asset_id = Self::find_asset_id(token_asset);
                let unit_asset = assets[&p.unit];
                let unit_asset_id = Self::find_asset_id(unit_asset);
                HydratedPool {
                    pool: p.clone(),
                    token_asset_id,
                    token_digits: token_asset.digits,
                    unit_asset_id,
                    unit_digits: unit_asset.digits,
                }
            })
            .collect()
    }

    fn build_asset_lookup(&self) -> HashMap<&String, &CollateralConfig> {
        let mut result = HashMap::new();
        for collateral in &self.collateral {
            result.insert(&collateral.name, collateral);
        }
        result
    }

    fn find_asset_id(asset: &CollateralConfig) -> Option<AssetId> {
        if asset.name == "ADA" {
            return None;
        }
        let asset_id = asset
            .asset_id
            .as_ref()
            .unwrap_or_else(|| panic!("Token {} has no asset id", asset.name));
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

#[derive(Debug, Deserialize)]
pub struct ByBitConfig {
    pub tokens: Vec<ByBitTokenConfig>,
}

#[derive(Debug, Deserialize)]
pub struct ByBitTokenConfig {
    pub token: String,
    pub unit: String,
    pub stream: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SundaeSwapConfig {
    pub use_api: bool,
    pub kupo_address: String,
    pub address: String,
    pub policy_id: String,
    pub pools: Vec<Pool>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct MinswapConfig {
    pub kupo_address: String,
    pub credential: String,
    pub pools: Vec<Pool>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SpectrumConfig {
    pub kupo_address: String,
    pub credential: String,
    pub pools: Vec<Pool>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Pool {
    pub token: String,
    pub unit: String,
    pub asset_id: String,
}

#[derive(Debug, Clone)]
pub struct HydratedPool {
    pub pool: Pool,
    pub token_asset_id: Option<AssetId>,
    pub token_digits: u32,
    pub unit_asset_id: Option<AssetId>,
    pub unit_digits: u32,
}

pub fn load_config(config_files: &[String]) -> Result<OracleConfig> {
    let mut builder = Config::builder().add_source(File::with_name("config.base.yaml"));
    for config_file in config_files {
        builder = builder.add_source(File::with_name(config_file));
    }
    let config = builder
        .add_source(Environment::with_prefix("ORACLE_"))
        .build()?;
    Ok(config.try_deserialize()?)
}
