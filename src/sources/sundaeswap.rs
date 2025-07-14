use std::{collections::HashMap, str::FromStr, time::Duration};

use anyhow::{Result, anyhow};
use futures::{FutureExt, future::BoxFuture};
use reqwest::Client;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use tokio::time::sleep;
use tracing::{Level, warn};

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

    fn query<'a>(&'a self, sink: &'a PriceSink) -> BoxFuture<'a, Result<()>> {
        self.query_impl(sink).boxed()
    }
}

const URL: &str = "https://api.sundae.fi/graphql";
const GQL_OPERATION_NAME: &str = "ButaneOracleQuery";
const GQL_QUERY: &str = "\
query ButaneOracleQuery($ids: [ID!]!) {
    pools {
        byIds(ids: $ids) {
            id
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
        let sundaeswap_config = config.sundaeswap.as_ref()
            .ok_or_else(|| anyhow!("SundaeSwap configuration not found"))?;
        let client = Client::builder().build()?;
        let pools = sundaeswap_config
            .pools
            .iter()
            .map(|p| {
                // the id of a pool in sundaeswap's API is the asset name (minus a four-byte prefix)
                let prefix_len = sundaeswap_config.policy_id.len() + ".000de140".len();
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
        loop {
            self.query_sundaeswap(sink).await?;
            sleep(Duration::from_secs(3)).await;
        }
    }

    #[tracing::instrument(err(Debug, level = Level::WARN), skip_all)]
    async fn query_sundaeswap(&self, sink: &PriceSink) -> Result<()> {
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
            let Some(unit) = pool.asset_a.ticker else {
                continue;
            };

            let Stats {
                quantity_a,
                quantity_b,
            } = pool.current;

            let token_value = i64::from_str(&quantity_b.quantity)?;
            let unit_value = i64::from_str(&quantity_a.quantity)?;
            if unit_value == 0 {
                return Err(anyhow!(
                    "SundaeSwap reported value of {} as zero, ignoring",
                    token
                ));
            }
            if token_value == 0 {
                return Err(anyhow!(
                    "SundaeSwap reported value of {} as infinite, ignoring",
                    token
                ));
            }

            let value = Decimal::new(unit_value, pool.asset_a.decimals)
                / Decimal::new(token_value, pool.asset_b.decimals);
            let tvl = Decimal::new(unit_value * 2, 0);

            sink.send_named(
                PriceInfo {
                    token,
                    unit,
                    value,
                    reliability: tvl,
                },
                &pool.id,
            )?;
        }

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
    pub id: String,
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
