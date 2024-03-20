use std::{sync::Arc, time::Duration};

use anyhow::Result;
use dashmap::DashMap;
use num_bigint::BigUint;
use num_integer::Integer;
use rust_decimal::Decimal;
use tokio::{sync::watch::Sender, task::JoinSet, time::sleep};
use tracing::warn;

use crate::{
    apis::{
        binance::BinanceSource, bybit::ByBitSource, coinbase::CoinbaseSource,
        maestro::MaestroSource, source::PriceInfo, sundaeswap::SundaeSwapSource,
    },
    config::{CollateralConfig, Config, SyntheticConfig},
    health::HealthSink,
    price_feed::{PriceFeed, PriceFeedEntry, Validity},
};

use self::{conversions::ConversionLookup, source_adapter::SourceAdapter};

mod conversions;
mod source_adapter;

pub struct PriceAggregator {
    tx: Arc<Sender<Vec<PriceFeedEntry>>>,
    sources: Option<Vec<SourceAdapter>>,
    config: Arc<Config>,
}

impl PriceAggregator {
    pub fn new(tx: Sender<Vec<PriceFeedEntry>>, config: Arc<Config>) -> Result<Self> {
        Ok(Self {
            tx: Arc::new(tx),
            sources: Some(vec![
                SourceAdapter::new(BinanceSource::new()),
                SourceAdapter::new(ByBitSource::new()),
                SourceAdapter::new(CoinbaseSource::new()),
                SourceAdapter::new(MaestroSource::new()?),
                SourceAdapter::new(SundaeSwapSource::new()?),
            ]),
            config,
        })
    }

    pub async fn run(mut self, health: &HealthSink) {
        let mut set = JoinSet::new();

        let sources = self.sources.take().unwrap();
        let price_maps: Vec<_> = sources.iter().map(|s| s.get_prices()).collect();

        // Make each source update its price map asynchronously.
        for source in sources {
            let health = health.clone();
            set.spawn(async move {
                source.run(health).await;
            });
        }

        // Every second, we report the latest values from those price maps.
        set.spawn(async move {
            loop {
                self.report(&price_maps);
                sleep(Duration::from_secs(1)).await;
            }
        });

        while let Some(res) = set.join_next().await {
            if let Err(error) = res {
                warn!("{:?}", error);
            }
        }
    }

    fn report(&self, price_maps: &[Arc<DashMap<String, PriceInfo>>]) {
        let mut prices: Vec<PriceInfo> = vec![];
        for price_map in price_maps {
            for price in price_map.iter() {
                prices.push(price.value().clone());
            }
        }

        let conversions = ConversionLookup::new(&prices, &self.config);
        let payloads = self
            .config
            .synthetics
            .iter()
            .map(|s| self.compute_payload(s, &conversions))
            .collect();
        self.tx.send_replace(payloads);
    }

    fn compute_payload(
        &self,
        synth: &SyntheticConfig,
        conversions: &ConversionLookup,
    ) -> PriceFeedEntry {
        let prices: Vec<_> = synth
            .collateral
            .iter()
            .map(|c| {
                let collateral = self.get_collateral(c.as_str());
                let multiplier = Decimal::new(10i64.pow(synth.digits), 0)
                    / Decimal::new(10i64.pow(collateral.digits), 0);
                let p = conversions.value_in_usd(&collateral.name);
                p * multiplier
            })
            .collect();
        let price = conversions.value_in_usd(&synth.name);
        let (collateral_prices, denominator) = normalize(&prices, price);
        PriceFeedEntry {
            price,
            data: PriceFeed {
                collateral_prices,
                synthetic: synth.name.clone(),
                denominator,
                // TODO: limit validity
                validity: Validity::default(),
            },
        }
    }

    fn get_collateral(&self, collateral: &str) -> &CollateralConfig {
        let Some(config) = self.config.collateral.iter().find(|c| c.name == collateral) else {
            panic!("Unrecognized collateral {}", collateral);
        };
        config
    }
}

fn normalize(prices: &[Decimal], denominator: Decimal) -> (Vec<BigUint>, BigUint) {
    let scale = prices
        .iter()
        .fold(denominator.scale(), |acc, p| acc.max(p.scale()));
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
        .fold(normalized_denominator.clone(), |acc, el| acc.gcd(el));

    let collateral_prices = normalized_prices.iter().map(|p| p / gcd.clone()).collect();
    let denominator = normalized_denominator / gcd;
    (collateral_prices, denominator)
}

#[cfg(test)]
mod tests {
    use rust_decimal::Decimal;

    use super::normalize;

    #[test]
    fn normalize_should_not_panic_on_empty_input() {
        assert_eq!((vec![], 1u128.into()), normalize(&[], Decimal::ONE));
    }

    #[test]
    fn normalize_should_compute_gcd() {
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
    fn normalize_should_include_denominator_in_gcd() {
        let prices = [Decimal::new(2, 1), Decimal::new(4, 1), Decimal::new(6, 1)];
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
    fn normalize_should_normalize_numbers_with_same_decimal_count() {
        let prices = [Decimal::new(1337, 3), Decimal::new(9001, 3)];
        let (collateral_prices, denominator) = normalize(&prices, Decimal::ONE);
        assert_eq!(
            (vec![1337u128.into(), 9001u128.into()], 1000u128.into()),
            (collateral_prices, denominator)
        );
    }

    #[test]
    fn normalize_should_handle_decimals_with_different_scales() {
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
