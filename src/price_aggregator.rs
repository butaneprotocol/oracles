use std::{collections::HashMap, sync::Arc, time::Duration};

use dashmap::DashMap;
use futures::{channel::mpsc, StreamExt};
use num_bigint::BigUint;
use num_integer::Integer;
use rust_decimal::Decimal;
use tokio::{sync::watch::Sender, task::JoinSet, time::sleep};
use tracing::warn;

use crate::{
    apis::{
        self,
        source::{Origin, PriceInfo},
    },
    config::{CollateralConfig, Config, SyntheticConfig},
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
        let ada_in_usdt = self.get_collateral("ADA").price;
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
                let collateral = self.get_collateral(c.as_str());
                let multiplier = Decimal::new(10i64.pow(synth.digits), 0) / Decimal::new(10i64.pow(collateral.digits), 0);
                let p = all_prices
                    .get(c.as_str())
                    .cloned()
                    .unwrap_or_else(|| collateral.price);
                p * multiplier
            })
            .collect();
        let (collateral_prices, denominator) = normalize(&prices, price);
        Some(PriceFeed {
            collateral_prices,
            price,
            synthetic: synth.name.clone(),
            denominator,
        })
    }

    fn get_collateral(&self, collateral: &str) -> &CollateralConfig {
        let Some(config) = self.config.collateral.iter().find(|c| c.name == collateral) else {
            panic!("Unrecognized collateral {}", collateral);
        };
        config
    }
}

#[derive(Clone, Debug)]
pub struct PriceFeed {
    pub collateral_prices: Vec<BigUint>,
    pub synthetic: String,
    pub price: Decimal,
    pub denominator: BigUint,
}

fn normalize(prices: &[Decimal], denominator: Decimal) -> (Vec<BigUint>, BigUint) {
    let scale = prices.iter().fold(denominator.scale(), |acc, p| acc.max(p.scale()));
    let normalized_prices: Vec<BigUint> = prices
        .iter()
        .map(|p| {
            let res = BigUint::from(p.mantissa() as u128);
            res * BigUint::from(10u128.pow(scale - p.scale()))
        })
        .collect();
    let normalized_denominator = {
        let res = BigUint::from(denominator.mantissa() as u128);
        res * BigUint::from(10u128.pow(scale - denominator.scale()))
    };

    let gcd = normalized_prices
        .iter()
        .fold(normalized_denominator.clone(), |acc, el| acc.gcd(&el));

    let collateral_prices = normalized_prices.iter().map(|p| p / gcd.clone()).collect();
    let denominator = normalized_denominator / gcd;
    (collateral_prices, denominator)
}

#[cfg(test)]
mod tests {
    use rust_decimal::Decimal;

    use super::normalize;

    #[test]
    fn should_not_panic_on_empty_input() {
        assert_eq!((vec![], 1u128.into()), normalize(&[], Decimal::ONE));
    }

    #[test]
    fn should_compute_gcd() {
        let prices = [Decimal::new(5526312, 7), Decimal::new(1325517, 6)];
        let (collateral_prices, denominator) = normalize(&prices, Decimal::ONE);
        assert_eq!(
            (
                vec![2763156u128.into(), 6627585u128.into()],
                5000000u128.into()
            ),
            (collateral_prices, denominator)
        );
    }

    #[test]
    fn should_include_denominator_in_gcd() {
        let prices = [
            Decimal::new(2, 1),
            Decimal::new(4, 1),
            Decimal::new(6, 1),
        ];
        let (collateral_prices, denominator) = normalize(&prices, Decimal::new(7, 0));
        assert_eq!(
            (
                vec![1u128.into(), 2u128.into(), 3u128.into()],
                35u128.into(),
            ),
            (collateral_prices, denominator)
        );
    }

    #[test]
    fn should_normalize_numbers_with_same_decimal_count() {
        let prices = [Decimal::new(1337, 3), Decimal::new(9001, 3)];
        let (collateral_prices, denominator) = normalize(&prices, Decimal::ONE);
        assert_eq!(
            (vec![1337u128.into(), 9001u128.into()], 1000u128.into()),
            (collateral_prices, denominator)
        );
    }

    #[test]
    fn should_handle_decimals_with_different_scales() {
        let prices = [
            Decimal::new(2_000, 3),
            Decimal::new(4_000_000, 6),
            Decimal::new(6_000_000_000, 9),
        ];
        let (collateral_prices, denominator) = normalize(&prices, Decimal::ONE);
        assert_eq!(
            (vec![2u128.into(), 4u128.into(), 6u128.into()], 1u128.into()),
            (collateral_prices, denominator)
        );
    }
}
