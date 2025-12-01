use std::{
    collections::BTreeMap,
    env::{self, VarError},
    path::PathBuf,
};

use anyhow::Context;
use minicbor::{Decode, Encode};
use num_rational::BigRational;
use num_traits::One as _;
use tracing::{info, warn};

use crate::{cbor::CborBigRational, config::OracleConfig};

use super::{TokenPrice, TokenPriceSource, conversions::TokenPriceConverter};

#[derive(Encode, Decode, Default)]
struct PersistedData {
    #[n(0)]
    prices: Vec<PersistedTokenPrice>,
}

#[derive(Encode, Decode)]
struct PersistedTokenPrice {
    #[n(0)]
    token: String,
    #[n(1)]
    value: CborBigRational,
}

pub struct TokenPricePersistence {
    filename: PathBuf,
    currencies: Vec<String>,
    prices: BTreeMap<String, BigRational>,
    warned_about_fs_error: bool,
}

impl TokenPricePersistence {
    pub fn new(config: &OracleConfig) -> Self {
        let currencies = config.currencies.iter().map(|c| c.name.clone()).collect();
        let mut filename: PathBuf = match env::var("DATA_DIRECTORY") {
            Ok(path) => path.into(),
            Err(VarError::NotUnicode(path)) => path.into(),
            Err(VarError::NotPresent) => "data".into(),
        };
        if filename.is_relative()
            && let Ok(pwd) = env::current_dir()
        {
            filename = pwd.join(filename);
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

        let prices: BTreeMap<String, BigRational> = data
            .prices
            .into_iter()
            .map(|p| (p.token, p.value.into()))
            .collect();
        Self {
            filename,
            currencies,
            prices,
            warned_about_fs_error: false,
        }
    }

    pub async fn save_prices(&mut self, converter: &TokenPriceConverter<'_>) {
        self.prices = self
            .currencies
            .iter()
            .filter_map(|token| {
                let value = converter.value_in_usd(token)?;
                Some((token.clone(), value))
            })
            .collect();
        let data = PersistedData {
            prices: self
                .prices
                .iter()
                .map(|(token, value)| PersistedTokenPrice {
                    token: token.clone(),
                    value: value.clone().into(),
                })
                .collect(),
        };
        let mut bytes = vec![];
        minicbor::encode(data, &mut bytes).expect("infallible");
        if let Err(error) = tokio::fs::write(&self.filename, bytes).await
            && !self.warned_about_fs_error
        {
            warn!("Could not save price data to disk: {:#}", error);
            self.warned_about_fs_error = true;
        }
    }

    pub fn saved_prices(&self) -> Vec<TokenPrice> {
        self.prices
            .iter()
            .map(|(token, value)| TokenPrice {
                token: token.clone(),
                unit: "USD".into(),
                value: value.clone(),
                sources: vec![TokenPriceSource {
                    name: "Loaded from disk".into(),
                    value: value.clone(),
                    reliability: BigRational::one(),
                }],
            })
            .collect()
    }
}
