use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, SystemTime},
};

use anyhow::Result;
use dashmap::DashMap;
use gema::GemaCalculator;
use num_bigint::{BigInt, BigUint};
use num_integer::Integer as _;
use num_rational::BigRational;
use num_traits::{One as _, Signed as _, ToPrimitive as _};
use persistence::TokenPricePersistence;
use tokio::{sync::watch, task::JoinSet, time::sleep};
use tracing::{debug, error, warn};

use crate::{
    config::{OracleConfig, SyntheticConfig},
    health::HealthSink,
    price_feed::{IntervalBound, PriceFeed, PriceFeedEntry, Validity},
    signature_aggregator::{Payload, TimestampedPayloadEntry},
    sources::{
        binance::BinanceSource, bybit::ByBitSource, coinbase::CoinbaseSource,
        fxratesapi::FxRatesApiSource, maestro::MaestroSource, minswap::MinswapSource,
        source::PriceInfo, spectrum::SpectrumSource, sundaeswap::SundaeSwapSource,
        sundaeswap_kupo::SundaeSwapKupoSource,
    },
};

pub use self::conversions::{TokenPrice, TokenPriceSource};
use self::{conversions::TokenPriceConverter, source_adapter::SourceAdapter};

mod conversions;
mod gema;
mod persistence;
mod source_adapter;
mod utils;

struct PreviousPriceFeed {
    timestamp: SystemTime,
    prices: Vec<BigRational>,
}

pub struct PriceAggregator {
    feed_sink: watch::Sender<Vec<PriceFeedEntry>>,
    audit_sink: watch::Sender<Vec<TokenPrice>>,
    payload_source: watch::Receiver<Payload>,
    price_sources: Option<Vec<SourceAdapter>>,
    previous_prices: BTreeMap<String, PreviousPriceFeed>,
    persistence: TokenPricePersistence,
    gema: GemaCalculator,
    config: Arc<OracleConfig>,
}

impl PriceAggregator {
    pub fn new(
        feed_sink: watch::Sender<Vec<PriceFeedEntry>>,
        audit_sink: watch::Sender<Vec<TokenPrice>>,
        payload_source: watch::Receiver<Payload>,
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
        if let Some(fxratesapi_source) = FxRatesApiSource::new(&config)? {
            sources.push(SourceAdapter::new(fxratesapi_source));
        } else {
            warn!("Not querying FXRatesAPI, because no FXRATESAPI_API_KEY was provided");
        }
        if config.sundaeswap.use_api {
            sources.push(SourceAdapter::new(SundaeSwapSource::new(&config)?));
        } else {
            sources.push(SourceAdapter::new(SundaeSwapKupoSource::new(&config)?));
        }
        Ok(Self {
            feed_sink,
            audit_sink,
            payload_source,
            price_sources: Some(sources),
            previous_prices: BTreeMap::new(),
            persistence: TokenPricePersistence::new(&config),
            gema: GemaCalculator::new(config.gema_periods, config.round_duration),
            config,
        })
    }

    pub async fn run(mut self, health: &HealthSink) {
        let mut set = JoinSet::new();

        let sources = self.price_sources.take().unwrap();
        let price_maps: Vec<_> = sources
            .iter()
            .map(|s| (s.name.clone(), s.get_prices()))
            .collect();

        // Make each source update its price map asynchronously.
        for source in sources {
            let health = health.clone();
            set.spawn(source.run(health));
        }

        // Every second, we report the latest values from those price maps.
        set.spawn(async move {
            loop {
                self.update_previous_prices();
                self.report(&price_maps).await;
                sleep(Duration::from_secs(1)).await;
            }
        });

        while let Some(res) = set.join_next().await {
            if let Err(error) = res {
                error!("Panicked while querying data from an API! {:?}", error);
            }
        }
    }

    fn update_previous_prices(&mut self) {
        let new_payload = {
            let borrow = self.payload_source.borrow();
            if !borrow.has_changed() {
                return;
            }
            borrow.clone()
        };
        for TimestampedPayloadEntry { timestamp, entry } in new_payload.entries {
            if self
                .previous_prices
                .get(&entry.synthetic)
                .is_some_and(|e| e.timestamp > timestamp)
            {
                continue;
            }
            let data = entry.payload.data;
            let mut numerators = data.collateral_prices;
            let mut denominator = data.denominator;

            restrict_output_size(&mut numerators, &mut denominator, 512);

            let prices = numerators
                .into_iter()
                .map(|n| BigRational::new(n.into(), denominator.clone().into()))
                .collect();
            self.previous_prices
                .insert(entry.synthetic, PreviousPriceFeed { timestamp, prices });
        }
    }

    async fn report(&mut self, price_maps: &[(String, Arc<DashMap<String, PriceInfo>>)]) {
        let mut source_prices = vec![];
        for (source, price_map) in price_maps {
            for price in price_map.iter() {
                source_prices.push((source.clone(), price.value().clone()));
            }
        }

        let default_prices = self.persistence.saved_prices();
        let converter =
            TokenPriceConverter::new(&source_prices, &default_prices, &self.config.synthetics);

        let price_feeds = self
            .config
            .synthetics
            .iter()
            .map(|s| self.compute_payload(s, &converter))
            .collect();
        self.feed_sink.send_replace(price_feeds);

        let token_values = converter.token_prices();
        self.audit_sink.send_replace(token_values);

        self.persistence.save_prices(&converter).await;
    }

    fn compute_payload(
        &self,
        synth: &SyntheticConfig,
        converter: &TokenPriceConverter,
    ) -> PriceFeedEntry {
        let synth_digits = self.get_digits(&synth.name);
        let synth_price = converter.value_in_usd(&synth.name);

        let prices = synth
            .collateral
            .iter()
            .map(|c| {
                let collateral_digits = self.get_digits(c.as_str());
                let p = converter.value_in_usd(c.as_str());
                let p_scaled = p * BigInt::from(10i64.pow(collateral_digits))
                    / BigInt::from(10i64.pow(synth_digits));
                p_scaled / &synth_price
            })
            .collect();

        // apply GEMA smoothing to the prices we've found this round
        let prices = self.apply_gema(&synth.name, prices);

        // track metrics for the different prices
        for (collateral_name, collateral_price) in synth.collateral.iter().zip(prices.iter()) {
            let price = collateral_price.to_f64().expect("infallible");
            debug!(
                collateral_name,
                synthetic_name = synth.name,
                histogram.collateral_price = price,
                "price metrics",
            );
        }

        let (collateral_prices, denominator) = normalize(&prices);
        let valid_from = SystemTime::now() - Duration::from_secs(60);
        let valid_to = valid_from + Duration::from_secs(360);

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

    fn apply_gema(&self, synth: &str, prices: Vec<BigRational>) -> Vec<BigRational> {
        let now = SystemTime::now();
        let Some(prev_feed) = self.previous_prices.get(synth) else {
            // We don't have any previous set of prices to reference
            return prices;
        };
        let Ok(time_elapsed) = now.duration_since(prev_feed.timestamp) else {
            // someone's clock must be extremely messed up
            return prices;
        };
        self.gema.smooth(time_elapsed, &prev_feed.prices, prices)
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

fn normalize(prices: &[BigRational]) -> (Vec<BigUint>, BigUint) {
    assert!(prices.iter().all(|p| p.is_positive()));
    let denominator = prices.iter().fold(BigInt::one(), |acc, p| acc * p.denom());
    let normalized_numerators: Vec<_> = prices
        .iter()
        .map(|p| p.numer() * &denominator / p.denom())
        .collect();
    let gcd = normalized_numerators
        .iter()
        .fold(denominator.clone(), |acc, n| acc.gcd(n));
    let mut numerators: Vec<_> = normalized_numerators
        .into_iter()
        .map(|n| (n / &gcd).to_biguint().unwrap())
        .collect();
    let mut denominator = (denominator / gcd).to_biguint().unwrap();
    restrict_output_size(&mut numerators, &mut denominator, 1024);
    (numerators, denominator)
}

fn restrict_output_size(numerators: &mut [BigUint], denominator: &mut BigUint, max_bits: u64) {
    let bits = numerators
        .iter()
        .fold(denominator.bits(), |acc, n| n.bits().max(acc));

    if bits < max_bits {
        return;
    }
    let bits_to_clear = bits - max_bits;

    // when rounding the denominator, bias away from 0
    let mask_to_clear = (BigUint::one() << bits_to_clear) - BigUint::one();
    let cleared_any_bits = denominator.clone() & mask_to_clear != BigUint::ZERO;
    *denominator >>= bits_to_clear;
    if cleared_any_bits {
        *denominator |= BigUint::one();
    }

    // when rounding numerators, bias towards 0
    for numerator in numerators {
        *numerator >>= bits_to_clear;
    }
}

#[cfg(test)]
mod tests {
    use num_bigint::{BigInt, BigUint};
    use num_rational::BigRational;
    use num_traits::One as _;

    use super::{normalize, restrict_output_size};

    fn decimal_rational(value: u128, scale: u32) -> BigRational {
        let numer = BigInt::from(value);
        let denom = BigInt::from(10u128).pow(scale);
        BigRational::new(numer, denom)
    }

    #[test]
    fn normalize_should_not_panic_on_empty_input() {
        assert_eq!((vec![], BigUint::one()), normalize(&[]));
    }

    #[test]
    fn normalize_should_compute_gcd() {
        let prices = [decimal_rational(5526312, 7), decimal_rational(1325517, 6)];
        let (collateral_prices, denominator) = normalize(&prices);
        assert_eq!(
            (
                vec![2763156u128.into(), 6627585u128.into()],
                5000000u128.into()
            ),
            (collateral_prices, denominator)
        );
    }

    #[test]
    fn normalize_should_normalize_numbers_with_same_decimal_count() {
        let prices = [decimal_rational(1337, 3), decimal_rational(9001, 3)];
        let (collateral_prices, denominator) = normalize(&prices);
        assert_eq!(
            (vec![1337u128.into(), 9001u128.into()], 1000u128.into()),
            (collateral_prices, denominator)
        );
    }

    #[test]
    fn normalize_should_handle_decimals_with_different_scales() {
        let prices = [
            decimal_rational(2_000, 3),
            decimal_rational(4_000_000, 6),
            decimal_rational(6_000_000_000, 9),
        ];
        let (collateral_prices, denominator) = normalize(&prices);
        assert_eq!(
            (vec![2u128.into(), 4u128.into(), 6u128.into()], 1u128.into()),
            (collateral_prices, denominator)
        );
    }

    #[test]
    fn normalize_should_restrict_output_to_at_most_128_bytes() {
        let prices = [
            BigRational::new(BigInt::from(2u128), BigInt::one() << 1024),
            BigRational::new(BigInt::from(3u128), BigInt::one() << 1024),
            BigRational::new(BigInt::from(4u128), BigInt::one() << 1024),
        ];
        let (collateral_prices, denominator) = normalize(&prices);
        assert_eq!(
            (
                vec![BigUint::one(), BigUint::one(), 2u128.into()],
                BigUint::one() << 1023,
            ),
            (collateral_prices, denominator),
        );
    }

    #[test]
    fn restrict_output_size_should_round_numerator_down_and_denominator_up() {
        let mut numerators = vec![0b1001u128.into()];
        let mut denominator = 0b1001u128.into();
        restrict_output_size(&mut numerators, &mut denominator, 3);
        assert_eq!(numerators, vec![0b100u128.into()]);
        assert_eq!(denominator, 0b101u128.into());
    }

    #[test]
    fn restrict_output_size_should_not_round_denominator_up_when_only_zeroes_were_truncated() {
        let mut numerators = vec![0b1001u128.into()];
        let mut denominator = 0b1000u128.into();
        restrict_output_size(&mut numerators, &mut denominator, 3);
        assert_eq!(numerators, vec![0b100u128.into()]);
        assert_eq!(denominator, 0b100u128.into());
    }
}
