use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use crate::config::{CollateralConfig, OracleConfig};
use anyhow::{bail, Result};
use futures::{stream::FuturesUnordered, StreamExt};
use kupon::MatchOptions;
use pallas_primitives::{Constr, PlutusData};
use tracing::warn;

pub struct SyntheticConfigSource {
    asset_names: HashMap<String, String>,
    nfts: HashMap<String, String>,
    collateral: HashMap<String, Vec<String>>,
    client: kupon::Client,
    next_refresh: Instant,
}

impl SyntheticConfigSource {
    pub fn new(config: &OracleConfig) -> Result<Self> {
        let mut asset_ids = HashMap::new();
        let mut asset_names = HashMap::new();
        for (name, asset_id) in config
            .currencies
            .iter()
            .filter_map(|c| Some((&c.name, c.asset_id.as_ref()?)))
        {
            asset_ids.insert(name.clone(), asset_id.clone());
            asset_names.insert(asset_id.clone(), name.clone());
        }

        let mut nfts = HashMap::new();
        let mut collateral = HashMap::new();
        for synth in &config.synthetics {
            match &synth.collateral {
                CollateralConfig::List(tokens) => {
                    let mut tokens = tokens.clone();
                    tokens.sort_by_cached_key(|c| asset_ids.get(c));
                    collateral.insert(synth.name.clone(), tokens);
                }
                CollateralConfig::Nft(nft) => {
                    nfts.insert(synth.name.clone(), nft.clone());
                }
            }
        }
        let client = config.kupo.new_client()?;
        let next_refresh = Instant::now();
        Ok(Self {
            asset_names,
            nfts,
            collateral,
            client,
            next_refresh,
        })
    }

    pub async fn refresh(&mut self) {
        let now = Instant::now();
        if now < self.next_refresh {
            return;
        }

        let mut futures = FuturesUnordered::new();
        for (synthetic, nft) in &self.nfts {
            let asset_names = &self.asset_names;
            let client = &self.client;
            futures.push(async move {
                /// Convenient macro for returning an error,
                /// plus whether or not we should clear older config for the synthetic.
                /// In general, we want to keep using existing config on network error,
                /// but clear it on any other kind of error.
                macro_rules! fail {
                    ($clear:expr, $msg:literal $(,)?) => {
                        return Err(UpdateConfigError {
                            synthetic,
                            error: anyhow::anyhow!($msg),
                            clear: $clear,
                        })
                    };
                    ($clear:expr, $fmt:expr, $($arg:tt)*) => {
                        return Err(UpdateConfigError {
                            synthetic,
                            error: anyhow::anyhow!($fmt, $($arg)*),
                            clear: $clear,
                        })
                    };
                }
                let query = MatchOptions::default().asset_id(nft).only_unspent();
                let mut matches = match client.matches(&query).await {
                    Ok(matches) => matches,
                    Err(error) => fail!(false, "could not fetch owner for NFT {nft}: {error}"),
                };
                if matches.is_empty() {
                    fail!(true, "no UTxO found for NFT {nft}");
                }
                if matches.len() > 1 {
                    fail!(
                        true,
                        "found {} UTxOs for NFT {nft}, expected 1",
                        matches.len()
                    );
                }
                let Some(datum_hash) = matches.pop().unwrap().datum else {
                    fail!(true, "no datum associated with NFT {nft}");
                };
                let raw_datum = match client.datum(&datum_hash.hash).await {
                    Ok(Some(raw_datum)) => raw_datum,
                    Ok(None) => fail!(true, "datum not found for NFT {nft}"),
                    Err(error) => fail!(false, "could not fetch datum for NFT {nft}: {error}"),
                };
                let Ok(datum_bytes) = hex::decode(raw_datum) else {
                    fail!(true, "malformed datum for NFT {nft}");
                };
                let Ok(datum) = minicbor::Decoder::new(&datum_bytes).decode() else {
                    fail!(true, "invalid CBOR for NFT {nft}");
                };

                let collateral_assets = match extract_collateral_assets(datum) {
                    Ok(assets) => assets,
                    Err(error) => fail!(true, "could not parse datum for NFT {nft}: {error}"),
                };

                let mut collateral = vec![];
                for AssetClass {
                    policy_id,
                    asset_name,
                } in collateral_assets
                {
                    if policy_id.is_empty() && asset_name.is_empty() {
                        collateral.push("ADA".to_string());
                        continue;
                    }
                    let asset_id = format!("{policy_id}.{asset_name}");
                    let Some(name) = asset_names.get(&asset_id) else {
                        fail!(true, "unrecognized asset id {asset_id}");
                    };
                    collateral.push(name.clone());
                }

                Ok((synthetic, collateral))
            });
        }

        while let Some(result) = futures.next().await {
            match result {
                Ok((synthetic, collateral)) => {
                    self.collateral.insert(synthetic.clone(), collateral);
                }
                Err(error) => {
                    if error.clear {
                        self.collateral.remove(error.synthetic);
                    }
                    warn!(
                        "could not update parameters for {}: {}",
                        error.synthetic, error.error
                    );
                }
            }
        }

        self.next_refresh = now + Duration::from_secs(30);
    }

    pub fn synthetic_collateral(&self, name: &str) -> Option<Vec<String>> {
        Some(self.collateral.get(name)?.clone())
    }
}

// input is a MonoDatum from the butane Aiken definition
fn extract_collateral_assets(datum: PlutusData) -> Result<Vec<AssetClass>> {
    // extract the ParamsWrapper
    let [wrapper] = decode_struct(datum, 0)?;
    // extract the LiveParams
    let [params] = decode_struct(wrapper, 0)?;
    // extract the collateral assets from the LiveParams
    let [collateral_assets] = decode_struct(params, 0)?;

    let PlutusData::Array(collateral_assets) = collateral_assets else {
        bail!("datum has invalid collateral assets");
    };

    let mut assets = vec![];
    for (index, asset) in collateral_assets.to_vec().into_iter().enumerate() {
        let [policy_id, asset_name] = decode_struct(asset, 0)?;
        let PlutusData::BoundedBytes(policy_id) = policy_id else {
            bail!("asset {index} has invalid policy id");
        };
        let PlutusData::BoundedBytes(asset_name) = asset_name else {
            bail!("asset {index} has invalid asset name");
        };
        assets.push(AssetClass {
            policy_id: policy_id.into(),
            asset_name: asset_name.into(),
        });
    }
    Ok(assets)
}

fn decode_struct<const N: usize>(datum: PlutusData, variant: u64) -> Result<[PlutusData; N]> {
    let PlutusData::Constr(Constr { tag, fields, .. }) = datum else {
        bail!("datum is not a struct");
    };
    if tag != variant + 121 {
        bail!(
            "datum has unexpected variant (expected {}, got {})",
            variant + 121,
            tag
        );
    }
    fields
        .to_vec()
        .into_iter()
        .take(N)
        .collect::<Vec<_>>()
        .try_into()
        .map_err(|e: Vec<_>| {
            anyhow::anyhow!("too few elements (expected at least {N}, got {})", e.len())
        })
}

struct UpdateConfigError<'a> {
    synthetic: &'a str,
    error: anyhow::Error,
    clear: bool,
}

struct AssetClass {
    policy_id: String,
    asset_name: String,
}
