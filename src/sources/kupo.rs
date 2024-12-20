use std::{collections::HashMap, time::Duration};

use anyhow::{bail, Result};
use futures::{stream::FuturesUnordered, Future, StreamExt};
use kupon::{AssetId, Client, HealthStatus, Match};
use rust_decimal::Decimal;
use tokio::time::sleep;
use tracing::{debug, warn};

use crate::config::Pool;

use super::source::{PriceInfo, PriceSink};

pub async fn wait_for_sync(client: &Client) {
    let mut last_status = None;
    loop {
        let health = client.health().await;
        let status_changed = !last_status.is_some_and(|s| s == health.status);
        last_status = Some(health.status.clone());
        match health.status {
            HealthStatus::Healthy => return,
            HealthStatus::Syncing => {
                let status = health
                    .info
                    .map(|i| format!("syncing ({})", i.sync_progress()))
                    .unwrap_or("syncing".into());
                if status_changed {
                    warn!(
                        "Kupo server is {}. This source will be ignored until it is fully synced.",
                        status
                    );
                } else {
                    debug!("Kupo server is still {}.", status);
                }
                sleep(Duration::from_secs(1)).await;
            }
            HealthStatus::Disconnected => {
                if status_changed {
                    warn!("Kupo server is disconnected from the underlying node. Please check that it is configured correctly. This source will be ignored until kupo is connected and fully synced.")
                } else {
                    debug!("Kupo server is still disconnected from the underlying node.");
                }
                sleep(Duration::from_secs(5)).await;
            }
            HealthStatus::Error(reason) => {
                if status_changed {
                    warn!("Kupo server is unreachable or otherwise unhealthy: {}. Please ensure the server is running and configured correctly. This source will be ignored until it is.", reason);
                } else {
                    debug!("Kupo server is still unhealthy: {}", reason);
                }
                sleep(Duration::from_secs(10)).await;
            }
        }
    }
}

pub fn get_asset_value(matc: &Match, asset_id: &Option<AssetId>) -> Option<u64> {
    get_asset_value_minus_tx_fee(matc, asset_id, 0)
}

pub fn get_asset_value_minus_tx_fee(
    matc: &Match,
    asset_id: &Option<AssetId>,
    tx_fee: u64,
) -> Option<u64> {
    match asset_id {
        Some(token) => matc.value.assets.get(token).copied(),
        None => matc.value.coins.checked_sub(tx_fee),
    }
}

// a wrapper for PriceSink which aggregates the prices reported by different pools
pub struct MultiPoolPriceSink<'a> {
    sink: &'a PriceSink,
    prices: HashMap<TokenPair, Vec<TokenPrice>>,
    pool_counts: HashMap<TokenPair, usize>,
}

impl<'a> MultiPoolPriceSink<'a> {
    pub fn new<I: IntoIterator<Item = &'a Pool>>(sink: &'a PriceSink, pools: I) -> Self {
        let mut pool_counts = HashMap::new();
        for pool in pools {
            let pair = TokenPair::new(&pool.token, &pool.unit);
            *pool_counts.entry(pair).or_default() += 1;
        }
        Self {
            sink,
            prices: HashMap::new(),
            pool_counts,
        }
    }

    pub fn send(&mut self, price: PriceInfo) -> Result<()> {
        let pair = TokenPair::new(price.token, price.unit);
        let Some(expected_count) = self.pool_counts.get(&pair).copied() else {
            bail!("unexpected price reported for token pair {:?}", pair);
        };
        let prices = self.prices.entry(pair.clone()).or_default();
        prices.push(TokenPrice {
            value: price.value,
            tvl: price.reliability,
        });
        if prices.len() == expected_count {
            self.pool_counts.remove(&pair);
            let (pair, prices) = self.prices.remove_entry(&pair).unwrap();
            self.aggregate_and_send(pair, prices)?;
        }
        Ok(())
    }

    pub fn flush(mut self) -> Result<()> {
        for (pair, prices) in std::mem::take(&mut self.prices) {
            self.aggregate_and_send(pair, prices)?;
        }
        Ok(())
    }

    fn aggregate_and_send(&self, pair: TokenPair, prices: Vec<TokenPrice>) -> Result<()> {
        if prices.is_empty() {
            bail!("We are somehow reporting 0 prices");
        }
        let mut total_value = Decimal::ZERO;
        let mut total_tvl = Decimal::ZERO;
        for price in &prices {
            total_value += price.value * price.tvl;
            total_tvl += price.tvl;
        }
        self.sink.send(PriceInfo {
            token: pair.token,
            unit: pair.unit,
            value: total_value / total_tvl,
            reliability: total_tvl,
        })
    }
}

#[derive(Eq, PartialEq, Hash, Clone, Debug)]
struct TokenPair {
    token: String,
    unit: String,
}
impl TokenPair {
    fn new(token: impl Into<String>, unit: impl Into<String>) -> Self {
        Self {
            token: token.into(),
            unit: unit.into(),
        }
    }
}

struct TokenPrice {
    value: Decimal,
    tvl: Decimal,
}

/**
 * Runs a set of futures, only allowing a certain number to execute at a time.
 */
pub struct MaxConcurrencyFutureSet<F: Future> {
    running: FuturesUnordered<F>,
    queued: Vec<F>,
    concurrency: usize,
}
impl<F: Future> MaxConcurrencyFutureSet<F> {
    pub fn new(concurrency: usize) -> Self {
        assert!(concurrency > 0);
        Self {
            running: FuturesUnordered::new(),
            queued: vec![],
            concurrency,
        }
    }

    pub fn push(&mut self, future: F) {
        if self.running.len() < self.concurrency {
            self.running.push(future);
        } else {
            self.queued.push(future);
        }
    }

    pub async fn next(&mut self) -> Option<F::Output> {
        let next = self.running.next().await;
        if next.is_none() {
            if let Some(another_task) = self.queued.pop() {
                self.running.push(another_task);
                return self.running.next().await;
            }
        }
        next
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use rust_decimal::Decimal;
    use tokio::sync::mpsc;

    use crate::{config::Pool, sources::source::{PriceInfo, PriceInfoAsOf, PriceSink}};

    use super::MultiPoolPriceSink;

    fn make_pool(token: &str, unit: &str) -> Pool {
        Pool {
            token: token.into(),
            unit: unit.into(),
            credential: "".into(),
            asset_id: "".into()
        }
    }

    fn assert_next_value(rx: &mut mpsc::UnboundedReceiver<PriceInfoAsOf>, value: Option<PriceInfo>) {
        let info = rx.try_recv().ok().map(|pi| pi.info);
        assert_eq!(info, value);
    }

    #[test]
    fn multipoolpricesink_should_weight_prices_by_tvl() -> Result<()> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let inner_sink = PriceSink::new(tx);

        let pools = [
            make_pool("iUSD", "ADA"),
            make_pool("iUSD", "ADA"),
        ];

        let mut sink = MultiPoolPriceSink::new(&inner_sink, &pools);
        sink.send(PriceInfo {
            token: "iUSD".into(),
            unit: "ADA".into(),
            value: Decimal::new(4, 0),
            reliability: Decimal::new(1, 0),
        })?;
        sink.send(PriceInfo {
            token: "iUSD".into(),
            unit: "ADA".into(),
            value: Decimal::new(8, 0),
            reliability: Decimal::new(3, 0),
        })?;

        assert_next_value(&mut rx, Some(PriceInfo {
            token: "iUSD".into(),
            unit: "ADA".into(),
            value: Decimal::new(7, 0),
            reliability: Decimal::new(4, 0),
        }));

        assert_next_value(&mut rx, None);
        sink.flush()?;
        assert_next_value(&mut rx, None);

        Ok(())
    }

    #[test]
    fn multipoolpricesink_should_report_prices_when_one_pool_is_missing() -> Result<()> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let inner_sink = PriceSink::new(tx);

        let pools = [
            make_pool("iUSD", "ADA"),
            make_pool("iUSD", "ADA"),
        ];

        let mut sink = MultiPoolPriceSink::new(&inner_sink, &pools);
        sink.send(PriceInfo {
            token: "iUSD".into(),
            unit: "ADA".into(),
            value: Decimal::new(4, 0),
            reliability: Decimal::new(1, 0),
        })?;

        assert_next_value(&mut rx, None);
        sink.flush()?;
        assert_next_value(&mut rx, Some(PriceInfo {
            token: "iUSD".into(),
            unit: "ADA".into(),
            value: Decimal::new(4, 0),
            reliability: Decimal::new(1, 0),
        }));
        assert_next_value(&mut rx, None);

        Ok(())
    }
}