use std::collections::BTreeMap;

use num_traits::Inv;
use rust_decimal::Decimal;
use serde::Serialize;

use crate::{
    config::{CurrencyConfig, SyntheticConfig},
    sources::source::PriceInfo,
};

#[derive(Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct TokenPair<'a>(&'a str, &'a str);

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TokenPrice {
    pub token: String,
    pub unit: String,
    pub value: Decimal,
    pub sources: Vec<TokenPriceSource>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TokenPriceSource {
    pub name: String,
    pub value: Decimal,
    pub reliability: Decimal,
}

pub struct TokenPriceConverter<'a> {
    prices: BTreeMap<&'a str, Vec<TokenPrice>>,
    synthetics: BTreeMap<&'a str, &'a SyntheticConfig>,
}

impl<'a> TokenPriceConverter<'a> {
    pub fn new(
        source_prices: &'a [(String, PriceInfo)],
        synthetics: &'a [SyntheticConfig],
        currencies: &'a [CurrencyConfig],
    ) -> Self {
        let synthetics = synthetics.iter().map(|s| (s.name.as_str(), s)).collect();

        let mut value_sources = BTreeMap::new();
        for (source_name, price) in source_prices {
            let source = TokenPriceSource {
                name: source_name.to_string(),
                value: price.value,
                reliability: price.reliability,
            };
            value_sources
                .entry(TokenPair(&price.token, &price.unit))
                .or_insert(vec![])
                .push(source);
        }

        let mut prices = BTreeMap::new();
        for (tokens, sources) in value_sources {
            let mut value_sum = Decimal::ZERO;
            let mut reliability_sum = Decimal::ZERO;
            for source in &sources {
                value_sum += source.value * source.reliability;
                reliability_sum += source.reliability;
            }
            let value = TokenPrice {
                token: tokens.0.to_string(),
                unit: tokens.1.to_string(),
                value: value_sum / reliability_sum,
                sources,
            };
            prices.entry(tokens.0).or_insert(vec![]).push(value.clone());
        }

        // set defaults for anything we don't have a price for
        for curr in currencies {
            prices.entry(&curr.name).or_insert(vec![TokenPrice {
                token: curr.name.clone(),
                unit: "USD".into(),
                value: curr.price,
                sources: vec![TokenPriceSource {
                    name: "Hard-coded default value".into(),
                    value: curr.price,
                    reliability: Decimal::ONE,
                }],
            }]);
        }

        Self { prices, synthetics }
    }

    pub fn value_in_usd(&self, token: &str) -> Decimal {
        if token == "USD" {
            return Decimal::ONE;
        }

        // A synthetic has the same value as its backing currency
        if let Some(synthetic) = self.synthetics.get(token) {
            let value = self.value_in_usd(&synthetic.backing_currency);
            return if synthetic.invert { value.inv() } else { value };
        }

        let prices = self
            .prices
            .get(token)
            .filter(|ps| !ps.is_empty())
            .unwrap_or_else(|| panic!("No price found for {}!", token));

        let mut value = Decimal::ZERO;
        let mut reliability = Decimal::ZERO;
        for price in prices {
            let conversion_factor = self.value_in_usd(&price.unit);
            for source in &price.sources {
                value += source.value * conversion_factor * source.reliability;
                reliability += source.reliability;
            }
        }

        value / reliability
    }

    pub fn token_prices(&self) -> Vec<TokenPrice> {
        self.prices.values().flatten().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use rust_decimal::Decimal;

    use crate::{
        config::{CurrencyConfig, SyntheticConfig},
        price_aggregator::{TokenPrice, TokenPriceSource},
        sources::source::PriceInfo,
    };

    use super::TokenPriceConverter;

    fn make_config() -> (Vec<SyntheticConfig>, Vec<CurrencyConfig>) {
        let synthetics = vec![
            SyntheticConfig {
                name: "USDb".into(),
                backing_currency: "USD".into(),
                invert: false,
                collateral: vec![],
            },
            SyntheticConfig {
                name: "BTCb".into(),
                backing_currency: "BTC".into(),
                invert: false,
                collateral: vec![],
            },
            SyntheticConfig {
                name: "SOLp".into(),
                backing_currency: "SOL".into(),
                invert: true,
                collateral: vec![],
            },
        ];
        let currencies = vec![
            CurrencyConfig {
                name: "ADA".into(),
                asset_id: None,
                price: Decimal::new(6, 1),
                digits: 6,
            },
            CurrencyConfig {
                name: "BTC".into(),
                asset_id: None,
                price: Decimal::new(58262, 0),
                digits: 8,
            },
            CurrencyConfig {
                name: "LENFI".into(),
                asset_id: Some(
                    "8fef2d34078659493ce161a6c7fba4b56afefa8535296a5743f69587.41414441".into(),
                ),
                price: Decimal::new(379, 2),
                digits: 6,
            },
            CurrencyConfig {
                name: "USDT".into(),
                asset_id: None,
                price: Decimal::ONE,
                digits: 6,
            },
        ];
        (synthetics, currencies)
    }

    #[test]
    fn value_in_usd_should_return_defaults() {
        let (synthetics, currencies) = make_config();
        let source_prices = vec![];
        let converter = TokenPriceConverter::new(&source_prices, &synthetics, &currencies);

        assert_eq!(converter.value_in_usd("ADA"), Decimal::new(6, 1));
    }

    #[test]
    fn value_in_usd_should_return_value_from_source() {
        let (synthetics, currencies) = make_config();
        let source_prices = vec![(
            "Source".into(),
            PriceInfo {
                token: "BTC".into(),
                unit: "USD".into(),
                value: Decimal::new(60000, 0),
                reliability: Decimal::ONE,
            },
        )];
        let converter = TokenPriceConverter::new(&source_prices, &synthetics, &currencies);

        assert_eq!(converter.value_in_usd("BTC"), Decimal::new(60000, 0));
    }

    #[test]
    fn value_in_usd_should_average_prices() {
        let (synthetics, currencies) = make_config();
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
        let converter = TokenPriceConverter::new(&source_prices, &synthetics, &currencies);

        assert_eq!(converter.value_in_usd("BTC"), Decimal::new(75000, 0));
    }

    #[test]
    fn value_in_usd_should_weight_prices() {
        let (synthetics, currencies) = make_config();
        let source_prices = vec![
            (
                "Vibes".into(),
                PriceInfo {
                    token: "BTC".into(),
                    unit: "USD".into(),
                    value: Decimal::new(100, 0),
                    reliability: Decimal::ONE,
                },
            ),
            (
                "My uncle".into(),
                PriceInfo {
                    token: "BTC".into(),
                    unit: "USD".into(),
                    value: Decimal::new(200, 0),
                    reliability: Decimal::new(3, 0),
                },
            ),
        ];
        let converter = TokenPriceConverter::new(&source_prices, &synthetics, &currencies);

        assert_eq!(converter.value_in_usd("BTC"), Decimal::new(175, 0));
    }

    #[test]
    fn value_in_usd_should_convert_prices_in_ada_using_default_ada_price() {
        let (synthetics, currencies) = make_config();
        let source_prices = vec![(
            "someone".into(),
            PriceInfo {
                token: "LENFI".into(),
                unit: "ADA".into(),
                value: Decimal::new(10000, 0),
                reliability: Decimal::ONE,
            },
        )];
        let converter = TokenPriceConverter::new(&source_prices, &synthetics, &currencies);

        assert_eq!(converter.value_in_usd("LENFI"), Decimal::new(6000, 0));
    }

    #[test]
    fn value_in_usd_should_convert_prices_in_ada_using_ada_price_from_api() {
        let (synthetics, currencies) = make_config();
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
        let converter = TokenPriceConverter::new(&source_prices, &synthetics, &currencies);

        assert_eq!(converter.value_in_usd("LENFI"), Decimal::new(3000, 0));
    }

    #[test]
    fn value_in_usd_should_convert_prices_in_usdt_using_default_usdt_price() {
        let (synthetics, currencies) = make_config();
        let source_prices = vec![(
            "someone".into(),
            PriceInfo {
                token: "BTC".into(),
                unit: "USDT".into(),
                value: Decimal::new(5000, 0),
                reliability: Decimal::ONE,
            },
        )];
        let converter = TokenPriceConverter::new(&source_prices, &synthetics, &currencies);

        assert_eq!(converter.value_in_usd("BTC"), Decimal::new(5000, 0));
    }

    #[test]
    fn value_in_usd_should_convert_prices_in_usdt_using_usdt_price_from_api() {
        let (synthetics, currencies) = make_config();
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
        let converter = TokenPriceConverter::new(&source_prices, &synthetics, &currencies);

        assert_eq!(converter.value_in_usd("BTC"), Decimal::new(5025, 0));
    }

    #[test]
    fn value_in_usd_should_average_prices_in_different_currencies() {
        let (synthetics, currencies) = make_config();
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
                "price for BTC in USD".into(),
                PriceInfo {
                    token: "BTC".into(),
                    unit: "USD".into(),
                    value: Decimal::new(5000, 0),
                    reliability: Decimal::ONE,
                },
            ),
            (
                "price for BTC in USDT".into(),
                PriceInfo {
                    token: "BTC".into(),
                    unit: "USDT".into(),
                    value: Decimal::new(5000, 0),
                    reliability: Decimal::ONE,
                },
            ),
        ];
        let converter = TokenPriceConverter::new(&source_prices, &synthetics, &currencies);

        assert_eq!(converter.value_in_usd("BTC"), Decimal::new(50125, 1));
    }

    #[test]
    fn value_in_usd_should_return_value_of_underlying_currency_for_synthetics() {
        let (synthetics, currencies) = make_config();
        let source_prices = vec![(
            "price for BTC".into(),
            PriceInfo {
                token: "BTC".into(),
                unit: "USD".into(),
                value: Decimal::new(9001, 0),
                reliability: Decimal::ONE,
            },
        )];
        let converter = TokenPriceConverter::new(&source_prices, &synthetics, &currencies);

        assert_eq!(converter.value_in_usd("BTCb"), Decimal::new(9001, 0));
    }

    #[test]
    fn value_in_usd_should_invert_value_of_underlying_currency_for_solp() {
        let (synthetics, currencies) = make_config();
        let source_prices = vec![(
            "price for SOL".into(),
            PriceInfo {
                token: "SOL".into(),
                unit: "USD".into(),
                value: Decimal::new(4, 0),
                reliability: Decimal::ONE,
            },
        )];
        let converter = TokenPriceConverter::new(&source_prices, &synthetics, &currencies);

        // SOL is 4, so SOLp is 1/4
        assert_eq!(converter.value_in_usd("SOLp"), Decimal::new(25, 2));
    }

    #[test]
    fn token_prices_should_include_all_alphabetized_sources() {
        let (synthetics, currencies) = make_config();
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
        let converter = TokenPriceConverter::new(&source_prices, &synthetics, &currencies);

        let prices = converter.token_prices();
        let lenfi_prices: Vec<_> = prices.into_iter().filter(|p| p.token == "LENFI").collect();
        assert_eq!(
            lenfi_prices,
            vec![TokenPrice {
                token: "LENFI".into(),
                unit: "ADA".into(),
                value: Decimal::new(10000, 0),
                sources: vec![
                    TokenPriceSource {
                        name: "LENFI source 1".into(),
                        value: Decimal::new(5000, 0),
                        reliability: Decimal::ONE,
                    },
                    TokenPriceSource {
                        name: "LENFI source 2".into(),
                        value: Decimal::new(15000, 0),
                        reliability: Decimal::ONE,
                    }
                ]
            }]
        );
    }

    #[test]
    fn token_prices_should_include_defaults_if_no_explicit_prices_were_found() {
        let (synthetics, currencies) = make_config();
        let source_prices = vec![];
        let converter = TokenPriceConverter::new(&source_prices, &synthetics, &currencies);

        let prices = converter.token_prices();
        let lenfi_prices: Vec<_> = prices.into_iter().filter(|p| p.token == "LENFI").collect();
        assert_eq!(
            lenfi_prices,
            vec![TokenPrice {
                token: "LENFI".into(),
                unit: "USD".into(),
                value: Decimal::new(379, 2),
                sources: vec![TokenPriceSource {
                    name: "Hard-coded default value".into(),
                    value: Decimal::new(379, 2),
                    reliability: Decimal::ONE,
                },]
            }]
        );
    }
}
