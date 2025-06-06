use std::collections::BTreeMap;

use itertools::Itertools;
use num_bigint::BigInt;
use num_rational::BigRational;
use num_traits::{Inv, One, Signed, ToPrimitive, Zero};
use serde::Serialize;
use tracing::warn;

use crate::{
    config::{CurrencyConfig, SyntheticConfig},
    sources::source::PriceInfo,
};

use super::utils;

#[derive(Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct TokenPair<'a>(&'a str, &'a str);

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TokenPrice {
    pub token: String,
    pub unit: String,
    #[serde(serialize_with = "utils::serialize_rational_as_decimal")]
    pub value: BigRational,
    pub sources: Vec<TokenPriceSource>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TokenPriceSource {
    pub name: String,
    #[serde(serialize_with = "utils::serialize_rational_as_decimal")]
    pub value: BigRational,
    #[serde(serialize_with = "utils::serialize_rational_as_decimal")]
    pub reliability: BigRational,
}

pub struct TokenPriceConverter<'a> {
    prices: BTreeMap<&'a str, Vec<TokenPrice>>,
    synthetics: BTreeMap<&'a str, &'a SyntheticConfig>,
    min_tvls: BTreeMap<&'a str, BigRational>,
    threshold: BigRational,
}

impl<'a> TokenPriceConverter<'a> {
    pub fn new(
        source_prices: &'a [(String, PriceInfo)],
        default_prices: &'a [TokenPrice],
        synthetics: &'a [SyntheticConfig],
        currencies: &'a [CurrencyConfig],
        max_price_divergence: BigRational,
    ) -> Self {
        let synthetics = synthetics.iter().map(|s| (s.name.as_str(), s)).collect();
        let min_tvls = currencies
            .iter()
            .map(|c| (c.name.as_str(), utils::decimal_to_rational(c.min_tvl)))
            .collect();

        let mut value_sources = BTreeMap::new();
        for (source_name, price) in source_prices {
            let source = TokenPriceSource {
                name: source_name.to_string(),
                value: utils::decimal_to_rational(price.value),
                reliability: utils::decimal_to_rational(price.reliability),
            };
            value_sources
                .entry(TokenPair(&price.token, &price.unit))
                .or_insert(vec![])
                .push(source);
        }

        let mut prices = BTreeMap::new();
        for (tokens, sources) in value_sources {
            let mut value_sum = BigRational::zero();
            let mut reliability_sum = BigRational::zero();
            for source in &sources {
                value_sum += &source.value * &source.reliability;
                reliability_sum += &source.reliability;
            }
            let value = TokenPrice {
                token: tokens.0.to_string(),
                unit: tokens.1.to_string(),
                value: value_sum / reliability_sum,
                sources,
            };
            prices.entry(tokens.0).or_insert(vec![]).push(value.clone());
        }

        // use defaults for anything we don't have a price for
        for price in default_prices {
            prices.entry(&price.token).or_insert(vec![price.clone()]);
        }

        Self {
            prices,
            synthetics,
            min_tvls,
            threshold: max_price_divergence,
        }
    }

    pub fn value_in_usd(&self, token: &str) -> Option<BigRational> {
        if token == "USD" {
            return Some(BigRational::one());
        }

        // A synthetic has the same value as its backing currency
        if let Some(synthetic) = self.synthetics.get(token) {
            return self.synthetic_value_in_usd(synthetic);
        }

        let prices = self.prices.get(token).into_iter().flat_map(|p| p.iter());

        let min_tvl = self
            .min_tvls
            .get(token)
            .unwrap_or_else(|| panic!("Unrecognized currency {token}"));

        let mut candidate_prices = vec![];
        for price in prices {
            let Some(conversion_factor) = self.value_in_usd(&price.unit) else {
                continue;
            };
            for source in &price.sources {
                let normalized_reliability = &source.reliability * &conversion_factor;
                if &normalized_reliability < min_tvl {
                    continue;
                }
                candidate_prices.push((
                    &source.name,
                    &source.value * &conversion_factor,
                    normalized_reliability,
                ));
            }
        }

        // Find the weighted median of all prices being considered
        let total_weight: BigRational = candidate_prices.iter().map(|(_, _, weight)| weight).sum();
        let median_price = find_weighted_median(&candidate_prices)?;

        // Filter out "outlier" prices which are too distant from the median
        let max_divergence = &median_price * &self.threshold;
        candidate_prices.retain(|(source, price, _)| {
            let divergence = (price - &median_price).abs();
            if divergence > max_divergence {
                let median_price = median_price.to_f64().expect("infallible");
                let source_price = price.to_f64().expect("infallible");
                warn!(
                    token,
                    source, median_price, source_price, "Ignoring outlier price"
                );
                false
            } else {
                true
            }
        });

        // if more than half of our prices by weight are outliers, these prices are too unstable to use
        let remaining_weight: BigRational =
            candidate_prices.iter().map(|(_, _, weight)| weight).sum();
        if remaining_weight < total_weight / BigRational::new(BigInt::from(2), BigInt::one()) {
            return None;
        }

        // otherwise, use the weighted average of every "normal" price
        let mut total_value = BigRational::zero();
        let mut total_reliability = BigRational::zero();
        for (_, value, reliability) in candidate_prices {
            total_value += value * &reliability;
            total_reliability += reliability;
        }

        Some(total_value / total_reliability)
    }

    fn synthetic_value_in_usd(&self, synthetic: &SyntheticConfig) -> Option<BigRational> {
        let mut values: Vec<BigRational> = synthetic
            .backing_currencies
            .iter()
            .filter_map(|backing| self.value_in_usd(backing))
            .collect();
        assert!(
            values.len() <= 3,
            "No decision on how to handle synthetics with >3 backing currencies"
        );
        if values.len() < 2 {
            // We have at most one value, report that
            let value = values.first()?;
            return Some(if synthetic.invert {
                value.inv()
            } else {
                value.clone()
            });
        }
        values.sort();

        // Find the average of every value we're considering.
        let average = values.iter().sum::<BigRational>() / BigInt::from(values.len() as u64);

        let max_divergence = values.iter().map(|v| find_divergence(v, &average)).max()?;
        let value = if max_divergence < self.threshold {
            // All values are close enough, so return their average
            average
        } else if let Ok([first, second, third]) = TryInto::<[BigRational; 3]>::try_into(values) {
            // If any two values are close enough together, return the average of those two
            if find_divergence(&first, &second) < self.threshold {
                (first + second) / BigInt::from(2)
            } else if find_divergence(&second, &third) < self.threshold {
                (second + third) / BigInt::from(2)
            } else {
                // If they all diverge, take the median price
                second
            }
        } else {
            // Guess we can't decide on a price
            return None;
        };
        Some(if synthetic.invert { value.inv() } else { value })
    }

    pub fn token_prices(&self) -> Vec<TokenPrice> {
        self.prices.values().flatten().cloned().collect()
    }
}

fn find_divergence(v1: &BigRational, v2: &BigRational) -> BigRational {
    (v1 - v2).abs() / v1.min(v2)
}

fn find_weighted_median(prices: &[(&String, BigRational, BigRational)]) -> Option<BigRational> {
    let sorted_prices: Vec<_> = prices.iter().sorted_by_key(|(_, price, _)| price).collect();
    let two = BigRational::new(BigInt::from(2), BigInt::one());

    let total_weight: BigRational = sorted_prices.iter().map(|(_, _, weight)| weight).sum();
    let threshold_weight = total_weight / &two;

    let mut seen_weight = BigRational::zero();
    for (index, (_, price, weight)) in sorted_prices.iter().enumerate() {
        seen_weight += weight;
        if seen_weight == threshold_weight {
            let next_price = &sorted_prices[index + 1].1;
            return Some((price + next_price) / two);
        }
        if seen_weight > threshold_weight {
            return Some(price.clone());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use num_bigint::BigInt;
    use num_rational::BigRational;
    use num_traits::One as _;
    use rust_decimal::Decimal;

    use crate::{
        config::{CollateralConfig, CurrencyConfig, SyntheticConfig},
        price_aggregator::{TokenPrice, TokenPriceSource},
        sources::source::PriceInfo,
    };

    use super::{TokenPriceConverter, find_weighted_median};

    fn decimal_rational(value: u64, scale: u32) -> BigRational {
        let numer = BigInt::from(value);
        let denom = BigInt::from(10u64.pow(scale));
        BigRational::new(numer, denom)
    }

    fn simple_rational(numer: u64, denom: u64) -> BigRational {
        BigRational::new(BigInt::from(numer), BigInt::from(denom))
    }

    fn default_threshold() -> BigRational {
        simple_rational(1, 10)
    }

    fn make_default_price(token: &str, value: BigRational) -> TokenPrice {
        TokenPrice {
            token: token.into(),
            unit: "USD".into(),
            value: value.clone(),
            sources: vec![TokenPriceSource {
                name: "Hard-coded default value".into(),
                value,
                reliability: BigRational::one(),
            }],
        }
    }

    const MULTIFEED_BACKING_CURRENCIES: usize = 3;

    fn make_synthetics() -> Vec<SyntheticConfig> {
        vec![
            SyntheticConfig {
                name: "USDb".into(),
                backing_currencies: vec!["USD".into()],
                invert: false,
                digits: 6,
                collateral: CollateralConfig::List(vec![]),
            },
            SyntheticConfig {
                name: "BTCb".into(),
                backing_currencies: vec!["BTC".into()],
                invert: false,
                digits: 8,
                collateral: CollateralConfig::List(vec![]),
            },
            SyntheticConfig {
                name: "SOLp".into(),
                backing_currencies: vec!["SOL".into()],
                invert: true,
                digits: 9,
                collateral: CollateralConfig::List(vec![]),
            },
            SyntheticConfig {
                name: "MULTI".into(),
                backing_currencies: (0..MULTIFEED_BACKING_CURRENCIES)
                    .map(|i| format!("COL{i}"))
                    .collect(),
                invert: false,
                digits: 6,
                collateral: CollateralConfig::List(vec![]),
            },
        ]
    }

    fn make_currencies() -> Vec<CurrencyConfig> {
        ["ADA", "BTC", "LENFI", "USDT", "SOL", "COL0", "COL1", "COL2"]
            .into_iter()
            .map(|name| CurrencyConfig {
                name: name.into(),
                asset_id: None,
                digits: 0,
                min_tvl: Decimal::new(0, 0),
            })
            .collect()
    }

    fn make_default_prices() -> Vec<TokenPrice> {
        vec![
            make_default_price("ADA", decimal_rational(6, 1)),
            make_default_price("BTC", decimal_rational(58262, 0)),
            make_default_price("LENFI", decimal_rational(379, 2)),
            make_default_price("USDT", BigRational::one()),
        ]
    }

    #[test]
    fn find_weighted_median_should_return_none_for_empty_list() {
        let prices = [];
        let median = find_weighted_median(&prices);
        assert_eq!(median, None);
    }

    #[test]
    fn find_weighted_median_should_return_lone_element() {
        let source = String::new();
        let prices = [(&source, decimal_rational(1337, 0), decimal_rational(1, 0))];
        let median = find_weighted_median(&prices);
        assert_eq!(median, Some(decimal_rational(1337, 0)));
    }

    #[test]
    fn find_weighted_median_should_return_middle_element_by_weight() {
        let source = String::new();
        let prices = [
            (&source, decimal_rational(1, 0), decimal_rational(2, 0)),
            (&source, decimal_rational(2, 0), decimal_rational(1, 0)),
            (&source, decimal_rational(3, 0), decimal_rational(1, 0)),
            (&source, decimal_rational(4, 0), decimal_rational(1, 0)),
        ];
        let median = find_weighted_median(&prices);
        assert_eq!(median, Some(decimal_rational(2, 0)));
    }

    #[test]
    fn find_weighted_median_should_return_mean_on_ties() {
        let source = String::new();
        let prices = [
            (&source, decimal_rational(1, 0), decimal_rational(1, 0)),
            (&source, decimal_rational(2, 0), decimal_rational(1, 0)),
            (&source, decimal_rational(3, 0), decimal_rational(1, 0)),
            (&source, decimal_rational(4, 0), decimal_rational(1, 0)),
        ];
        let median = find_weighted_median(&prices);
        assert_eq!(median, Some(decimal_rational(25, 1)));
    }

    #[test]
    fn find_weighted_median_should_not_require_sorted_input() {
        let source = String::new();
        let prices = [
            (&source, decimal_rational(3, 0), decimal_rational(1, 0)),
            (&source, decimal_rational(4, 0), decimal_rational(1, 0)),
            (&source, decimal_rational(2, 0), decimal_rational(1, 0)),
            (&source, decimal_rational(1, 0), decimal_rational(1, 0)),
        ];
        let median = find_weighted_median(&prices);
        assert_eq!(median, Some(decimal_rational(25, 1)));
    }

    #[test]
    fn value_in_usd_should_return_1_for_usd() {
        let source_prices = vec![];
        let default_prices = vec![];
        let synthetics = make_synthetics();
        let currencies = make_currencies();
        let converter = TokenPriceConverter::new(
            &source_prices,
            &default_prices,
            &synthetics,
            &currencies,
            default_threshold(),
        );

        assert_eq!(converter.value_in_usd("USD"), Some(BigRational::one()));
    }

    #[test]
    fn value_in_usd_should_return_defaults() {
        let source_prices = vec![];
        let default_prices = make_default_prices();
        let synthetics = make_synthetics();
        let currencies = make_currencies();
        let converter = TokenPriceConverter::new(
            &source_prices,
            &default_prices,
            &synthetics,
            &currencies,
            default_threshold(),
        );

        assert_eq!(converter.value_in_usd("ADA"), Some(decimal_rational(6, 1)));
    }

    #[test]
    fn value_in_usd_should_return_none_when_values_are_missing() {
        let source_prices = vec![];
        let default_prices = vec![];
        let synthetics = make_synthetics();
        let currencies = make_currencies();
        let converter = TokenPriceConverter::new(
            &source_prices,
            &default_prices,
            &synthetics,
            &currencies,
            default_threshold(),
        );

        assert_eq!(converter.value_in_usd("ADA"), None);
    }

    #[test]
    fn value_in_usd_should_return_value_from_source() {
        let source_prices = vec![(
            "Source".into(),
            PriceInfo {
                token: "BTC".into(),
                unit: "USD".into(),
                value: Decimal::new(60000, 0),
                reliability: Decimal::ONE,
            },
        )];
        let default_prices = vec![];
        let synthetics = make_synthetics();
        let currencies = make_currencies();
        let converter = TokenPriceConverter::new(
            &source_prices,
            &default_prices,
            &synthetics,
            &currencies,
            default_threshold(),
        );

        assert_eq!(
            converter.value_in_usd("BTC"),
            Some(decimal_rational(60000, 0))
        );
    }

    #[test]
    fn value_in_usd_should_average_prices() {
        let source_prices = vec![
            (
                "Word on the street".into(),
                PriceInfo {
                    token: "BTC".into(),
                    unit: "USD".into(),
                    value: Decimal::new(70000, 0),
                    reliability: Decimal::ONE,
                },
            ),
            (
                "My gut".into(),
                PriceInfo {
                    token: "BTC".into(),
                    unit: "USD".into(),
                    value: Decimal::new(80000, 0),
                    reliability: Decimal::ONE,
                },
            ),
        ];
        let default_prices = vec![];
        let synthetics = make_synthetics();
        let currencies = make_currencies();
        let converter = TokenPriceConverter::new(
            &source_prices,
            &default_prices,
            &synthetics,
            &currencies,
            default_threshold(),
        );

        assert_eq!(
            converter.value_in_usd("BTC"),
            Some(decimal_rational(75000, 0))
        );
    }

    #[test]
    fn value_in_usd_should_weight_prices() {
        let source_prices = vec![
            (
                "Vibes".into(),
                PriceInfo {
                    token: "BTC".into(),
                    unit: "USD".into(),
                    value: Decimal::new(97, 0),
                    reliability: Decimal::ONE,
                },
            ),
            (
                "My uncle".into(),
                PriceInfo {
                    token: "BTC".into(),
                    unit: "USD".into(),
                    value: Decimal::new(101, 0),
                    reliability: Decimal::new(3, 0),
                },
            ),
        ];
        let default_prices = vec![];
        let synthetics = make_synthetics();
        let currencies = make_currencies();
        let converter = TokenPriceConverter::new(
            &source_prices,
            &default_prices,
            &synthetics,
            &currencies,
            default_threshold(),
        );

        assert_eq!(
            converter.value_in_usd("BTC"),
            Some(decimal_rational(100, 0))
        );
    }

    #[test]
    fn value_in_usd_should_convert_prices_in_ada_using_default_ada_price() {
        let source_prices = vec![(
            "someone".into(),
            PriceInfo {
                token: "LENFI".into(),
                unit: "ADA".into(),
                value: Decimal::new(10000, 0),
                reliability: Decimal::ONE,
            },
        )];
        let default_prices = make_default_prices();
        let synthetics = make_synthetics();
        let currencies = make_currencies();
        let converter = TokenPriceConverter::new(
            &source_prices,
            &default_prices,
            &synthetics,
            &currencies,
            default_threshold(),
        );

        assert_eq!(
            converter.value_in_usd("LENFI"),
            Some(decimal_rational(6000, 0))
        );
    }

    #[test]
    fn value_in_usd_should_not_convert_prices_in_ada_when_ada_price_not_known() {
        let source_prices = vec![(
            "someone".into(),
            PriceInfo {
                token: "LENFI".into(),
                unit: "ADA".into(),
                value: Decimal::new(10000, 0),
                reliability: Decimal::ONE,
            },
        )];
        let default_prices = vec![];
        let synthetics = make_synthetics();
        let currencies = make_currencies();
        let converter = TokenPriceConverter::new(
            &source_prices,
            &default_prices,
            &synthetics,
            &currencies,
            default_threshold(),
        );

        assert_eq!(converter.value_in_usd("LENFI"), None);
    }

    #[test]
    fn value_in_usd_should_convert_prices_in_ada_using_ada_price_from_api() {
        let source_prices = vec![
            (
                "price for ada".into(),
                PriceInfo {
                    token: "ADA".into(),
                    unit: "USD".into(),
                    value: Decimal::new(3, 1),
                    reliability: Decimal::ONE,
                },
            ),
            (
                "price for anything else".into(),
                PriceInfo {
                    token: "LENFI".into(),
                    unit: "ADA".into(),
                    value: Decimal::new(10000, 0),
                    reliability: Decimal::ONE,
                },
            ),
        ];
        let default_prices = vec![];
        let synthetics = make_synthetics();
        let currencies = make_currencies();
        let converter = TokenPriceConverter::new(
            &source_prices,
            &default_prices,
            &synthetics,
            &currencies,
            default_threshold(),
        );

        assert_eq!(
            converter.value_in_usd("LENFI"),
            Some(decimal_rational(3000, 0))
        );
    }

    #[test]
    fn value_in_usd_should_convert_prices_in_usdt_using_default_usdt_price() {
        let source_prices = vec![(
            "someone".into(),
            PriceInfo {
                token: "BTC".into(),
                unit: "USDT".into(),
                value: Decimal::new(5000, 0),
                reliability: Decimal::ONE,
            },
        )];
        let default_prices = make_default_prices();
        let synthetics = make_synthetics();
        let currencies = make_currencies();
        let converter = TokenPriceConverter::new(
            &source_prices,
            &default_prices,
            &synthetics,
            &currencies,
            default_threshold(),
        );

        assert_eq!(
            converter.value_in_usd("BTC"),
            Some(decimal_rational(5000, 0))
        );
    }

    #[test]
    fn value_in_usd_should_not_convert_prices_in_usdt_when_usdt_price_not_known() {
        let source_prices = vec![(
            "someone".into(),
            PriceInfo {
                token: "BTC".into(),
                unit: "USDT".into(),
                value: Decimal::new(5000, 0),
                reliability: Decimal::ONE,
            },
        )];
        let default_prices = vec![];
        let synthetics = make_synthetics();
        let currencies = make_currencies();
        let converter = TokenPriceConverter::new(
            &source_prices,
            &default_prices,
            &synthetics,
            &currencies,
            default_threshold(),
        );

        assert_eq!(converter.value_in_usd("BTC"), None);
    }

    #[test]
    fn value_in_usd_should_convert_prices_in_usdt_using_usdt_price_from_api() {
        let source_prices = vec![
            (
                "price for usdt".into(),
                PriceInfo {
                    token: "USDT".into(),
                    unit: "USD".into(),
                    value: Decimal::new(1005, 3),
                    reliability: Decimal::ONE,
                },
            ),
            (
                "price for anything else".into(),
                PriceInfo {
                    token: "BTC".into(),
                    unit: "USDT".into(),
                    value: Decimal::new(5000, 0),
                    reliability: Decimal::ONE,
                },
            ),
        ];
        let default_prices = make_default_prices();
        let synthetics = make_synthetics();
        let currencies = make_currencies();
        let converter = TokenPriceConverter::new(
            &source_prices,
            &default_prices,
            &synthetics,
            &currencies,
            default_threshold(),
        );

        assert_eq!(
            converter.value_in_usd("BTC"),
            Some(decimal_rational(5025, 0))
        );
    }

    #[test]
    fn value_in_usd_should_average_prices_in_different_currencies() {
        let source_prices = vec![
            (
                "price for usdt".into(),
                PriceInfo {
                    token: "USDT".into(),
                    unit: "USD".into(),
                    value: Decimal::new(5, 1),
                    reliability: Decimal::ONE,
                },
            ),
            (
                "price for BTC in USD".into(),
                PriceInfo {
                    token: "BTC".into(),
                    unit: "USD".into(),
                    value: Decimal::new(4995, 0),
                    reliability: Decimal::ONE,
                },
            ),
            (
                "price for BTC in USDT".into(),
                PriceInfo {
                    token: "BTC".into(),
                    unit: "USDT".into(),
                    value: Decimal::new(10010, 0),
                    reliability: Decimal::TWO,
                },
            ),
        ];
        let default_prices = vec![];
        let synthetics = make_synthetics();
        let currencies = make_currencies();
        let converter = TokenPriceConverter::new(
            &source_prices,
            &default_prices,
            &synthetics,
            &currencies,
            default_threshold(),
        );

        assert_eq!(
            converter.value_in_usd("BTC"),
            Some(decimal_rational(5000, 0))
        );
    }

    #[test]
    fn value_in_usd_should_return_value_of_underlying_currency_for_synthetics() {
        let source_prices = vec![(
            "price for BTC".into(),
            PriceInfo {
                token: "BTC".into(),
                unit: "USD".into(),
                value: Decimal::new(9001, 0),
                reliability: Decimal::ONE,
            },
        )];
        let default_prices = vec![];
        let synthetics = make_synthetics();
        let currencies = make_currencies();
        let converter = TokenPriceConverter::new(
            &source_prices,
            &default_prices,
            &synthetics,
            &currencies,
            default_threshold(),
        );

        assert_eq!(
            converter.value_in_usd("BTCb"),
            Some(decimal_rational(9001, 0))
        );
    }

    #[test]
    fn value_in_usd_should_invert_value_of_underlying_currency_for_solp() {
        let source_prices = vec![(
            "price for SOL".into(),
            PriceInfo {
                token: "SOL".into(),
                unit: "USD".into(),
                value: Decimal::new(4, 0),
                reliability: Decimal::ONE,
            },
        )];
        let default_prices = vec![];
        let synthetics = make_synthetics();
        let currencies = make_currencies();
        let converter = TokenPriceConverter::new(
            &source_prices,
            &default_prices,
            &synthetics,
            &currencies,
            default_threshold(),
        );

        // SOL is 4, so SOLp is 1/4
        assert_eq!(
            converter.value_in_usd("SOLp"),
            Some(decimal_rational(25, 2))
        );
    }

    #[test]
    fn value_in_usd_should_ignore_source_with_tvl_below_threshold() {
        let source_prices = vec![
            (
                "price for SUNDAE in USD".into(),
                PriceInfo {
                    token: "SUNDAE".into(),
                    unit: "USD".into(),
                    value: Decimal::new(100, 0),
                    reliability: Decimal::new(100, 0),
                },
            ),
            (
                "price for SUNDAE in USD".into(),
                PriceInfo {
                    token: "SUNDAE".into(),
                    unit: "USD".into(),
                    value: Decimal::new(1000, 0),
                    reliability: Decimal::new(1000, 0),
                },
            ),
        ];
        let default_prices = vec![];
        let synthetics = make_synthetics();
        let mut currencies = make_currencies();
        currencies.push(CurrencyConfig {
            name: "SUNDAE".into(),
            asset_id: None,
            digits: 0,
            min_tvl: Decimal::new(1000, 0),
        });
        let converter = TokenPriceConverter::new(
            &source_prices,
            &default_prices,
            &synthetics,
            &currencies,
            default_threshold(),
        );

        assert_eq!(
            converter.value_in_usd("SUNDAE"),
            Some(decimal_rational(1000, 0))
        );
    }

    #[test]
    fn value_in_usd_should_ignore_outlier_source() {
        let source_prices = vec![
            (
                "reasonable price".into(),
                PriceInfo {
                    token: "LENFI".into(),
                    unit: "USD".into(),
                    value: Decimal::new(98, 0),
                    reliability: Decimal::new(100, 0),
                },
            ),
            (
                "also reasonable price".into(),
                PriceInfo {
                    token: "LENFI".into(),
                    unit: "USD".into(),
                    value: Decimal::new(102, 0),
                    reliability: Decimal::new(100, 0),
                },
            ),
            (
                "extreme outlier".into(),
                PriceInfo {
                    token: "LENFI".into(),
                    unit: "USD".into(),
                    value: Decimal::new(200, 0),
                    reliability: Decimal::new(100, 0),
                },
            ),
        ];
        let default_prices = vec![];
        let synthetics = make_synthetics();
        let currencies = make_currencies();
        let converter = TokenPriceConverter::new(
            &source_prices,
            &default_prices,
            &synthetics,
            &currencies,
            default_threshold(),
        );

        assert_eq!(
            converter.value_in_usd("LENFI"),
            Some(decimal_rational(100, 0))
        );
    }

    #[test]
    fn value_in_usd_should_not_report_price_if_sources_are_too_far_apart() {
        let source_prices = vec![
            (
                "too low".into(),
                PriceInfo {
                    token: "LENFI".into(),
                    unit: "USD".into(),
                    value: Decimal::new(89, 0),
                    reliability: Decimal::new(100, 0),
                },
            ),
            (
                "too high".into(),
                PriceInfo {
                    token: "LENFI".into(),
                    unit: "USD".into(),
                    value: Decimal::new(111, 0),
                    reliability: Decimal::new(100, 0),
                },
            ),
        ];
        let default_prices = vec![];
        let synthetics = make_synthetics();
        let currencies = make_currencies();
        let converter = TokenPriceConverter::new(
            &source_prices,
            &default_prices,
            &synthetics,
            &currencies,
            default_threshold(),
        );

        assert_eq!(converter.value_in_usd("LENFI"), None,);
    }

    fn synthetic_multifeed_test<P: Into<Decimal>>(
        prices: impl IntoIterator<Item = P>,
        result: Option<BigRational>,
    ) {
        let source_prices: Vec<_> = prices
            .into_iter()
            .enumerate()
            .map(|(i, price)| {
                let label = format!("price {i}");
                let value = PriceInfo {
                    token: format!("COL{i}"),
                    unit: "USD".into(),
                    value: price.into(),
                    reliability: Decimal::ONE,
                };
                (label, value)
            })
            .collect();
        assert!(source_prices.len() <= MULTIFEED_BACKING_CURRENCIES);
        let default_prices = vec![];
        let synthetics = make_synthetics();
        let currencies = make_currencies();
        let converter = TokenPriceConverter::new(
            &source_prices,
            &default_prices,
            &synthetics,
            &currencies,
            default_threshold(),
        );
        assert_eq!(converter.value_in_usd("MULTI"), result);
    }

    #[test]
    fn value_in_usd_multifeed_should_use_average_of_three_prices() {
        synthetic_multifeed_test([100, 105, 108], Some(simple_rational(313, 3)));
    }

    #[test]
    fn value_in_usd_multifeed_should_ignore_very_divergent_third_price() {
        synthetic_multifeed_test([100, 105, 120], Some(simple_rational(205, 2)));
    }

    #[test]
    fn value_in_usd_multifeed_should_use_average_of_two_nearby_prices() {
        synthetic_multifeed_test([100, 105], Some(simple_rational(205, 2)));
    }

    #[test]
    fn value_in_usd_multifeed_should_not_report_price_when_two_prices_diverge() {
        synthetic_multifeed_test([100, 120], None);
    }

    #[test]
    fn value_in_usd_multifeed_should_use_single_reported_price() {
        synthetic_multifeed_test([100], Some(simple_rational(100, 1)));
    }

    #[test]
    fn value_in_usd_multifeed_should_gracefully_handle_no_prices() {
        synthetic_multifeed_test::<Decimal>([], None);
    }

    #[test]
    fn value_in_usd_multifeed_should_use_median_price_when_everything_diverges() {
        synthetic_multifeed_test([100, 120, 140], Some(simple_rational(120, 1)));
    }

    #[test]
    fn token_prices_should_include_all_alphabetized_sources() {
        let source_prices = vec![
            (
                "ADA source".into(),
                PriceInfo {
                    token: "ADA".into(),
                    unit: "USD".into(),
                    value: Decimal::new(5235, 4),
                    reliability: Decimal::ONE,
                },
            ),
            (
                "LENFI source 1".into(),
                PriceInfo {
                    token: "LENFI".into(),
                    unit: "ADA".into(),
                    value: Decimal::new(5000, 0),
                    reliability: Decimal::ONE,
                },
            ),
            (
                "LENFI source 2".into(),
                PriceInfo {
                    token: "LENFI".into(),
                    unit: "ADA".into(),
                    value: Decimal::new(15000, 0),
                    reliability: Decimal::ONE,
                },
            ),
        ];
        let default_prices = vec![];
        let synthetics = make_synthetics();
        let currencies = make_currencies();
        let converter = TokenPriceConverter::new(
            &source_prices,
            &default_prices,
            &synthetics,
            &currencies,
            default_threshold(),
        );

        let prices = converter.token_prices();
        let lenfi_prices: Vec<_> = prices.into_iter().filter(|p| p.token == "LENFI").collect();
        assert_eq!(
            lenfi_prices,
            vec![TokenPrice {
                token: "LENFI".into(),
                unit: "ADA".into(),
                value: decimal_rational(10000, 0),
                sources: vec![
                    TokenPriceSource {
                        name: "LENFI source 1".into(),
                        value: decimal_rational(5000, 0),
                        reliability: BigRational::one(),
                    },
                    TokenPriceSource {
                        name: "LENFI source 2".into(),
                        value: decimal_rational(15000, 0),
                        reliability: BigRational::one(),
                    }
                ]
            }]
        );
    }

    #[test]
    fn token_prices_should_include_defaults_if_no_explicit_prices_were_found() {
        let source_prices = vec![];
        let default_prices = make_default_prices();
        let synthetics = make_synthetics();
        let currencies = make_currencies();
        let converter = TokenPriceConverter::new(
            &source_prices,
            &default_prices,
            &synthetics,
            &currencies,
            default_threshold(),
        );

        let prices = converter.token_prices();
        let lenfi_prices: Vec<_> = prices.into_iter().filter(|p| p.token == "LENFI").collect();
        assert_eq!(
            lenfi_prices,
            vec![TokenPrice {
                token: "LENFI".into(),
                unit: "USD".into(),
                value: decimal_rational(379, 2),
                sources: vec![TokenPriceSource {
                    name: "Hard-coded default value".into(),
                    value: decimal_rational(379, 2),
                    reliability: BigRational::one(),
                },]
            }]
        );
    }

    #[test]
    fn token_prices_should_not_include_synthetics_without_prices() {
        let source_prices = vec![];
        let default_prices = vec![];
        let synthetics = make_synthetics();
        let currencies = make_currencies();
        let converter = TokenPriceConverter::new(
            &source_prices,
            &default_prices,
            &synthetics,
            &currencies,
            default_threshold(),
        );

        let prices = converter.token_prices();
        let lenfi_prices: Vec<_> = prices.into_iter().filter(|p| p.token == "LENFI").collect();
        assert_eq!(lenfi_prices, vec![]);
    }
}
