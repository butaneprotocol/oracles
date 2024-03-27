use std::collections::HashMap;

use rust_decimal::Decimal;

use crate::{
    apis::source::PriceInfo,
    config::{CollateralConfig, SyntheticConfig},
};

#[derive(Debug, Eq, Hash, PartialEq)]
struct CurrencyPair<'a>(&'a str, &'a str);
pub struct ConversionLookup<'a> {
    conversions: HashMap<CurrencyPair<'a>, Decimal>,
    defaults: HashMap<&'a str, Decimal>,
}

impl<'a> ConversionLookup<'a> {
    pub fn new(
        prices: &'a [PriceInfo],
        synthetics: &'a [SyntheticConfig],
        collateral: &'a [CollateralConfig],
    ) -> Self {
        let mut conversions = HashMap::new();

        // Set our default prices
        let mut defaults = HashMap::new();
        for synth in synthetics {
            defaults.insert(synth.name.as_str(), synth.price);
        }
        for coll in collateral {
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

#[cfg(test)]
mod tests {
    use rust_decimal::Decimal;

    use crate::{
        apis::source::PriceInfo,
        config::{CollateralConfig, SyntheticConfig},
    };

    use super::ConversionLookup;

    fn make_config() -> (Vec<SyntheticConfig>, Vec<CollateralConfig>) {
        let synthetics = vec![
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
        ];
        let collateral = vec![
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
        ];
        (synthetics, collateral)
    }

    #[test]
    fn value_in_usd_should_return_defaults() {
        let (synthetics, collateral) = make_config();
        let prices = vec![];
        let lookup = ConversionLookup::new(&prices, &synthetics, &collateral);

        assert_eq!(lookup.value_in_usd("ADA"), Decimal::new(6, 1));
    }

    #[test]
    fn value_in_usd_should_return_value_from_source() {
        let (synthetics, collateral) = make_config();
        let prices = vec![PriceInfo {
            token: "BTCb".into(),
            unit: "USD".into(),
            value: Decimal::new(60000, 0),
        }];
        let lookup = ConversionLookup::new(&prices, &synthetics, &collateral);

        assert_eq!(lookup.value_in_usd("BTCb"), Decimal::new(60000, 0));
    }

    #[test]
    fn value_in_usd_should_average_prices() {
        let (synthetics, collateral) = make_config();
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
        let lookup = ConversionLookup::new(&prices, &synthetics, &collateral);

        assert_eq!(lookup.value_in_usd("BTCb"), Decimal::new(75000, 0));
    }

    #[test]
    fn value_in_usd_should_convert_prices_in_ada_using_default_ada_price() {
        let (synthetics, collateral) = make_config();
        let prices = vec![PriceInfo {
            token: "LENFI".into(),
            unit: "ADA".into(),
            value: Decimal::new(10000, 0),
        }];
        let lookup = ConversionLookup::new(&prices, &synthetics, &collateral);

        assert_eq!(lookup.value_in_usd("LENFI"), Decimal::new(6000, 0));
    }

    #[test]
    fn value_in_usd_should_convert_prices_in_ada_using_ada_price_from_api() {
        let (synthetics, collateral) = make_config();
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
        let lookup = ConversionLookup::new(&prices, &synthetics, &collateral);

        assert_eq!(lookup.value_in_usd("LENFI"), Decimal::new(3000, 0));
    }
}
