use std::{collections::HashMap, str::FromStr, time::Duration};

use anyhow::{anyhow, Result};
use futures::{future::BoxFuture, FutureExt};
use reqwest::Client;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use tokio::time::sleep;
use tracing::warn;

use crate::config::OracleConfig;

use super::source::{PriceInfo, PriceSink, Source};

struct SundaeSwapPool {
    token: String,
    id: String,
}

pub struct SundaeSwapSource {
    client: Client,
    pools: Vec<SundaeSwapPool>,
}

impl Source for SundaeSwapSource {
    fn name(&self) -> String {
        "SundaeSwap".into()
    }

    fn tokens(&self) -> Vec<String> {
        self.pools.iter().map(|p| p.token.clone()).collect()
    }

    fn query<'a>(&'a self, sink: &'a PriceSink) -> BoxFuture<anyhow::Result<()>> {
        self.query_impl(sink).boxed()
    }
}

const URL: &str = "https://api.sundae.fi/graphql";
const GQL_OPERATION_NAME: &str = "ButaneOracleQuery";
const GQL_QUERY: &str = "\
query ButaneOracleQuery($ids: [ID!]!) {
    pools {
        byIds(ids: $ids) {
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
    pub fn new(config: &OracleConfig) -> Result<Self> {
        let client = Client::builder().build()?;
        let pools = config
            .sundaeswap
            .pools
            .iter()
            .map(|p| {
                // the id of a pool in sundaeswap's API is the asset name (minus a two-byte prefix)
                let prefix_len = config.sundaeswap.policy_id.len() + ".7020".len();
                let id = &p.asset_id[prefix_len..];
                SundaeSwapPool {
                    token: p.token.clone(),
                    id: id.to_string(),
                }
            })
            .collect();
        Ok(Self { client, pools })
    }

    async fn query_impl(&self, sink: &PriceSink) -> Result<()> {
        let mut variables = HashMap::new();
        variables.insert("ids", self.pools.iter().map(|p| p.id.as_str()).collect());
        let query = serde_json::to_string(&GraphQLQuery {
            query: GQL_QUERY,
            operation_name: GQL_OPERATION_NAME,
            variables,
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
        for error in &gql_response.errors.unwrap_or_default() {
            warn!("error returned by sundaeswap api: {}", error.message);
        }
        let Some(data) = gql_response.data else {
            return Err(anyhow!("no data returned by sundaeswap api"));
        };
        for pool in data.pools.by_ids {
            let Some(token) = pool.asset_b.ticker else {
                continue;
            };

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
                reliability: numerator,
            })?;
        }

        sleep(Duration::from_secs(3)).await;
        Ok(())
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GraphQLQuery<'a> {
    query: &'a str,
    operation_name: &'a str,
    variables: HashMap<&'a str, Vec<&'a str>>,
}

#[derive(Deserialize)]
struct GraphQLResponse {
    data: Option<DataQuery>,
    errors: Option<Vec<GraphQLError>>,
}

#[derive(Deserialize)]
struct GraphQLError {
    message: String,
}

#[derive(Deserialize)]
struct DataQuery {
    pub pools: PoolsQuery,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PoolsQuery {
    pub by_ids: Vec<Pool>,
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
