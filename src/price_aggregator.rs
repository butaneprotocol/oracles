use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Result, bail};
use dashmap::DashMap;
use gema::GemaCalculator;
use num_bigint::{BigInt, BigUint};
use num_integer::Integer as _;
use num_rational::BigRational;
use num_traits::{One as _, Signed as _, ToPrimitive as _};
use persistence::TokenPricePersistence;
use synth_config_source::SyntheticConfigSource;
use tokio::{sync::watch, task::JoinSet, time::sleep};
use tracing::{debug, error, warn};

use crate::{
    config::{OracleConfig, SyntheticConfig},
    health::{HealthSink, HealthStatus, Origin},
    price_feed::{
        GenericPriceFeed, IntervalBound, PriceData, SyntheticPriceData, SyntheticPriceFeed,
        Validity,
    },
    signature_aggregator::Payload,
    sources::{
        binance::BinanceSource,
        bybit::ByBitSource,
        coinbase::CoinbaseSource,
        crypto_com::CryptoComSource,
        cswap::CSwapSource,
        fxratesapi::FxRatesApiSource,
        kucoin::KucoinSource,
        maestro::MaestroSource,
        minswap::MinswapSource,
        okx::OkxSource,
        source::{PriceInfo, PriceInfoSnapshot},
        splash::SplashSource,
        sundaeswap::SundaeSwapSource,
        sundaeswap_kupo::SundaeSwapKupoSource,
        vyfi::VyFiSource,
        wingriders::WingRidersSource,
    },
};

pub use self::conversions::{TokenPrice, TokenPriceSource};
use self::{conversions::TokenPriceConverter, source_adapter::SourceAdapter};

mod conversions;
mod gema;
mod persistence;
mod source_adapter;
mod synth_config_source;
mod utils;

struct PreviousSyntheticPrices {
    timestamp: SystemTime,
    prices: Vec<BigRational>,
}

struct PreviousPrice {
    timestamp: SystemTime,
    price: BigUint,
}

pub struct PriceAggregator {
    price_sink: watch::Sender<PriceData>,
    audit_sink: watch::Sender<Vec<TokenPrice>>,
    payload_source: watch::Receiver<Payload>,
    price_sources: Option<Vec<SourceAdapter>>,
    synth_config_source: Option<SyntheticConfigSource>,
    previous_synth_prices: BTreeMap<String, PreviousSyntheticPrices>,
    previous_prices: BTreeMap<String, PreviousPrice>,
    persistence: TokenPricePersistence,
    gema: GemaCalculator,
    config: Arc<OracleConfig>,
}

impl PriceAggregator {
    pub fn new(
        price_sink: watch::Sender<PriceData>,
        audit_sink: watch::Sender<Vec<TokenPrice>>,
        payload_source: watch::Receiver<Payload>,
        config: Arc<OracleConfig>,
    ) -> Result<Self> {
        let mut sources = vec![
            SourceAdapter::new(BinanceSource::new(&config), &config),
            SourceAdapter::new(ByBitSource::new(&config), &config),
            SourceAdapter::new(CoinbaseSource::new(&config), &config),
            SourceAdapter::new(CryptoComSource::new(&config), &config),
            SourceAdapter::new(KucoinSource::new(&config), &config),
            SourceAdapter::new(OkxSource::new(&config)?, &config),
        ];
        
        // Add DEX sources only if configured
        if config.minswap.is_some() {
            sources.push(SourceAdapter::new(MinswapSource::new(&config)?, &config));
        }
        if config.splash.is_some() {
            sources.push(SourceAdapter::new(SplashSource::new(&config)?, &config));
        }
        if config.cswap.is_some() {
            match CSwapSource::new(&config) {
                Ok(cswap) => {
                    warn!("CSwap source initialized successfully");
                    sources.push(SourceAdapter::new(cswap, &config));
                }
                Err(e) => {
                    warn!("Failed to initialize CSwap source: {}", e);
                }
            }
        }
        if config.vyfi.is_some() {
            sources.push(SourceAdapter::new(VyFiSource::new(&config)?, &config));
        }
        if config.wingriders.is_some() {
            sources.push(SourceAdapter::new(WingRidersSource::new(&config)?, &config));
        }
        match MaestroSource::new(&config)? {
            Some(maestro_source) => {
                sources.push(SourceAdapter::new(maestro_source, &config));
            }
            _ => {
                warn!("Not querying maestro, because no MAESTRO_API_KEY was provided");
            }
        }
        match FxRatesApiSource::new(&config)? {
            Some(fxratesapi_source) => {
                sources.push(SourceAdapter::new(fxratesapi_source, &config));
            }
            _ => {
                warn!("Not querying FXRatesAPI, because no FXRATESAPI_API_KEY was provided");
            }
        }
        if let Some(sundaeswap_config) = &config.sundaeswap {
            if sundaeswap_config.use_api {
                sources.push(SourceAdapter::new(SundaeSwapSource::new(&config)?, &config));
            } else {
                sources.push(SourceAdapter::new(
                    SundaeSwapKupoSource::new(&config)?,
                    &config,
                ));
            }
        }
        Ok(Self {
            price_sink,
            audit_sink,
            payload_source,
            price_sources: Some(sources),
            synth_config_source: if config.enable_synthetics {
                Some(SyntheticConfigSource::new(&config)?)
            } else {
                None
            },
            previous_synth_prices: BTreeMap::new(),
            previous_prices: BTreeMap::new(),
            persistence: TokenPricePersistence::new(&config),
            gema: GemaCalculator::new(config.gema_periods, config.round_duration),
            config,
        })
    }

    pub async fn run(mut self, health: &HealthSink) {
        let mut set = JoinSet::new();

        let sources = self.price_sources.take().unwrap();
        let reporter = SourcePriceReporter::new(&sources);

        // Make each source update its price map asynchronously.
        for source in sources {
            let health = health.clone();
            set.spawn(source.run(health));
        }

        // Every second, we report the latest values from those price maps.
        let health = health.clone();
        set.spawn(async move {
            loop {
                self.update_previous_prices();
                if let Some(ref mut synth_config_source) = self.synth_config_source {
                    synth_config_source.refresh(&health).await;
                }
                self.report(&reporter.latest_source_prices(), &health).await;
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
        for entry in new_payload.synthetics {
            if self
                .previous_synth_prices
                .get(&entry.synthetic)
                .is_some_and(|e| e.timestamp > entry.timestamp)
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
            self.previous_synth_prices.insert(
                entry.synthetic,
                PreviousSyntheticPrices {
                    timestamp: entry.timestamp,
                    prices,
                },
            );
        }
        for entry in new_payload.generics {
            if self
                .previous_prices
                .get(&entry.name)
                .is_some_and(|e| e.timestamp > entry.timestamp)
            {
                continue;
            }
            self.previous_prices.insert(
                entry.name,
                PreviousPrice {
                    timestamp: entry.timestamp,
                    price: entry.payload.data.price,
                },
            );
        }
    }

    async fn report(&mut self, source_prices: &[(String, PriceInfo)], health: &HealthSink) {
        let default_prices = if self.config.use_persisted_prices {
            self.persistence.saved_prices()
        } else {
            vec![]
        };
        let converter = TokenPriceConverter::new(
            source_prices,
            &default_prices,
            &self.config.synthetics,
            &self.config.currencies,
            utils::decimal_to_rational(self.config.max_price_divergence),
        );

        let synthetics = self
            .config
            .synthetics
            .iter()
            .filter_map(|s| self.compute_synthetic_payload(s, &converter, health))
            .collect();
        let generics = self
            .config
            .feeds
            .currencies
            .iter()
            .flat_map(|c| {
                self.compute_generic_payload(c, &converter, health)
                    .into_iter()
            })
            .collect();
        self.price_sink.send_replace(PriceData {
            synthetics,
            generics,
        });

        let token_values = converter.token_prices();
        self.audit_sink.send_replace(token_values);

        self.persistence.save_prices(&converter).await;
    }

    fn compute_synthetic_payload(
        &self,
        synth: &SyntheticConfig,
        converter: &TokenPriceConverter,
        health: &HealthSink,
    ) -> Option<SyntheticPriceData> {
        let origin = Origin::Currency(synth.name.clone());
        match self.do_compute_synthetic_payload(synth, converter) {
            Ok(data) => {
                health.update(origin, HealthStatus::Healthy);
                Some(data)
            }
            Err(error) => {
                warn!("could not compute payload for {}: {error}", synth.name);
                health.update(origin, HealthStatus::Unhealthy(error.to_string()));
                None
            }
        }
    }

    fn do_compute_synthetic_payload(
        &self,
        synth: &SyntheticConfig,
        converter: &TokenPriceConverter,
    ) -> Result<SyntheticPriceData> {
        let synth_digits = self.get_digits(&synth.name);
        let Some(synth_price) = converter.value_in_usd(&synth.name) else {
            bail!("could not compute synthetic price");
        };
        let Some(ref synth_config_source) = self.synth_config_source else {
            bail!("synthetics are not enabled");
        };
        let Some(collateral) = synth_config_source.synthetic_collateral(&synth.name) else {
            bail!("could not retrieve synthetic config");
        };

        let mut prices = vec![];
        for collateral in &collateral {
            let collateral_digits = self.get_digits(collateral);
            let Some(p) = converter.value_in_usd(collateral) else {
                bail!("could not compute price for collateral {collateral}");
            };
            let p_scaled = p * BigInt::from(10i64.pow(synth_digits))
                / BigInt::from(10i64.pow(collateral_digits));
            prices.push(p_scaled / &synth_price);
        }

        // track prices before smoothing, to measure the effect of smoothing
        for (collateral_name, collateral_price) in collateral.iter().zip(prices.iter()) {
            let price = collateral_price.to_f64().expect("infallible");
            debug!(
                collateral_name,
                synthetic_name = synth.name,
                histogram.raw_collateral_price = price,
                "pre-smoothing price metrics",
            );
        }

        // apply GEMA smoothing to the prices we've found this round
        let prices = self.apply_synth_gema(&synth.name, prices);

        // track metrics for the different prices
        for (collateral_name, collateral_price) in collateral.iter().zip(prices.iter()) {
            let price = collateral_price.to_f64().expect("infallible");
            debug!(
                collateral_name,
                synthetic_name = synth.name,
                histogram.collateral_price = price,
                "price metrics",
            );
        }

        let (collateral_prices, denominator) = normalize(&prices, self.config.price_precision);
        let valid_from = SystemTime::now() - Duration::from_secs(60);
        let valid_to = valid_from + Duration::from_secs(360);

        Ok(SyntheticPriceData {
            price: synth_price,
            feed: SyntheticPriceFeed {
                collateral_names: Some(collateral),
                collateral_prices,
                synthetic: synth.name.clone(),
                denominator,
                validity: Validity {
                    lower_bound: IntervalBound::moment(valid_from, true),
                    upper_bound: IntervalBound::moment(valid_to, false),
                },
            },
        })
    }

    fn compute_generic_payload(
        &self,
        currency: &str,
        converter: &TokenPriceConverter,
        health: &HealthSink,
    ) -> Vec<GenericPriceFeed> {
        let origin = Origin::Currency(currency.to_string());
        let mut feeds = vec![];
        let now = SystemTime::now();
        let timestamp = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("it is after 1970")
            .as_millis() as u64;
        let to_int = |price: &BigRational| {
            let int_price: BigInt = (price.numer() * 10_000_000) / price.denom();
            int_price.to_biguint().unwrap()
        };

        let Some(price) = converter.value_in_usd(currency) else {
            health.update(
                origin,
                HealthStatus::Unhealthy("could not compute price".to_string()),
            );
            return feeds;
        };
        let token_usd = to_int(&price);
        let usd_token = to_int(&(BigRational::one() / &price));

        feeds.push(GenericPriceFeed {
            price: token_usd.clone(),
            name: format!("{currency}/USD#RAW"),
            timestamp,
        });
        feeds.push(GenericPriceFeed {
            price: usd_token.clone(),
            name: format!("USD/{currency}#RAW"),
            timestamp,
        });

        let token_usd_gema = format!("{currency}/USD#GEMA");
        feeds.push(GenericPriceFeed {
            price: self.apply_gema(&token_usd_gema, token_usd),
            name: token_usd_gema,
            timestamp,
        });

        let usd_token_gema_feed = format!("USD/{currency}#GEMA");
        feeds.push(GenericPriceFeed {
            price: self.apply_gema(&usd_token_gema_feed, usd_token),
            name: usd_token_gema_feed,
            timestamp,
        });

        health.update(origin, HealthStatus::Healthy);
        feeds
    }

    fn apply_gema(&self, feed: &str, price: BigUint) -> BigUint {
        let now = SystemTime::now();
        let Some(prev_feed) = self.previous_prices.get(feed) else {
            // We don't have any previous price to reference
            return price;
        };
        let Ok(time_elapsed) = now.duration_since(prev_feed.timestamp) else {
            // someone's clock must be extremely messed up
            return price;
        };
        self.gema
            .smooth_price(time_elapsed, &prev_feed.price, price)
    }

    fn apply_synth_gema(&self, synth: &str, prices: Vec<BigRational>) -> Vec<BigRational> {
        let now = SystemTime::now();
        let Some(prev_feed) = self.previous_synth_prices.get(synth) else {
            // We don't have any previous set of prices to reference
            return prices;
        };
        let Ok(time_elapsed) = now.duration_since(prev_feed.timestamp) else {
            // someone's clock must be extremely messed up
            return prices;
        };
        self.gema
            .smooth_synthetic_price(time_elapsed, &prev_feed.prices, prices)
    }

    fn get_digits(&self, currency: &str) -> u32 {
        if let Some(synth_config) = self.config.synthetics.iter().find(|s| s.name == currency) {
            return synth_config.digits;
        }
        let Some(config) = self.config.currencies.iter().find(|c| c.name == currency) else {
            panic!("Unrecognized currency {}", currency);
        };
        config.digits
    }
}

struct SourcePrices {
    source: String,
    max_age: Duration,
    price_map: Arc<DashMap<String, PriceInfoSnapshot>>,
}
struct SourcePriceReporter {
    price_maps: Vec<SourcePrices>,
}

impl SourcePriceReporter {
    fn new(sources: &[SourceAdapter]) -> Self {
        let price_maps = sources
            .iter()
            .map(|s| SourcePrices {
                source: s.name.clone(),
                max_age: s.max_time_without_updates,
                price_map: s.get_prices(),
            })
            .collect();
        Self { price_maps }
    }

    #[tracing::instrument(skip_all)]
    fn latest_source_prices(&self) -> Vec<(String, PriceInfo)> {
        let now = Instant::now();
        let mut source_prices = vec![];
        for SourcePrices {
            source,
            max_age,
            price_map,
        } in &self.price_maps
        {
            for price in price_map.iter() {
                let price_age = now - price.as_of;
                let price_age_in_millis = u64::try_from(price_age.as_millis()).unwrap();
                debug!(
                    histogram.source_price_age = price_age_in_millis,
                    source,
                    token = price.info.token,
                    unit = price.info.unit,
                    "source price age metrics"
                );
                if price_age > *max_age {
                    let max_age_in_millis = u64::try_from(max_age.as_millis()).unwrap();
                    warn!(
                        price_age_in_millis,
                        max_age_in_millis,
                        source,
                        token = price.info.token,
                        unit = price.info.unit,
                        "ignoring price from source because it is too old"
                    );
                    continue;
                }
                let source_name = match &price.name {
                    Some(name) => format!("{} ({})", source, name),
                    None => source.clone(),
                };
                source_prices.push((source_name, price.info.clone()));
            }
        }
        source_prices
    }
}

fn normalize(prices: &[BigRational], max_bits: u64) -> (Vec<BigUint>, BigUint) {
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
    restrict_output_size(&mut numerators, &mut denominator, max_bits);
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
        assert_eq!((vec![], BigUint::one()), normalize(&[], 1024));
    }

    #[test]
    fn normalize_should_compute_gcd() {
        let prices = [decimal_rational(5526312, 7), decimal_rational(1325517, 6)];
        let (collateral_prices, denominator) = normalize(&prices, 1024);
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
        let (collateral_prices, denominator) = normalize(&prices, 1024);
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
        let (collateral_prices, denominator) = normalize(&prices, 1024);
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
        let (collateral_prices, denominator) = normalize(&prices, 1024);
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
