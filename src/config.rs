use anyhow::Result;
use rust_decimal::Decimal;
use serde::Deserialize;
use tokio::fs;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub synthetics: Vec<SyntheticConfig>,
    pub collateral: Vec<CollateralConfig>,
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

pub async fn load_config() -> Result<Config> {
    let raw_config = fs::read("config.yaml").await?;
    return Ok(serde_yaml::from_slice(&raw_config)?);
}
