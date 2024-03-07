use std::collections::HashMap;

use futures::{channel::mpsc, StreamExt};
use num_integer::Integer;
use rust_decimal::Decimal;
use tokio::task::JoinSet;

use crate::{apis, token::Token};

pub struct Aggregator {}

impl Aggregator {
    pub fn new() -> Self {
        Aggregator {}
    }

    pub async fn aggregate(&self) {
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
            // TODO: tie it all together
            println!("{:?}", info);
        }
    }
}

pub struct PriceFeed {
    pub collateral_prices: Vec<u64>,
    pub synthetic: String,
    pub denominator: u64,
    /* TODO: validity */
}

pub struct SyntheticConfiguration {
    pub token: Token,
    pub collateral: Vec<Token>,
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

pub fn compute_payload(
    config: &SyntheticConfiguration,
    all_prices: HashMap<Token, Decimal>,
) -> PriceFeed {
    // TODO: how to handle missing prices?
    let prices: Vec<_> = config
        .collateral
        .iter()
        .map(|token| all_prices[token])
        .collect();
    let (collateral_prices, denominator) = normalize_collateral_prices(&prices);
    PriceFeed {
        collateral_prices,
        synthetic: config.token.name(),
        denominator,
    }
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
