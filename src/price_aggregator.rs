use std::{collections::HashMap, sync::Arc, time::Duration};

use dashmap::DashMap;
use futures::{channel::mpsc, StreamExt};
use num_integer::Integer;
use rust_decimal::Decimal;
use tokio::{sync::watch::Sender, task::JoinSet, time::sleep};
use tracing::warn;

use crate::{
    apis::{
        self,
        source::{Origin, PriceInfo},
    },
    config::{Config, SyntheticConfig},
};

#[derive(Clone)]
pub struct PriceAggregator {
    prices: Arc<DashMap<(String, Origin), PriceInfo>>,
    tx: Arc<Sender<Vec<PriceFeed>>>,
    config: Arc<Config>,
}

impl PriceAggregator {
    pub fn new(tx: Sender<Vec<PriceFeed>>, config: Config) -> Self {
        PriceAggregator {
            prices: Arc::new(DashMap::new()),
            tx: Arc::new(tx),
            config: Arc::new(config),
        }
    }

    pub async fn run(&self) {
        let mut set = JoinSet::new();

        let me = self.clone();
        set.spawn(async move {
            me.aggregate().await;
        });

        let me = self.clone();
        set.spawn(async move {
            loop {
                me.report();
                sleep(Duration::from_secs(1)).await;
            }
        });

        while let Some(res) = set.join_next().await {
            if let Err(error) = res {
                warn!("{:?}", error);
            }
        }
    }

    async fn aggregate(&self) {
        let mut set = JoinSet::new();
        let (tx, mut rx) = mpsc::unbounded();

        let tx2 = tx.clone();
        let binance = apis::binance::BinanceSource::new();
        set.spawn(async move {
            binance.query(tx2).await;
        });

        let tx2 = tx.clone();
        let maestro = apis::maestro::MaestroSource::new().unwrap();
        set.spawn(async move {
            maestro.query(tx2).await;
        });

        while let Some(info) = rx.next().await {
            self.prices.insert((info.token.clone(), info.origin), info);
        }
    }

    fn report(&self) {
        // Normalize to ADA for now.
        // TODO: use whatever has the most liquidity
        let ada_in_usdt = self.get_collateral_price("ADA");
        let mut aggregated_prices: HashMap<String, Vec<Decimal>> = HashMap::new();
        for price_info in self.prices.iter() {
            let normalized_price = match price_info.relative_to.as_str() {
                "ADA" => price_info.value * ada_in_usdt,
                "USDb" => price_info.value,
                _ => panic!(
                    "Can't handle converting from {:?} to ADA",
                    price_info.relative_to
                ),
            };
            aggregated_prices
                .entry(price_info.token.clone())
                .and_modify(|e| e.push(normalized_price))
                .or_insert(vec![normalized_price]);
        }

        // Now we've collected a bunch of prices, average results across sources
        let mut all_prices = HashMap::new();
        for (token, prices) in aggregated_prices {
            let average_price = prices.iter().fold(Decimal::ZERO, |a, b| a + b)
                / Decimal::new(prices.len() as i64, 0);
            all_prices.insert(token, average_price);
        }

        let mut payloads = vec![];
        for synthetic in &self.config.synthetics {
            let price = all_prices
                .get(synthetic.name.as_str())
                .unwrap_or_else(|| &synthetic.price);
            if let Some(payload) = self.compute_payload(&synthetic, *price, &all_prices) {
                payloads.push(payload);
            }
        }
        self.tx.send_replace(payloads);
    }

    fn compute_payload(
        &self,
        synth: &SyntheticConfig,
        price: Decimal,
        all_prices: &HashMap<String, Decimal>,
    ) -> Option<PriceFeed> {
        let prices: Vec<_> = synth
            .collateral
            .iter()
            .map(|c| {
                all_prices
                    .get(c.as_str())
                    .cloned()
                    .unwrap_or_else(|| self.get_collateral_price(c.as_str()))
            })
            .map(|p| p / price)
            .collect();
        let (collateral_prices, denominator) = normalize_collateral_prices(&prices);
        Some(PriceFeed {
            collateral_prices,
            price,
            synthetic: synth.name.clone(),
            denominator,
        })
    }

    fn get_collateral_price(&self, collateral: &str) -> Decimal {
        let Some(config) = self.config.collateral.iter().find(|c| c.name == collateral) else {
            panic!("Unrecognized collateral {}", collateral);
        };
        config.price
    }
}

#[derive(Clone, Debug)]
pub struct PriceFeed {
    pub collateral_prices: Vec<u64>,
    pub synthetic: String,
    pub price: Decimal,
    pub denominator: u64,
}

fn normalize_collateral_prices(prices: &[Decimal]) -> (Vec<u64>, u64) {
    let scale = prices.iter().map(|p| p.scale()).max().unwrap_or(0);
    let normalized_values: Vec<_> = prices
        .iter()
        .map(|p| p.mantissa() * 10i128.pow(scale - p.scale()))
        .collect();

    let denominator = 10i128.pow(scale);
    let gcd = normalized_values
        .iter()
        .fold(denominator, |acc, &el| acc.gcd(&el));

    let collateral_prices = normalized_values.iter().map(|p| (p / gcd) as u64).collect();
    (collateral_prices, (denominator / gcd) as u64)
}

#[cfg(test)]
mod tests {
    use rust_decimal::Decimal;

    use super::normalize_collateral_prices;

    #[test]
    fn should_not_panic_on_empty_input() {
        assert_eq!((vec![], 1), normalize_collateral_prices(&[]));
    }

    #[test]
    fn should_compute_gcd() {
        let prices = [Decimal::new(5526312, 7), Decimal::new(1325517, 6)];
        let (collateral_prices, denominator) = normalize_collateral_prices(&prices);
        assert_eq!(
            (vec![2763156, 6627585], 5000000),
            (collateral_prices, denominator)
        );
    }

    #[test]
    fn should_normalize_numbers_with_same_decimal_count() {
        let prices = [Decimal::new(1337, 3), Decimal::new(9001, 3)];
        let (collateral_prices, denominator) = normalize_collateral_prices(&prices);
        assert_eq!((vec![1337, 9001], 1000), (collateral_prices, denominator));
    }

    #[test]
    fn should_handle_decimals_with_different_scales() {
        let prices = [
            Decimal::new(2_000, 3),
            Decimal::new(4_000_000, 6),
            Decimal::new(6_000_000_000, 9),
        ];
        let (collateral_prices, denominator) = normalize_collateral_prices(&prices);
        assert_eq!((vec![2, 4, 6], 1), (collateral_prices, denominator));
    }
}
