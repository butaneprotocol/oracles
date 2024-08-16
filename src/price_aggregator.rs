use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};

use anyhow::Result;
use dashmap::DashMap;
use num_bigint::BigUint;
use num_integer::Integer;
use rust_decimal::{prelude::ToPrimitive, Decimal};
use tokio::{sync::watch::Sender, task::JoinSet, time::sleep};
use tracing::{debug, warn, Instrument};

use crate::{
    config::{OracleConfig, SyntheticConfig},
    health::HealthSink,
    price_feed::{IntervalBound, PriceFeed, PriceFeedEntry, Validity},
    sources::{
        binance::BinanceSource, bybit::ByBitSource, coinbase::CoinbaseSource,
        maestro::MaestroSource, minswap::MinswapSource, source::PriceInfo,
        spectrum::SpectrumSource, sundaeswap::SundaeSwapSource,
        sundaeswap_kupo::SundaeSwapKupoSource,
    },
};

pub use self::conversions::{TokenPrice, TokenPriceSource};
use self::{conversions::TokenPriceConverter, source_adapter::SourceAdapter};

mod conversions;
mod source_adapter;

pub struct PriceAggregator {
    feed_tx: Arc<Sender<Vec<PriceFeedEntry>>>,
    audit_tx: Arc<Sender<Vec<TokenPrice>>>,
    sources: Option<Vec<SourceAdapter>>,
    config: Arc<OracleConfig>,
}

impl PriceAggregator {
    pub fn new(
        feed_tx: Sender<Vec<PriceFeedEntry>>,
        audit_tx: Sender<Vec<TokenPrice>>,
        config: Arc<OracleConfig>,
    ) -> Result<Self> {
        let mut sources = vec![
            SourceAdapter::new(BinanceSource::new(&config)),
            SourceAdapter::new(ByBitSource::new(&config)),
            SourceAdapter::new(CoinbaseSource::new(&config)),
            SourceAdapter::new(MinswapSource::new(&config)?),
            SourceAdapter::new(SpectrumSource::new(&config)?),
        ];
        if let Some(maestro_source) = MaestroSource::new(&config)? {
            sources.push(SourceAdapter::new(maestro_source));
        } else {
            warn!("Not querying maestro, because no MAESTRO_API_KEY was provided");
        }
        if config.sundaeswap.use_api {
            sources.push(SourceAdapter::new(SundaeSwapSource::new(&config)?));
        } else {
            sources.push(SourceAdapter::new(SundaeSwapKupoSource::new(&config)?));
        }
        Ok(Self {
            feed_tx: Arc::new(feed_tx),
            audit_tx: Arc::new(audit_tx),
            sources: Some(sources),
            config,
        })
    }

    pub async fn run(mut self, health: &HealthSink) {
        let mut set = JoinSet::new();

        let sources = self.sources.take().unwrap();
        let price_maps: Vec<_> = sources
            .iter()
            .map(|s| (s.name.clone(), s.get_prices()))
            .collect();

        // Make each source update its price map asynchronously.
        for source in sources {
            let health = health.clone();
            set.spawn(
                async move {
                    source.run(health).await;
                }
                .in_current_span(),
            );
        }

        // Every second, we report the latest values from those price maps.
        set.spawn(
            async move {
                loop {
                    self.report(&price_maps);
                    sleep(Duration::from_secs(1)).await;
                }
            }
            .in_current_span(),
        );

        while let Some(res) = set.join_next().await {
            if let Err(error) = res {
                warn!("{:?}", error);
            }
        }
    }

    fn report(&self, price_maps: &[(String, Arc<DashMap<String, PriceInfo>>)]) {
        let mut source_prices = vec![];
        for (source, price_map) in price_maps {
            for price in price_map.iter() {
                source_prices.push((source.clone(), price.value().clone()));
            }
        }

        let converter = TokenPriceConverter::new(
            &source_prices,
            &self.config.synthetics,
            &self.config.currencies,
        );

        let price_feeds = self
            .config
            .synthetics
            .iter()
            .map(|s| self.compute_payload(s, &converter))
            .collect();
        self.feed_tx.send_replace(price_feeds);

        let token_values = converter.token_prices();
        self.audit_tx.send_replace(token_values);
    }

    fn compute_payload(
        &self,
        synth: &SyntheticConfig,
        converter: &TokenPriceConverter,
    ) -> PriceFeedEntry {
        let synth_digits = self.get_digits(&synth.name);
        let prices: Vec<_> = synth
            .collateral
            .iter()
            .map(|c| {
                let collateral_digits = self.get_digits(c.as_str());
                let multiplier = Decimal::new(10i64.pow(synth_digits), 0)
                    / Decimal::new(10i64.pow(collateral_digits), 0);
                let p = converter.value_in_usd(c.as_str());
                p * multiplier
            })
            .collect();
        let synth_price = converter.value_in_usd(&synth.name);

        // track metrics for the different prices
        for (collateral_name, collateral_price) in synth.collateral.iter().zip(prices.iter()) {
            let price = (collateral_price / synth_price)
                .to_f64()
                .expect("infallible");
            debug!(
                collateral_name,
                synthetic_name = synth.name,
                histogram.collateral_price = price,
                "price metrics",
            );
        }

        let (collateral_prices, denominator) = normalize(&prices, synth_price);
        let valid_from = SystemTime::now();
        let valid_to = valid_from + Duration::from_secs(300);

        PriceFeedEntry {
            price: synth_price,
            data: PriceFeed {
                collateral_names: Some(synth.collateral.clone()),
                collateral_prices,
                synthetic: synth.name.clone(),
                denominator,
                validity: Validity {
                    lower_bound: IntervalBound::moment(valid_from, true),
                    upper_bound: IntervalBound::moment(valid_to, false),
                },
            },
        }
    }

    fn get_digits(&self, currency: &str) -> u32 {
        if let Some(synth_config) = self.config.synthetics.iter().find(|s| s.name == currency) {
            return self.get_digits(&synth_config.backing_currency);
        }
        let Some(config) = self.config.currencies.iter().find(|c| c.name == currency) else {
            panic!("Unrecognized currency {}", currency);
        };
        config.digits
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
