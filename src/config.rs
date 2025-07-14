use std::{collections::HashMap, str::FromStr, time::Duration};

use anyhow::{Result, bail};
use config::{Config, Environment, File, FileFormat};
use ed25519::{PublicKeyBytes, pkcs8::DecodePublicKey};
use ed25519_dalek::{SigningKey, VerifyingKey};
use kupon::{AssetId, MatchOptions};
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
    #[serde(default)]
    pub peers: Vec<RawPeerConfig>,
    pub heartbeat_ms: u64,
    pub timeout_ms: u64,
    pub round_duration_ms: u64,
    pub gema_periods: usize,
    pub price_precision: u64,
    pub max_source_price_age_ms: u64,
    pub use_persisted_prices: bool,
    pub max_price_divergence: Decimal,
    pub publish_url: Option<String>,
    pub publish_feed_base_url: Option<String>,
    pub logs: RawLogConfig,
    pub frost_address: Option<String>,
    pub keygen: KeygenConfig,
    #[serde(default)]
    pub enable_synthetics: bool,
    #[serde(default)]
    pub synthetics: Vec<SyntheticConfig>,
    pub currencies: Vec<CurrencyConfig>,
    pub feeds: FeedConfig,
    pub kupo: RawKupoConfig,
    pub binance: BinanceConfig,
    pub bybit: ByBitConfig,
    pub coinbase: CoinbaseConfig,
    pub crypto_com: CryptoComConfig,
    pub fxratesapi: FxRatesApiConfig,
    pub kucoin: KucoinConfig,
    pub maestro: MaestroConfig,
    pub okx: OkxConfig,
    pub sundaeswap: Option<SundaeSwapConfig>,
    pub minswap: Option<MinswapConfig>,
    pub splash: Option<SplashConfig>,
    pub cswap: Option<CSwapConfig>,
    pub vyfi: Option<VyFiConfig>,
    pub wingriders: Option<WingRidersConfig>,
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
    pub max_source_price_age: Duration,
    pub use_persisted_prices: bool,
    pub max_price_divergence: Decimal,
    pub publish_url: Option<String>,
    pub publish_feed_base_url: Option<String>,
    pub logs: LogConfig,
    pub frost_address: Option<String>,
    pub keygen: KeygenConfig,
    pub enable_synthetics: bool,
    pub synthetics: Vec<SyntheticConfig>,
    pub currencies: Vec<CurrencyConfig>,
    pub feeds: FeedConfig,
    pub kupo: KupoConfig,
    pub bybit: ByBitConfig,
    pub binance: BinanceConfig,
    pub coinbase: CoinbaseConfig,
    pub crypto_com: CryptoComConfig,
    pub fxratesapi: FxRatesApiConfig,
    pub kucoin: KucoinConfig,
    pub maestro: MaestroConfig,
    pub okx: OkxConfig,
    pub sundaeswap: Option<SundaeSwapConfig>,
    pub minswap: Option<MinswapConfig>,
    pub splash: Option<SplashConfig>,
    pub cswap: Option<CSwapConfig>,
    pub vyfi: Option<VyFiConfig>,
    pub wingriders: Option<WingRidersConfig>,
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

    pub fn kupo_with_overrides(&self, raw: &RawKupoConfig) -> KupoConfig {
        KupoConfig {
            address: raw
                .kupo_address
                .as_ref()
                .unwrap_or(&self.kupo.address)
                .clone(),
            retries: raw.retries.or(self.kupo.retries),
            timeout: raw
                .timeout_ms
                .map(Duration::from_millis)
                .or(self.kupo.timeout),
        }
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

        let publish_feed_base_url = raw.publish_feed_base_url.or_else(|| {
            let base_url = raw.publish_url.as_ref()?.strip_suffix("/oraclePrices")?;
            Some(format!("{base_url}/oracles"))
        });

        let kupo = KupoConfig {
            address: match raw.kupo.kupo_address {
                Some(addr) => addr,
                None => bail!("kupo.kupo_address required"),
            },
            retries: raw.kupo.retries,
            timeout: raw.kupo.timeout_ms.map(Duration::from_millis),
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
            max_source_price_age: Duration::from_millis(raw.max_source_price_age_ms),
            use_persisted_prices: raw.use_persisted_prices,
            max_price_divergence: raw.max_price_divergence,
            publish_url: raw.publish_url,
            publish_feed_base_url,
            logs,
            frost_address: raw.frost_address,
            keygen: raw.keygen,
            enable_synthetics: raw.enable_synthetics,
            synthetics: raw.synthetics,
            currencies: raw.currencies,
            feeds: raw.feeds,
            kupo,
            binance: raw.binance,
            bybit: raw.bybit,
            coinbase: raw.coinbase,
            crypto_com: raw.crypto_com,
            fxratesapi: raw.fxratesapi,
            kucoin: raw.kucoin,
            maestro: raw.maestro,
            okx: raw.okx,
            sundaeswap: raw.sundaeswap,
            minswap: raw.minswap,
            splash: raw.splash,
            cswap: raw.cswap,
            vyfi: raw.vyfi,
            wingriders: raw.wingriders,
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
    pub backing_currencies: Vec<String>,
    #[serde(default)]
    pub invert: bool,
    pub digits: u32,
    pub collateral: CollateralConfig,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "snake_case")]
pub enum CollateralConfig {
    List(Vec<String>),
    Nft(String),
}

#[derive(Debug, Deserialize)]
pub struct CurrencyConfig {
    pub name: String,
    pub asset_id: Option<String>,
    pub digits: u32,
    #[serde(default)]
    pub min_tvl: Decimal,
}

#[derive(Debug, Deserialize)]
pub struct FeedConfig {
    pub currencies: Vec<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct RawKupoConfig {
    pub kupo_address: Option<String>,
    pub retries: Option<usize>,
    pub timeout_ms: Option<u64>,
}

#[derive(Debug)]
pub struct KupoConfig {
    pub address: String,
    pub retries: Option<usize>,
    pub timeout: Option<Duration>,
}

impl KupoConfig {
    pub fn new_client(&self) -> Result<kupon::Client> {
        let mut builder = kupon::Builder::with_endpoint(&self.address);
        if let Some(retries) = self.retries {
            builder = builder.with_retries(retries);
        }
        if let Some(timeout) = self.timeout {
            builder = builder.with_timeout(timeout);
        }
        Ok(builder.build()?)
    }
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
pub struct CryptoComConfig {
    pub tokens: Vec<CryptoComTokenConfig>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct CryptoComTokenConfig {
    pub token: String,
    pub unit: String,
    pub stream: String,
}

#[derive(Debug, Deserialize)]
pub struct FxRatesApiConfig {
    pub cron: String,
    pub currencies: Vec<String>,
    pub base: String,
}

#[derive(Debug, Deserialize)]
pub struct KucoinConfig {
    pub tokens: Vec<KucoinTokenConfig>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct KucoinTokenConfig {
    pub token: String,
    pub unit: String,
    pub symbol: String,
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

#[derive(Debug, Deserialize)]
pub struct OkxConfig {
    pub tokens: Vec<OkxTokenConfig>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct OkxTokenConfig {
    pub token: String,
    pub unit: String,
    pub index: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SundaeSwapConfig {
    pub use_api: bool,
    #[serde(flatten)]
    pub kupo: RawKupoConfig,
    pub policy_id: String,
    pub pools: Vec<Pool>,
    pub max_concurrency: usize,
}

#[derive(Debug, Deserialize, Clone)]
pub struct MinswapConfig {
    #[serde(flatten)]
    pub kupo: RawKupoConfig,
    pub pools: Vec<Pool>,
    pub max_concurrency: usize,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SplashConfig {
    #[serde(flatten)]
    pub kupo: RawKupoConfig,
    pub pools: Vec<Pool>,
    pub max_concurrency: usize,
}

#[derive(Debug, Deserialize, Clone)]
pub struct CSwapConfig {
    #[serde(flatten)]
    pub kupo: RawKupoConfig,
    pub pools: Vec<Pool>,
    pub max_concurrency: usize,
}

#[derive(Debug, Deserialize, Clone)]
pub struct VyFiConfig {
    #[serde(flatten)]
    pub kupo: RawKupoConfig,
    pub pools: Vec<Pool>,
    pub max_concurrency: usize,
}

#[derive(Debug, Deserialize, Clone)]
pub struct WingRidersConfig {
    #[serde(flatten)]
    pub kupo: RawKupoConfig,
    pub pools: Vec<Pool>,
    pub max_concurrency: usize,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Pool {
    pub token: String,
    pub unit: String,
    pub credential: Option<String>,
    pub asset_id: String,
}

impl Pool {
    pub fn kupo_query(&self) -> MatchOptions {
        let mut query = MatchOptions::default();
        if let Some(credential) = &self.credential {
            query = query.credential(credential);
        }
        query.asset_id(&self.asset_id).only_unspent()
    }
}

#[derive(Debug, Clone)]
pub struct HydratedPool {
    pub pool: Pool,
    pub token_asset_id: Option<AssetId>,
    pub token_digits: u32,
    pub unit_asset_id: Option<AssetId>,
    pub unit_digits: u32,
}

pub fn load_config<S: AsRef<str>>(
    config_files: impl IntoIterator<Item = S>,
) -> Result<OracleConfig> {
    let mut builder = Config::builder().add_source(File::from_str(
        include_str!("../config.base.yaml"),
        FileFormat::Yaml,
    ));
    for config_file in config_files {
        builder = builder.add_source(File::with_name(config_file.as_ref()));
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

#[cfg(test)]
mod tests {
    use anyhow::Result;

    use super::load_config;

    #[test]
    fn should_load_default_config_without_errors() -> Result<()> {
        let test_keys_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/test_data/keys");
        let sample_config_file = concat!(env!("CARGO_MANIFEST_DIR"), "/config.example.yaml");
        temp_env::with_var("KEYS_DIRECTORY", Some(test_keys_dir), || {
            load_config([sample_config_file]).unwrap();
        });
        Ok(())
    }
}
