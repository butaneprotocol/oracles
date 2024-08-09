use std::collections::BTreeMap;

use num_bigint::BigInt;
use num_traits::ToPrimitive;
use tracing::debug;

type Ratio = num_rational::Ratio<BigInt>;
use crate::price_feed::{PriceFeed, PriceFeedEntry};

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct PriceKey {
    synthetic: String,
    collateral: usize,
}

fn variance(values: &[Ratio]) -> Ratio {
    let average = values.iter().sum::<Ratio>() / Ratio::from_integer(BigInt::from(values.len()));
    values.iter().map(|v| (v - &average).pow(2)).sum::<Ratio>()
        / Ratio::from_integer(BigInt::from(values.len()))
}

#[derive(Default)]
pub struct PriceInstrumentation {
    round: Option<String>,
    collateral_names: BTreeMap<String, Vec<String>>,
    all_prices: BTreeMap<PriceKey, Vec<Ratio>>,
}

impl PriceInstrumentation {
    pub fn begin_round(&mut self, round: &str, my_prices: &[PriceFeedEntry]) {
        self.end_round();
        self.round = Some(round.to_string());
        self.collateral_names = extract_collateral_names(my_prices);

        for (key, value) in my_prices
            .iter()
            .flat_map(|e| extract_prices_from_feed(&e.data))
        {
            self.all_prices.insert(key, vec![value]);
        }
    }

    pub fn track_prices(&mut self, round: &str, prices: &[PriceFeed]) {
        if !self.round.as_ref().is_some_and(|r| r == round) {
            return;
        }
        for (key, value) in prices.iter().flat_map(extract_prices_from_feed) {
            self.all_prices
                .entry(key)
                .and_modify(|values| values.push(value));
        }
    }

    pub fn end_round(&mut self) {
        if self.round.take().is_some() {
            self.emit_price_variance();
        }
        self.round = None;
        self.all_prices.clear();
    }

    fn emit_price_variance(&self) {
        for (key, values) in self.all_prices.iter() {
            let synthetic_name = &key.synthetic;
            let collateral_name = &self.collateral_names[synthetic_name][key.collateral];
            let variance = variance(values).to_f64().expect("infallible");
            debug!(
                collateral_name,
                synthetic_name,
                histogram.collateral_price_variance = variance,
                "price variance"
            );
        }
    }
}

fn extract_prices_from_feed(feed: &PriceFeed) -> impl Iterator<Item = (PriceKey, Ratio)> + '_ {
    feed.collateral_prices
        .iter()
        .enumerate()
        .map(|(index, coll)| {
            let key = PriceKey {
                synthetic: feed.synthetic.clone(),
                collateral: index,
            };
            let value = Ratio::new(coll.clone().into(), feed.denominator.clone().into());
            (key, value)
        })
}

fn extract_collateral_names(entries: &[PriceFeedEntry]) -> BTreeMap<String, Vec<String>> {
    entries
        .iter()
        .map(|e| {
            let synthetic = e.data.synthetic.clone();
            let collateral = e
                .data
                .collateral_names
                .as_ref()
                .cloned()
                .expect("collateral names should always be included");
            (synthetic, collateral)
        })
        .collect()
}
