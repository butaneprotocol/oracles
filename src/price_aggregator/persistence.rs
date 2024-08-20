use std::{
    collections::BTreeMap,
    env::{self, VarError},
    path::PathBuf,
};

use anyhow::Context;
use minicbor::{Decode, Encode};
use rust_decimal::Decimal;
use tracing::{info, warn};

use crate::config::OracleConfig;

use super::{conversions::TokenPriceConverter, TokenPrice, TokenPriceSource};

#[derive(Encode, Decode, Default)]
struct PersistedData {
    #[n(0)]
    prices: Vec<PersistedTokenPrice>,
}

#[derive(Encode, Decode)]
struct PersistedTokenPrice {
    #[n(0)]
    token: String,
    /// Serialized Decimal
    #[n(1)]
    value: [u8; 16],
}

pub struct TokenPricePersistence {
    filename: PathBuf,
    prices: Vec<TokenPrice>,
    warned_about_fs_error: bool,
}

impl TokenPricePersistence {
    pub fn new(config: &OracleConfig) -> Self {
        let mut filename: PathBuf = match env::var("DATA_DIRECTORY") {
            Ok(path) => path.into(),
            Err(VarError::NotUnicode(path)) => path.into(),
            Err(VarError::NotPresent) => "data".into(),
        };
        if filename.is_relative() {
            if let Ok(pwd) = env::current_dir() {
                filename = pwd.join(filename);
            }
        }
        let _ = std::fs::create_dir_all(&filename);
        filename.push("prices.cbor");

        let data = std::fs::read(&filename)
            .context("file did not exist")
            .and_then(|bytes| minicbor::decode(&bytes).context("file was not valid"))
            .unwrap_or_else(|error| {
                info!("Persisted data not loaded: {:#}", error);
                PersistedData::default()
            });

        let old_prices: BTreeMap<String, Decimal> = data
            .prices
            .into_iter()
            .map(|p| (p.token, Decimal::deserialize(p.value)))
            .collect();
        let prices = config
            .currencies
            .iter()
            .map(|curr| {
                let token = curr.name.clone();
                let (value, source) = match old_prices.get(&token) {
                    Some(price) => (*price, "Loaded from disk"),
                    None => (curr.price, "Hard-coded default value"),
                };
                TokenPrice {
                    token,
                    unit: "USD".into(),
                    value,
                    sources: vec![TokenPriceSource {
                        name: source.into(),
                        value,
                        reliability: Decimal::ONE,
                    }],
                }
            })
            .collect();
        Self {
            filename,
            prices,
            warned_about_fs_error: false,
        }
    }

    pub async fn save_prices(&mut self, converter: &TokenPriceConverter<'_>) {
        for price in self.prices.iter_mut() {
            let value = converter.value_in_usd(&price.token);
            price.value = value;
            price.sources[0].value = value;
        }
        let data = PersistedData {
            prices: self
                .prices
                .iter()
                .map(|p| PersistedTokenPrice {
                    token: p.token.clone(),
                    value: p.value.serialize(),
                })
                .collect(),
        };
        let mut bytes = vec![];
        minicbor::encode(data, &mut bytes).expect("infallible");
        if let Err(error) = tokio::fs::write(&self.filename, bytes).await {
            if !self.warned_about_fs_error {
                warn!("Could not save price data to disk: {:#}", error);
                self.warned_about_fs_error = true;
            }
        }
    }

    pub fn previous_prices(&self) -> Vec<TokenPrice> {
        self.prices.clone()
    }
}
