use std::time::Duration;

use anyhow::Result;
use config::{Config, Environment, File};
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
}

impl OracleConfig {
    pub fn heartbeat(&self) -> Duration {
        Duration::from_millis(self.heartbeat_ms)
    }

    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms)
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
    pub price: Decimal,
    pub digits: u32,
}

pub fn load_config(config_file: &str) -> Result<OracleConfig> {
    let huh = Config::builder()
        .add_source(File::with_name("config.base.yaml"))
        .add_source(File::with_name(config_file).required(false))
        .add_source(Environment::with_prefix("ORACLE_"))
        .build()?;
    Ok(huh.try_deserialize()?)
}
