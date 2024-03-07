use std::{collections::HashMap, sync::Arc, time::Duration};

use dashmap::DashMap;
use futures::{channel::mpsc, StreamExt};
use num_integer::Integer;
use rust_decimal::Decimal;
use tokio::{task::JoinSet, time::sleep};
use tracing::warn;

use crate::{
    apis::{
        self,
        source::{Origin, PriceInfo},
    },
    token::Token,
};

#[derive(Clone)]
pub struct Aggregator {
    prices: Arc<DashMap<(Token, Origin), PriceInfo>>,
}

impl Aggregator {
    pub fn new() -> Self {
        Aggregator {
            prices: Arc::new(DashMap::new()),
        }
    }

    pub async fn run(&self) {
        let mut set = JoinSet::new();

        let me = self.clone();
        set.spawn(async move {
            me.aggregate().await;
        });

        let config = vec![SyntheticConfiguration {
            token: Token::USDT,
            collateral: vec![
                Token::LENFI,
                Token::IUSD,
                Token::MIN,
                Token::SNEK,
                Token::ENCS,
                Token::DJED,
            ],
        }];

        let me = self.clone();
        set.spawn(async move {
            loop {
                me.report(&config);
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
            self.prices.insert((info.token, info.origin), info);
        }
    }

    fn report(&self, config: &[SyntheticConfiguration]) {
        // Normalize to USDT for now.
        // TODO: this logic is almost certainly wrong.
        let ada_to_usdt = match self.prices.get(&(Token::ADA, Origin::Binance)) {
            Some(uh) => uh.value,
            _ => {
                // If we haven't found this value yet, just don't report anything
                return;
            }
        };
        let mut aggregated_prices: HashMap<Token, Vec<Decimal>> = HashMap::new();
        for price_info in self.prices.iter() {
            let normalized_price = match price_info.relative_to {
                Token::ADA => price_info.value * ada_to_usdt,
                Token::USDT => price_info.value,
                _ => panic!(
                    "Can't handle converting from {:?} to USDT",
                    price_info.relative_to
                ),
            };
            aggregated_prices
                .entry(price_info.token)
                .and_modify(|e| e.push(normalized_price))
                .or_insert(vec![normalized_price]);
        }

        // Now we've collected a bunch of prices, average results across sources
        let mut average_prices = HashMap::new();
        for (token, prices) in aggregated_prices {
            let average_price = prices.iter().fold(Decimal::ZERO, |a, b| a + b)
                / Decimal::new(prices.len() as i64, 0);
            average_prices.insert(token, average_price);
        }

        for synthetic in config {
            let payload = compute_payload(synthetic, &average_prices);
            println!("{:?}", payload);
        }
    }
}

#[derive(Debug)]
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

fn compute_payload(
    config: &SyntheticConfiguration,
    all_prices: &HashMap<Token, Decimal>,
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
