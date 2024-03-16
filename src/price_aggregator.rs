use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::Result;
use dashmap::DashMap;
use num_bigint::BigUint;
use num_integer::Integer;
use rust_decimal::Decimal;
use tokio::{
    sync::{mpsc::unbounded_channel, watch::Sender},
    task::JoinSet,
    time::{sleep, Instant},
};
use tracing::warn;

use crate::{
    apis::{
        binance::BinanceSource,
        coinbase::CoinbaseSource,
        maestro::MaestroSource,
        source::{Origin, PriceInfo, Source},
    },
    config::{CollateralConfig, Config, SyntheticConfig},
    health::{HealthSink, HealthStatus},
};

struct SourceAdapter {
    origin: Origin,
    source: Box<dyn Source + Send + Sync>,
    prices: Arc<DashMap<String, PriceInfo>>,
}

impl SourceAdapter {
    pub fn new<T: Source + Send + Sync + 'static>(source: T) -> Self {
        Self {
            origin: source.origin(),
            source: Box::new(source),
            prices: Arc::new(DashMap::new()),
        }
    }

    pub fn get_prices(&self) -> Arc<DashMap<String, PriceInfo>> {
        self.prices.clone()
    }

    pub async fn run(self, health: HealthSink) {
        let mut set = JoinSet::new();

        let (tx, mut rx) = unbounded_channel();

        // track when each token was updated
        let last_updated = Arc::new(DashMap::new());
        for token in self.source.tokens() {
            last_updated.insert(token, None);
        }

        let source = self.source;
        set.spawn(async move {
            // read values from the source
            loop {
                if let Err(error) = source.query(&tx).await {
                    warn!(
                        "Error occurred while querying {:?}, retrying: {}",
                        self.origin, error
                    );
                }
            }
        });

        let updated = last_updated.clone();
        let prices = self.prices.clone();
        set.spawn(async move {
            // write emitted values into our map
            while let Some(info) = rx.recv().await {
                updated.insert(info.token.clone(), Some(Instant::now()));
                prices.insert(info.token.clone(), info);
            }
        });

        // Check how long it's been since we updated prices
        // Mark ourself as unhealthy if any prices are too old.
        set.spawn(async move {
            loop {
                sleep(Duration::from_secs(30)).await;
                let now = Instant::now();
                let mut missing_updates = vec![];
                for update_times in last_updated.iter() {
                    let too_long_without_update = update_times
                        .value()
                        .map_or(true, |v| now - v >= Duration::from_secs(30));
                    if too_long_without_update {
                        missing_updates.push(update_times.key().clone());
                    }
                }

                let status = if missing_updates.is_empty() {
                    HealthStatus::Healthy
                } else {
                    HealthStatus::Unhealthy(format!(
                        "Went more than 30 seconds without updates for {:?}",
                        missing_updates
                    ))
                };
                health.update(crate::health::Origin::Source(self.origin), status);
            }
        });

        while let Some(res) = set.join_next().await {
            if let Err(error) = res {
                warn!("{}", error);
            }
        }
    }
}

pub struct PriceAggregator {
    tx: Arc<Sender<Vec<PriceFeed>>>,
    sources: Option<Vec<SourceAdapter>>,
    config: Arc<Config>,
}

impl PriceAggregator {
    pub fn new(tx: Sender<Vec<PriceFeed>>, config: Arc<Config>) -> Result<Self> {
        Ok(Self {
            tx: Arc::new(tx),
            sources: Some(vec![
                SourceAdapter::new(BinanceSource::new()),
                SourceAdapter::new(CoinbaseSource::new()),
                SourceAdapter::new(MaestroSource::new()?),
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
    ) -> PriceFeed {
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
        PriceFeed {
            collateral_prices,
            price,
            synthetic: synth.name.clone(),
            denominator,
        }
    }

    fn get_collateral(&self, collateral: &str) -> &CollateralConfig {
        let Some(config) = self.config.collateral.iter().find(|c| c.name == collateral) else {
            panic!("Unrecognized collateral {}", collateral);
        };
        config
    }
}

#[derive(Eq, Hash, PartialEq)]
struct CurrencyPair<'a>(&'a str, &'a str);
struct ConversionLookup<'a> {
    conversions: HashMap<CurrencyPair<'a>, Decimal>,
    defaults: HashMap<&'a str, Decimal>,
}

impl<'a> ConversionLookup<'a> {
    pub fn new(prices: &'a [PriceInfo], config: &'a Arc<Config>) -> Self {
        let mut conversions = HashMap::new();

        // Set our default prices
        let mut defaults = HashMap::new();
        for synth in &config.synthetics {
            defaults.insert(synth.name.as_str(), synth.price);
        }
        for coll in &config.collateral {
            defaults.insert(coll.name.as_str(), coll.price);
        }

        let mut aggregated_prices: HashMap<_, Vec<Decimal>> = HashMap::new();
        for price in prices {
            aggregated_prices
                .entry(CurrencyPair(&price.token, &price.unit))
                .and_modify(|e| e.push(price.value))
                .or_insert(vec![price.value]);
        }
        for (currencies, prices) in aggregated_prices {
            let average_price = prices.iter().fold(Decimal::ZERO, |acc, el| acc + *el)
                / Decimal::new(prices.len() as i64, 0);
            conversions.insert(currencies, average_price);
        }

        Self {
            conversions,
            defaults,
        }
    }

    pub fn value_in_usd(&self, token: &str) -> Decimal {
        if token == "USD" {
            return Decimal::ONE;
        }
        // If we have a direct price per unit, return that.
        if let Some(usd_per_token) = self._get(token, "USD") {
            return usd_per_token;
        }
        // Lots of prices are stored in ada, try converting through that
        if token != "ADA" {
            if let Some(ada_per_token) = self._get(token, "ADA") {
                return ada_per_token * self.value_in_usd("ADA");
            }
        }

        *self
            .defaults
            .get(token)
            .unwrap_or_else(|| panic!("No price found for {}", token))
    }

    fn _get(&self, token: &str, unit: &str) -> Option<Decimal> {
        return self.conversions.get(&CurrencyPair(token, unit)).cloned();
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
    use std::sync::Arc;

    use rust_decimal::Decimal;

    use crate::{
        apis::source::PriceInfo,
        config::{CollateralConfig, Config, SyntheticConfig},
    };

    use super::{normalize, ConversionLookup};

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

    fn make_config() -> Arc<Config> {
        Arc::new(Config {
            synthetics: vec![
                SyntheticConfig {
                    name: "USDb".into(),
                    price: Decimal::ONE,
                    digits: 6,
                    collateral: vec![],
                },
                SyntheticConfig {
                    name: "BTCb".into(),
                    price: Decimal::new(50000, 0),
                    digits: 8,
                    collateral: vec![],
                },
            ],
            collateral: vec![
                CollateralConfig {
                    name: "ADA".into(),
                    price: Decimal::new(6, 1),
                    digits: 6,
                },
                CollateralConfig {
                    name: "LENFI".into(),
                    price: Decimal::new(379, 2),
                    digits: 6,
                },
            ],
        })
    }

    #[test]
    fn value_in_usd_should_return_defaults() {
        let config = make_config();
        let prices = vec![];
        let lookup = ConversionLookup::new(&prices, &config);

        assert_eq!(lookup.value_in_usd("ADA"), Decimal::new(6, 1));
    }

    #[test]
    fn value_in_usd_should_return_value_from_source() {
        let config = make_config();
        let prices = vec![PriceInfo {
            token: "BTCb".into(),
            unit: "USD".into(),
            value: Decimal::new(60000, 0),
        }];
        let lookup = ConversionLookup::new(&prices, &config);

        assert_eq!(lookup.value_in_usd("BTCb"), Decimal::new(60000, 0));
    }

    #[test]
    fn value_in_usd_should_average_prices() {
        let config = make_config();
        let prices = vec![
            PriceInfo {
                token: "BTCb".into(),
                unit: "USD".into(),
                value: Decimal::new(70000, 0),
            },
            PriceInfo {
                token: "BTCb".into(),
                unit: "USD".into(),
                value: Decimal::new(80000, 0),
            },
        ];
        let lookup = ConversionLookup::new(&prices, &config);

        assert_eq!(lookup.value_in_usd("BTCb"), Decimal::new(75000, 0));
    }

    #[test]
    fn value_in_usd_should_convert_prices_in_ada_using_default_ada_price() {
        let config = make_config();
        let prices = vec![PriceInfo {
            token: "LENFI".into(),
            unit: "ADA".into(),
            value: Decimal::new(10000, 0),
        }];
        let lookup = ConversionLookup::new(&prices, &config);

        assert_eq!(lookup.value_in_usd("LENFI"), Decimal::new(6000, 0));
    }

    #[test]
    fn value_in_usd_should_convert_prices_in_ada_using_ada_price_from_api() {
        let config = make_config();
        let prices = vec![
            PriceInfo {
                token: "ADA".into(),
                unit: "USD".into(),
                value: Decimal::new(3, 1),
            },
            PriceInfo {
                token: "LENFI".into(),
                unit: "ADA".into(),
                value: Decimal::new(10000, 0),
            },
        ];
        let lookup = ConversionLookup::new(&prices, &config);

        assert_eq!(lookup.value_in_usd("LENFI"), Decimal::new(3000, 0));
    }
}
