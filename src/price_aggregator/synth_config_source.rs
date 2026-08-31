use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use crate::{
    config::{CollateralConfig, OracleConfig},
    health::{HealthSink, HealthStatus, Origin},
};
use anyhow::{Result, bail};
use futures::{StreamExt, stream::FuturesUnordered};
use kupon::MatchOptions;
use pallas_primitives::{Constr, PlutusData};
use plutus_parser::AsPlutus;

struct CollateralState {
    config: CollateralConfig,
    collateral: Option<Vec<Collateral>>,
}

pub struct SyntheticConfigSource {
    asset_names: HashMap<String, String>,
    collateral: HashMap<String, CollateralState>,
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

        let mut collateral = HashMap::new();
        for synth in &config.synthetics {
            collateral.insert(
                synth.name.clone(),
                CollateralState {
                    config: synth.collateral.clone(),
                    collateral: None,
                },
            );
        }

        let client = config.kupo.new_client()?;
        let next_refresh = Instant::now();
        Ok(Self {
            asset_names,
            collateral,
            client,
            next_refresh,
        })
    }

    pub async fn refresh(&mut self, health: &HealthSink) {
        let now = Instant::now();
        if now < self.next_refresh {
            return;
        }

        let mut futures = FuturesUnordered::new();
        for (synthetic, state) in &self.collateral {
            let asset_names = &self.asset_names;
            let client = &self.client;
            let synthetic = synthetic.clone();
            let Some(nft) = state.config.nft.clone() else {
                continue;
            };
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
                let query = MatchOptions::default().asset_id(&nft).only_unspent();
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
                for (ac, enabled) in collateral_assets {
                    if ac.policy_id.is_empty() && ac.asset_name.is_empty() {
                        collateral.push(Collateral {
                            name: "ADA".to_string(),
                            enabled,
                        });
                        continue;
                    }
                    let asset_id = format!(
                        "{}.{}",
                        hex::encode(ac.policy_id),
                        hex::encode(ac.asset_name)
                    );
                    let Some(name) = asset_names.get(&asset_id) else {
                        fail!(true, "unrecognized asset id {asset_id}");
                    };
                    collateral.push(Collateral {
                        name: name.clone(),
                        enabled,
                    });
                }

                Ok((synthetic, collateral))
            });
        }

        while let Some(result) = futures.next().await {
            match result {
                Ok((synthetic, collateral)) => {
                    self.collateral.get_mut(&synthetic).unwrap().collateral = Some(collateral);
                    health.update(
                        Origin::SyntheticConfig(synthetic.clone()),
                        HealthStatus::Healthy,
                    );
                }
                Err(error) => {
                    if error.clear {
                        self.collateral
                            .get_mut(&error.synthetic)
                            .unwrap()
                            .collateral = None;
                    }
                    health.update(
                        Origin::SyntheticConfig(error.synthetic.to_string()),
                        HealthStatus::Unhealthy(error.error.to_string()),
                    );
                }
            }
        }

        self.next_refresh = now + Duration::from_secs(30);
    }

    pub fn synthetic_collateral(&self, name: &str) -> Option<Vec<Collateral>> {
        let state = self.collateral.get(name)?;
        if let Some(collateral) = &state.collateral {
            Some(collateral.clone())
        } else if !state.config.list.is_empty() {
            Some(
                state
                    .config
                    .list
                    .iter()
                    .map(|c| Collateral {
                        name: c.clone(),
                        enabled: true,
                    })
                    .collect(),
            )
        } else {
            None
        }
    }
}

// input is a MonoDatum from the butane Aiken definition
fn extract_collateral_assets(datum: PlutusData) -> Result<Vec<(AssetClass, bool)>> {
    let params = MonoDatum::from_plutus(datum)?.wrapper.live_params;
    if params.tag != 121 {
        bail!(
            "datum has unexpected variant (expected 121, got {})",
            params.tag
        );
    }
    if params.fields.len() == 11 {
        extract_v1_collateral_assets(params.fields.to_vec())
    } else if params.fields.len() == 15 {
        extract_v2_collateral_assets(params.fields.to_vec())
    } else {
        bail!("datum has unexpected field count ({})", params.fields.len())
    }
}

fn extract_v1_collateral_assets(fields: Vec<PlutusData>) -> Result<Vec<(AssetClass, bool)>> {
    let assets: Vec<AssetClass> = AsPlutus::from_plutus(fields[0].clone())?;
    let proportions: Vec<u64> = AsPlutus::from_plutus(fields[5].clone())?;
    if assets.len() != proportions.len() {
        bail!(
            "mismatched number of assets ({}) and proportions ({})",
            assets.len(),
            proportions.len(),
        );
    }
    // in v1 this field can't be set to 0, so treat 1 as disabled
    let enabled = proportions.iter().map(|p| *p > 1);
    Ok(assets.into_iter().zip(enabled).collect())
}

fn extract_v2_collateral_assets(fields: Vec<PlutusData>) -> Result<Vec<(AssetClass, bool)>> {
    let assets: Vec<AssetClass> = AsPlutus::from_plutus(fields[0].clone())?;
    let proportions: Vec<BasisPoints> = AsPlutus::from_plutus(fields[4].clone())?;
    if assets.len() != proportions.len() {
        bail!(
            "mismatched number of assets ({}) and proportions ({})",
            assets.len(),
            proportions.len(),
        );
    }
    let enabled = proportions.iter().map(|p| p.points > 0);
    Ok(assets.into_iter().zip(enabled).collect())
}

struct UpdateConfigError {
    synthetic: String,
    error: anyhow::Error,
    clear: bool,
}

#[derive(AsPlutus, PartialEq, Eq, Debug)]
struct MonoDatum {
    wrapper: ParamsWrapper,
}

#[derive(AsPlutus, PartialEq, Eq, Debug)]
struct ParamsWrapper {
    // opaque, because we have to count the fields to differentiate v1 and v2
    live_params: Constr<PlutusData>,
}

#[derive(AsPlutus, PartialEq, Eq, Debug)]
struct AssetClass {
    policy_id: Vec<u8>,
    asset_name: Vec<u8>,
}

#[derive(AsPlutus, PartialEq, Eq, Debug)]
struct BasisPoints {
    points: u64,
}

#[derive(Clone)]
pub struct Collateral {
    pub name: String,
    pub enabled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset_class(policy_id: &str, asset_name: &str) -> AssetClass {
        AssetClass {
            policy_id: hex::decode(policy_id).unwrap(),
            asset_name: hex::decode(asset_name).unwrap(),
        }
    }

    #[test]
    fn should_parse_v1_nft() {
        let midas_nft_hex = "d8799fd8799fd8799f9fd8799f4040ffd8799f581c016be5325fd988fea98ad422fcfd53e5352cacfced5c106a932a35a44342544effd8799f581c279c909f348e533da5808898f87f9a14bb2c3dfbbacccd631d927a3f44534e454bffd8799f581c29d222ce763455e3d7a09a665ce554f00ac89d2e99a1a83d267170c6434d494effd8799f581c577f0b1342f8f8f4aed3388b80a8535812950c7a892495c0ecdf0f1e480014df10464c4454ffd8799f581c5d16cc1a177b5d9ba9cfa9793b07e60f1fb70fea1f8aef064415d11443494147ffd8799f581c8db269c3ec630e06ae29f74bc39edd1f87c819f1056206e879a1cd614c5368656e4d6963726f555344ffd8799f581c8fef2d34078659493ce161a6c7fba4b56afefa8535296a5743f695874441414441ffd8799f581c9a9693a9a37912a5097918f97918d15240c92ab729a0b7c4aa144d774653554e444145ffd8799f581c9abf0afd2f236a19f2842d502d0450cbcd9c79f123a9708f96fd9b9644454e4353ffd8799f581cda8c30857834c6ae7203935b89278c532b3995245295456f993e1d24424c51ffd8799f581cf66d78b4a3cb3d37afa0ec36461e51ecbde00f26c8f0a68f94b698804469455448ffff9f0c0f0f0f120f0c120f12120dff0a1a02625a009f9f3b000001952830e967190384ff9f00190384ffff9f192710191388191388191388190bb8191388191f40190bb8191388190bb8191388190bb8ff193a98190fa01926de1913889f9f3b000001952830e9671901f4ffffffffff";
        let midas_nft_bytes = hex::decode(midas_nft_hex).unwrap();
        let midas_nft_datum: PlutusData =
            minicbor::Decoder::new(&midas_nft_bytes).decode().unwrap();

        let assets = extract_collateral_assets(midas_nft_datum).unwrap();
        assert_eq!(
            assets,
            vec![
                (asset_class("", ""), true),
                (
                    asset_class(
                        "016be5325fd988fea98ad422fcfd53e5352cacfced5c106a932a35a4",
                        "42544e"
                    ),
                    true
                ),
                (
                    asset_class(
                        "279c909f348e533da5808898f87f9a14bb2c3dfbbacccd631d927a3f",
                        "534e454b"
                    ),
                    true
                ),
                (
                    asset_class(
                        "29d222ce763455e3d7a09a665ce554f00ac89d2e99a1a83d267170c6",
                        "4d494e"
                    ),
                    true
                ),
                (
                    asset_class(
                        "577f0b1342f8f8f4aed3388b80a8535812950c7a892495c0ecdf0f1e",
                        "0014df10464c4454"
                    ),
                    true
                ),
                (
                    asset_class(
                        "5d16cc1a177b5d9ba9cfa9793b07e60f1fb70fea1f8aef064415d114",
                        "494147"
                    ),
                    true
                ),
                (
                    asset_class(
                        "8db269c3ec630e06ae29f74bc39edd1f87c819f1056206e879a1cd61",
                        "5368656e4d6963726f555344"
                    ),
                    true
                ),
                (
                    asset_class(
                        "8fef2d34078659493ce161a6c7fba4b56afefa8535296a5743f69587",
                        "41414441"
                    ),
                    true
                ),
                (
                    asset_class(
                        "9a9693a9a37912a5097918f97918d15240c92ab729a0b7c4aa144d77",
                        "53554e444145"
                    ),
                    true
                ),
                (
                    asset_class(
                        "9abf0afd2f236a19f2842d502d0450cbcd9c79f123a9708f96fd9b96",
                        "454e4353"
                    ),
                    true
                ),
                (
                    asset_class(
                        "da8c30857834c6ae7203935b89278c532b3995245295456f993e1d24",
                        "4c51"
                    ),
                    true
                ),
                (
                    asset_class(
                        "f66d78b4a3cb3d37afa0ec36461e51ecbde00f26c8f0a68f94b69880",
                        "69455448"
                    ),
                    true
                ),
            ]
        );
    }

    #[test]
    fn should_parse_v1_nft_with_disabled_asset() {
        let fake_nft_hex = "d8799fd8799fd8799f9fd8799f4040ffd8799f581c016be5325fd988fea98ad422fcfd53e5352cacfced5c106a932a35a44342544effff9f0c0fff0a1a02625a009f9f3b000001952830e967190384ff9f00190384ffff9f19271001ff193a98190fa01926de1913889f9f3b000001952830e9671901f4ffffffffff";
        let fake_nft_bytes = hex::decode(fake_nft_hex).unwrap();
        let fake_nft_datum: PlutusData = minicbor::Decoder::new(&fake_nft_bytes).decode().unwrap();

        let assets = extract_collateral_assets(fake_nft_datum).unwrap();
        assert_eq!(
            assets,
            vec![
                (asset_class("", ""), true),
                (
                    asset_class(
                        "016be5325fd988fea98ad422fcfd53e5352cacfced5c106a932a35a4",
                        "42544e"
                    ),
                    false
                ),
            ],
        );
    }

    #[test]
    fn should_parse_v2_nft() {
        let midas_nft_hex = "d8799fd8799fd8799f9fd8799f4040ffd8799f581c39c520d0627aafa728f7e4dd10142b77c257813c36f57e2cb88f72a5480014df10464c4454ffd8799f581c39c520d0627aafa728f7e4dd10142b77c257813c36f57e2cb88f72a54441414441ffd8799f581c39c520d0627aafa728f7e4dd10142b77c257813c36f57e2cb88f72a54342544effd8799f581c39c520d0627aafa728f7e4dd10142b77c257813c36f57e2cb88f72a544454e4353ffd8799f581c39c520d0627aafa728f7e4dd10142b77c257813c36f57e2cb88f72a543494147ffd8799f581c39c520d0627aafa728f7e4dd10142b77c257813c36f57e2cb88f72a5424c51ffd8799f581c39c520d0627aafa728f7e4dd10142b77c257813c36f57e2cb88f72a5434d494effd8799f581c39c520d0627aafa728f7e4dd10142b77c257813c36f57e2cb88f72a544534e454bffd8799f581c39c520d0627aafa728f7e4dd10142b77c257813c36f57e2cb88f72a54653554e444145ffd8799f581c39c520d0627aafa728f7e4dd10142b77c257813c36f57e2cb88f72a54c5368656e4d6963726f555344ffd8799f581c39c520d0627aafa728f7e4dd10142b77c257813c36f57e2cb88f72a54469455448ffff9f0c12120f120f120f0f0f0c0dff0a1a02625a009fd8799f192710ffd8799f190bb8ffd8799f190bb8ffd8799f191388ffd8799f1903e8ffd8799f191388ffd8799f191388ffd8799f191388ffd8799f191388ffd8799f191388ffd8799f191f40ffd8799f190bb8ffffd8799f193a98ffd8799f190fa0ffd8799f1926deffd8799fd8799f191388ffff19ea60d8799f1901f4ffd8799f1905dcff9fd8799fd8799f4040ffd8799f0101ffffffd8799f01ffd8799f581cd3741b9582d28b2e90e20b8f2c28273f0afb1b555eed1f1847307d71ffffffff";
        let midas_nft_bytes = hex::decode(midas_nft_hex).unwrap();
        let midas_nft_datum: PlutusData =
            minicbor::Decoder::new(&midas_nft_bytes).decode().unwrap();

        let assets = extract_collateral_assets(midas_nft_datum).unwrap();
        assert_eq!(
            assets,
            vec![
                (asset_class("", ""), true),
                (
                    asset_class(
                        "39c520d0627aafa728f7e4dd10142b77c257813c36f57e2cb88f72a5",
                        "0014df10464c4454"
                    ),
                    true
                ),
                (
                    asset_class(
                        "39c520d0627aafa728f7e4dd10142b77c257813c36f57e2cb88f72a5",
                        "41414441"
                    ),
                    true
                ),
                (
                    asset_class(
                        "39c520d0627aafa728f7e4dd10142b77c257813c36f57e2cb88f72a5",
                        "42544e"
                    ),
                    true
                ),
                (
                    asset_class(
                        "39c520d0627aafa728f7e4dd10142b77c257813c36f57e2cb88f72a5",
                        "454e4353"
                    ),
                    true
                ),
                (
                    asset_class(
                        "39c520d0627aafa728f7e4dd10142b77c257813c36f57e2cb88f72a5",
                        "494147"
                    ),
                    true
                ),
                (
                    asset_class(
                        "39c520d0627aafa728f7e4dd10142b77c257813c36f57e2cb88f72a5",
                        "4c51"
                    ),
                    true
                ),
                (
                    asset_class(
                        "39c520d0627aafa728f7e4dd10142b77c257813c36f57e2cb88f72a5",
                        "4d494e"
                    ),
                    true
                ),
                (
                    asset_class(
                        "39c520d0627aafa728f7e4dd10142b77c257813c36f57e2cb88f72a5",
                        "534e454b"
                    ),
                    true
                ),
                (
                    asset_class(
                        "39c520d0627aafa728f7e4dd10142b77c257813c36f57e2cb88f72a5",
                        "53554e444145"
                    ),
                    true
                ),
                (
                    asset_class(
                        "39c520d0627aafa728f7e4dd10142b77c257813c36f57e2cb88f72a5",
                        "5368656e4d6963726f555344"
                    ),
                    true
                ),
                (
                    asset_class(
                        "39c520d0627aafa728f7e4dd10142b77c257813c36f57e2cb88f72a5",
                        "69455448"
                    ),
                    true
                ),
            ]
        );
    }

    #[test]
    fn should_parse_v2_nft_with_disabled_asset() {
        let fake_nft_hex = "d8799fd8799fd8799f9fd8799f4040ffd8799f581c39c520d0627aafa728f7e4dd10142b77c257813c36f57e2cb88f72a5480014df10464c4454ffff9f0c12ff0a1a02625a009fd8799f192710ffd8799f00ffffd8799f193a98ffd8799f190fa0ffd8799f1926deffd8799fd8799f191388ffff19ea60d8799f1901f4ffd8799f1905dcff9fd8799fd8799f4040ffd8799f0101ffffffd8799f01ffd8799f581cd3741b9582d28b2e90e20b8f2c28273f0afb1b555eed1f1847307d71ffffffff";
        let fake_nft_bytes = hex::decode(fake_nft_hex).unwrap();
        let fake_nft_datum: PlutusData = minicbor::Decoder::new(&fake_nft_bytes).decode().unwrap();

        let assets = extract_collateral_assets(fake_nft_datum).unwrap();
        assert_eq!(
            assets,
            vec![
                (asset_class("", ""), true),
                (
                    asset_class(
                        "39c520d0627aafa728f7e4dd10142b77c257813c36f57e2cb88f72a5",
                        "0014df10464c4454"
                    ),
                    false
                ),
            ],
        );
    }
}
