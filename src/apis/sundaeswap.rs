use std::{str::FromStr, time::Duration};

use anyhow::Result;
use futures::{future::BoxFuture, FutureExt};
use reqwest::Client;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use tokio::time::sleep;

use crate::apis::source::Origin;

use super::source::{PriceInfo, PriceSink, Source};

const TOKENS: [&str; 5] = ["DJED", "ENCS", "LENFI", "MIN", "SNEK"];

pub struct SundaeSwapSource {
    client: Client,
}

impl Source for SundaeSwapSource {
    fn origin(&self) -> super::source::Origin {
        Origin::SundaeSwap
    }

    fn tokens(&self) -> Vec<String> {
        TOKENS.iter().map(|t| t.to_string()).collect()
    }

    fn query<'a>(&'a self, sink: &'a PriceSink) -> BoxFuture<anyhow::Result<()>> {
        self.query_impl(sink).boxed()
    }
}

const URL: &str = "https://api.sundae.fi/graphql";
const GQL_OPERATION_NAME: &str = "ButaneOracleQuery";
const GQL_QUERY: &str = "\
query ButaneOracleQuery {
    pools {
        byAsset(asset: \"ada.lovelace\") {
            assetA {
                ticker
                decimals
            }
            assetB {
                ticker
                decimals
            }
            current {
                quantityA {
                    quantity
                }
                quantityB {
                    quantity
                }
            }
        }
    }
}";

impl SundaeSwapSource {
    pub fn new() -> Result<Self> {
        let client = Client::builder().build()?;
        Ok(Self { client })
    }

    async fn query_impl(&self, sink: &PriceSink) -> Result<()> {
        loop {
            sleep(Duration::from_secs(3)).await;

            let query = serde_json::to_string(&GraphQLQuery {
                query: GQL_QUERY,
                operation_name: GQL_OPERATION_NAME,
            })?;

            let response = self
                .client
                .post(URL)
                .header("Accept", "application/json")
                .header("Content-Type", "application/json")
                .body(query)
                .timeout(Duration::from_secs(10))
                .send()
                .await?;
            let contents = response.text().await?;
            let gql_response: GraphQLResponse = serde_json::from_str(&contents)?;
            for pool in gql_response.data.pools.by_asset {
                let Some(token) = pool.asset_b.ticker else {
                    continue;
                };
                if TOKENS.iter().all(|&t| token != t) {
                    continue;
                }

                let Stats {
                    quantity_a,
                    quantity_b,
                } = pool.current;
                let numerator = Decimal::from_i128_with_scale(
                    i128::from_str(&quantity_a.quantity)?,
                    pool.asset_a.decimals,
                );
                let denominator = Decimal::from_i128_with_scale(
                    i128::from_str(&quantity_b.quantity)?,
                    pool.asset_b.decimals,
                );
                // TODO: maybe represent prices as numerator/denominator instead of decimal?
                let value = numerator / denominator;

                sink.send(PriceInfo {
                    token,
                    unit: "ADA".into(),
                    value,
                })?;
            }
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GraphQLQuery<'a> {
    query: &'a str,
    operation_name: &'a str,
}

#[derive(Deserialize)]
struct GraphQLResponse {
    data: DataQuery,
}

#[derive(Deserialize)]
struct DataQuery {
    pub pools: PoolsQuery,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PoolsQuery {
    pub by_asset: Vec<Pool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Pool {
    pub asset_a: Asset,
    pub asset_b: Asset,
    pub current: Stats,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Stats {
    pub quantity_a: AssetAmount,
    pub quantity_b: AssetAmount,
}

#[derive(Deserialize)]
struct AssetAmount {
    pub quantity: String,
}

#[derive(Deserialize)]
struct Asset {
    pub ticker: Option<String>,
    pub decimals: u32,
}
