use std::{collections::HashMap, str::FromStr, time::Duration};

use anyhow::Result;
use config::{Config, Environment, File, FileFormat};
use ed25519::{pkcs8::DecodePublicKey, PublicKeyBytes};
use ed25519_dalek::{SigningKey, VerifyingKey};
use kupon::AssetId;
use rust_decimal::Decimal;
use serde::Deserialize;
use tracing::Level;

use crate::{keys, network::NodeId};

#[derive(Deserialize)]
struct RawOracleConfig {
    /// Deprecated, switch to network_port
    pub port: Option<u16>,

    pub network_port: u16,
    pub health_port: u16,
    pub api_port: u16,
    pub network_timeout_ms: u64,
    pub consensus: bool,
    pub peers: Vec<RawPeerConfig>,
    pub heartbeat_ms: u64,
    pub timeout_ms: u64,
    pub round_duration_ms: u64,
    pub gema_periods: usize,
    pub price_precision: u64,
    pub logs: RawLogConfig,
    pub frost_address: Option<String>,
    pub keygen: KeygenConfig,
    pub synthetics: Vec<SyntheticConfig>,
    pub currencies: Vec<CurrencyConfig>,
    pub binance: BinanceConfig,
    pub bybit: ByBitConfig,
    pub coinbase: CoinbaseConfig,
    pub fxratesapi: FxRatesApiConfig,
    pub maestro: MaestroConfig,
    pub sundaeswap: SundaeSwapConfig,
    pub minswap: MinswapConfig,
    pub spectrum: SpectrumConfig,
}

pub struct OracleConfig {
    pub id: NodeId,
    pub label: String,
    pub network: NetworkConfig,
    pub health_port: u16,
    pub api_port: u16,
    pub consensus: bool,
    pub heartbeat: Duration,
    pub timeout: Duration,
    pub round_duration: Duration,
    pub gema_periods: usize,
    pub price_precision: u64,
    pub logs: LogConfig,
    pub frost_address: Option<String>,
    pub keygen: KeygenConfig,
    pub synthetics: Vec<SyntheticConfig>,
    pub currencies: Vec<CurrencyConfig>,
    pub bybit: ByBitConfig,
    pub binance: BinanceConfig,
    pub coinbase: CoinbaseConfig,
    pub fxratesapi: FxRatesApiConfig,
    pub maestro: MaestroConfig,
    pub sundaeswap: SundaeSwapConfig,
    pub minswap: MinswapConfig,
    pub spectrum: SpectrumConfig,
}

impl OracleConfig {
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

    fn build_asset_lookup(&self) -> HashMap<&String, &CurrencyConfig> {
        self.currencies.iter().map(|c| (&c.name, c)).collect()
    }

    fn find_asset_id(asset: &CurrencyConfig) -> Option<AssetId> {
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

impl TryFrom<RawOracleConfig> for OracleConfig {
    type Error = anyhow::Error;

    fn try_from(raw: RawOracleConfig) -> Result<Self, Self::Error> {
        let private_key = keys::read_private_key()?;

        let id = compute_node_id(&private_key.verifying_key());
        let mut label = id.to_string();
        let mut peers = vec![];

        let all_nodes: Result<Vec<Peer>> = raw.peers.into_iter().map(|p| p.try_into()).collect();
        for node in all_nodes? {
            if node.id == id {
                label = node.label;
            } else {
                peers.push(node);
            }
        }

        let network = NetworkConfig {
            port: raw.port.unwrap_or(raw.network_port),
            id: id.clone(),
            private_key,
            peers,
            timeout: Duration::from_millis(raw.network_timeout_ms),
        };

        let logs = LogConfig {
            id: id.clone(),
            label: label.clone(),
            json: raw.logs.json,
            level: Level::from_str(&raw.logs.level)?,
            otlp_endpoint: raw.logs.otlp_endpoint,
            uptrace_dsn: raw.logs.uptrace_dsn,
        };

        Ok(Self {
            id,
            label,
            network,
            health_port: raw.health_port,
            api_port: raw.api_port,
            consensus: raw.consensus,
            heartbeat: Duration::from_millis(raw.heartbeat_ms),
            timeout: Duration::from_millis(raw.timeout_ms),
            round_duration: Duration::from_millis(raw.round_duration_ms),
            gema_periods: raw.gema_periods,
            price_precision: raw.price_precision,
            logs,
            frost_address: raw.frost_address,
            keygen: raw.keygen,
            synthetics: raw.synthetics,
            currencies: raw.currencies,
            binance: raw.binance,
            bybit: raw.bybit,
            coinbase: raw.coinbase,
            fxratesapi: raw.fxratesapi,
            maestro: raw.maestro,
            sundaeswap: raw.sundaeswap,
            minswap: raw.minswap,
            spectrum: raw.spectrum,
        })
    }
}

pub struct NetworkConfig {
    pub id: NodeId,
    pub private_key: SigningKey,
    pub port: u16,
    pub peers: Vec<Peer>,
    pub timeout: Duration,
}

#[derive(Deserialize)]
struct RawLogConfig {
    pub json: bool,
    pub level: String,
    pub otlp_endpoint: Option<String>,
    pub uptrace_dsn: Option<String>,
}

pub struct LogConfig {
    pub id: NodeId,
    pub label: String,
    pub json: bool,
    pub level: Level,
    pub otlp_endpoint: Option<String>,
    pub uptrace_dsn: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct KeygenConfig {
    pub enabled: bool,
    pub min_signers: Option<u16>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct RawPeerConfig {
    pub label: Option<String>,
    pub address: String,
    pub public_key: String,
}

#[derive(Debug, Clone)]
pub struct Peer {
    pub id: NodeId,
    pub public_key: VerifyingKey,
    pub label: String,
    pub address: String,
}
impl TryFrom<RawPeerConfig> for Peer {
    type Error = anyhow::Error;

    fn try_from(raw: RawPeerConfig) -> Result<Self, Self::Error> {
        let public_key = {
            let key_bytes = PublicKeyBytes::from_public_key_pem(&raw.public_key)?;
            VerifyingKey::from_bytes(&key_bytes.0)?
        };
        let id = compute_node_id(&public_key);
        let label = raw.label.as_ref().unwrap_or(&raw.address).clone();
        Ok(Self {
            id,
            public_key,
            label,
            address: raw.address,
        })
    }
}

#[derive(Debug, Deserialize)]
pub struct SyntheticConfig {
    pub name: String,
    pub backing_currency: String,
    pub invert: bool,
    pub collateral: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct CurrencyConfig {
    pub name: String,
    pub asset_id: Option<String>,
    pub price: Decimal,
    pub digits: u32,
}

#[derive(Debug, Deserialize)]
pub struct BinanceConfig {
    pub tokens: Vec<BinanceTokenConfig>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct BinanceTokenConfig {
    pub token: String,
    pub unit: String,
    pub stream: String,
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

#[derive(Debug, Deserialize)]
pub struct CoinbaseConfig {
    pub tokens: Vec<CoinbaseTokenConfig>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CoinbaseTokenConfig {
    pub token: String,
    pub unit: String,
    pub product_id: String,
}

#[derive(Debug, Deserialize)]
pub struct FxRatesApiConfig {
    pub cron: String,
    pub currencies: Vec<String>,
    pub base: String,
}

#[derive(Debug, Deserialize)]
pub struct MaestroConfig {
    pub tokens: Vec<MaestroTokenConfig>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct MaestroTokenConfig {
    pub token: String,
    pub unit: String,
    pub dex: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SundaeSwapConfig {
    pub use_api: bool,
    pub kupo_address: String,
    pub credential: String,
    pub policy_id: String,
    pub pools: Vec<Pool>,
    pub max_concurrency: usize,
    pub retries: usize,
}

#[derive(Debug, Deserialize, Clone)]
pub struct MinswapConfig {
    pub kupo_address: String,
    pub credential: String,
    pub pools: Vec<Pool>,
    pub max_concurrency: usize,
    pub retries: usize,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SpectrumConfig {
    pub kupo_address: String,
    pub credential: String,
    pub pools: Vec<Pool>,
    pub max_concurrency: usize,
    pub retries: usize,
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
    let mut builder = Config::builder().add_source(File::from_str(
        include_str!("../config.base.yaml"),
        FileFormat::Yaml,
    ));
    for config_file in config_files {
        builder = builder.add_source(File::with_name(config_file));
    }
    let config = builder
        .add_source(Environment::with_prefix("ORACLE_"))
        .build()?;
    let raw: RawOracleConfig = config.try_deserialize()?;
    raw.try_into()
}

pub fn compute_node_id(public_key: &VerifyingKey) -> NodeId {
    NodeId::new(hex::encode(public_key.as_bytes()))
}
