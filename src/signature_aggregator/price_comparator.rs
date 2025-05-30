use std::collections::BTreeMap;

use crate::price_feed::{
    GenericPriceFeed, IntervalBound, IntervalBoundType, SyntheticPriceFeed, Validity,
};

#[derive(Debug)]
pub enum ComparisonResult {
    Sign,
    DoNotSign(String),
}

pub fn choose_generic_feeds_to_sign<'a>(
    leader_feed: &'a [GenericPriceFeed],
    my_feed: &'a [GenericPriceFeed],
) -> Vec<(&'a str, ComparisonResult)> {
    choose_feeds_to_sign(
        leader_feed,
        my_feed,
        |feed| &feed.name,
        should_sign_generic_feed,
    )
}

pub fn choose_synth_feeds_to_sign<'a>(
    leader_feed: &'a [SyntheticPriceFeed],
    my_feed: &'a [SyntheticPriceFeed],
) -> Vec<(&'a str, ComparisonResult)> {
    choose_feeds_to_sign(
        leader_feed,
        my_feed,
        |feed| &feed.synthetic,
        should_sign_synth_feed,
    )
}

fn choose_feeds_to_sign<'a, T, F, S>(
    leader_feed: &'a [T],
    my_feed: &'a [T],
    feed_name: F,
    should_sign: S,
) -> Vec<(&'a str, ComparisonResult)>
where
    F: Fn(&T) -> &str,
    S: Fn(&T, &T) -> ComparisonResult,
{
    let leader_values: BTreeMap<_, _> = leader_feed
        .iter()
        .map(|feed| (feed_name(feed), feed))
        .collect();
    let my_values: BTreeMap<_, _> = my_feed.iter().map(|feed| (feed_name(feed), feed)).collect();

    let mut results = vec![];
    for feed in leader_values.keys() {
        if !my_values.contains_key(feed) {
            results.push((
                *feed,
                ComparisonResult::DoNotSign(
                    "the leader had a price for this feed, but we did not".into(),
                ),
            ));
        }
    }
    for (feed, my_feed) in my_values {
        let comparison_result = match leader_values.get(feed) {
            Some(leader_feed) => should_sign(leader_feed, my_feed),
            None => {
                ComparisonResult::DoNotSign("the leader did not have a price for this feed".into())
            }
        };
        results.push((feed, comparison_result));
    }

    results
}

fn should_sign_generic_feed(
    leader_feed: &GenericPriceFeed,
    my_feed: &GenericPriceFeed,
) -> ComparisonResult {
    if leader_feed.name != my_feed.name {
        return ComparisonResult::DoNotSign(format!(
            "mismatched feeds: leader has {}, we have {}",
            leader_feed.name, my_feed.name
        ));
    }

    // let timestamps drift up to 60 seconds
    if leader_feed.timestamp.abs_diff(my_feed.timestamp) > 1000 * 60 {
        return ComparisonResult::DoNotSign(format!(
            "mismatched timestamps: leader has {}, we have {}",
            leader_feed.timestamp, my_feed.timestamp
        ));
    }

    let max_value = leader_feed.price.clone().max(my_feed.price.clone());
    let min_value = leader_feed.price.clone().min(my_feed.price.clone());
    let difference = &max_value - min_value;
    if difference * 100u32 > max_value {
        return ComparisonResult::DoNotSign(format!(
            "prices ({}) are too distant: leader has {}, we have {}",
            leader_feed.name, leader_feed.price, my_feed.price,
        ));
    }

    ComparisonResult::Sign
}

fn should_sign_synth_feed(
    leader_feed: &SyntheticPriceFeed,
    my_feed: &SyntheticPriceFeed,
) -> ComparisonResult {
    if leader_feed.synthetic != my_feed.synthetic {
        return ComparisonResult::DoNotSign(format!(
            "mismatched synthetics: leader has {}, we have {}",
            leader_feed.synthetic, my_feed.synthetic
        ));
    }
    if !is_validity_close_enough(&leader_feed.validity, &my_feed.validity) {
        return ComparisonResult::DoNotSign(format!(
            "mismatched validity: leader has {:?}, we have {:?}",
            leader_feed.validity, my_feed.validity
        ));
    }
    if leader_feed.collateral_prices.len() != my_feed.collateral_prices.len() {
        return ComparisonResult::DoNotSign(format!(
            "wrong number of collateral prices: leader has {}, we have {}",
            leader_feed.collateral_prices.len(),
            my_feed.collateral_prices.len()
        ));
    }
    // If one price is >1% less than the other, they're too distant to trust
    for ((leader_price, my_price), collateral) in leader_feed
        .collateral_prices
        .iter()
        .zip(my_feed.collateral_prices.iter())
        .zip(
            my_feed
                .collateral_names
                .as_ref()
                .expect("my feed should always have collateral names")
                .iter(),
        )
    {
        let leader_value = leader_price * &my_feed.denominator;
        let my_value = my_price * &leader_feed.denominator;
        let max_value = leader_value.clone().max(my_value.clone());
        let min_value = leader_value.min(my_value);
        let difference = &max_value - min_value;
        if difference * 100u32 > max_value {
            return ComparisonResult::DoNotSign(format!(
                "collateral prices ({} per {}) are too distant: leader has {}/{}, we have {}/{}",
                leader_feed.synthetic,
                collateral,
                leader_price,
                leader_feed.denominator,
                my_price,
                my_feed.denominator
            ));
        }
    }
    ComparisonResult::Sign
}
fn is_validity_close_enough(leader_validity: &Validity, my_validity: &Validity) -> bool {
    are_bounds_close_enough(&leader_validity.lower_bound, &my_validity.lower_bound)
        && are_bounds_close_enough(&leader_validity.upper_bound, &my_validity.upper_bound)
}

fn are_bounds_close_enough(leader_bound: &IntervalBound, my_bound: &IntervalBound) -> bool {
    if let IntervalBoundType::Finite(leader_moment) = leader_bound.bound_type {
        if let IntervalBoundType::Finite(my_moment) = my_bound.bound_type {
            let difference = leader_moment.max(my_moment) - leader_moment.min(my_moment);
            // allow up to 61 seconds difference between bounds
            return difference < 1000 * 61;
        }
    }
    leader_bound == my_bound
}

#[cfg(test)]
mod tests {
    use num_bigint::BigUint;

    use super::choose_synth_feeds_to_sign;
    use crate::{
        price_feed::{GenericPriceFeed, IntervalBound, SyntheticPriceFeed, Validity},
        signature_aggregator::price_comparator::{ComparisonResult, choose_generic_feeds_to_sign},
    };

    fn synthetic_feed(
        synthetic: &str,
        collateral_names: &[&str],
        collateral_prices: &[u64],
        denominator: u64,
    ) -> SyntheticPriceFeed {
        SyntheticPriceFeed {
            collateral_names: Some(collateral_names.iter().map(|&s| s.to_string()).collect()),
            collateral_prices: collateral_prices
                .iter()
                .map(|&p| BigUint::from(p))
                .collect(),
            synthetic: synthetic.into(),
            denominator: BigUint::from(denominator),
            validity: Validity::default(),
        }
    }

    fn generic_feed(name: &str, price: u64) -> GenericPriceFeed {
        GenericPriceFeed {
            price: BigUint::from(price),
            name: name.to_string(),
            timestamp: 1337,
        }
    }

    #[test]
    fn should_sign_close_enough_collateral_prices() {
        let leader_feed = vec![synthetic_feed("SYNTH", &["COLL"], &[2], 1)];
        let my_feed = vec![synthetic_feed("SYNTH", &["COLL"], &[199], 100)];

        let result = choose_synth_feeds_to_sign(&leader_feed, &my_feed);
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0], ("SYNTH", ComparisonResult::Sign)));
    }

    #[test]
    fn should_not_sign_distant_collateral_prices() {
        let leader_feed = vec![synthetic_feed("SYNTH", &["COLL"], &[2], 1)];
        let my_feed = vec![synthetic_feed("SYNTH", &["COLL"], &[3], 2)];

        let result = choose_synth_feeds_to_sign(&leader_feed, &my_feed);
        assert_eq!(result.len(), 1);
        assert!(matches!(
            result[0],
            ("SYNTH", ComparisonResult::DoNotSign(_))
        ));
    }

    #[test]
    fn should_sign_close_enough_validity() {
        const TIMESTAMP: u64 = 1712723729359;
        let mut leader_price = synthetic_feed("SYNTH", &["COLL"], &[1], 1);
        leader_price.validity = Validity {
            lower_bound: IntervalBound::unix_timestamp(TIMESTAMP, true),
            upper_bound: IntervalBound::unix_timestamp(TIMESTAMP + 3000000, true),
        };
        let leader_feed = vec![leader_price];
        let mut my_price = synthetic_feed("SYNTH", &["COLL"], &[1], 1);
        my_price.validity = Validity {
            lower_bound: IntervalBound::unix_timestamp(TIMESTAMP + 5000, true),
            upper_bound: IntervalBound::unix_timestamp(TIMESTAMP + 3005000, true),
        };
        let my_feed = vec![my_price];

        let result = choose_synth_feeds_to_sign(&leader_feed, &my_feed);
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0], ("SYNTH", ComparisonResult::Sign)));
    }

    #[test]
    fn should_not_sign_distant_validity() {
        const TIMESTAMP: u64 = 1712723729359;
        let mut leader_price = synthetic_feed("SYNTH", &["COLL"], &[1], 1);
        leader_price.validity = Validity {
            lower_bound: IntervalBound::unix_timestamp(TIMESTAMP, true),
            upper_bound: IntervalBound::unix_timestamp(TIMESTAMP + 3000000, true),
        };
        let leader_feed = vec![leader_price];
        let mut my_price = synthetic_feed("SYNTH", &["COLL"], &[1], 1);
        my_price.validity = Validity {
            lower_bound: IntervalBound::unix_timestamp(TIMESTAMP + 5000000, true),
            upper_bound: IntervalBound::unix_timestamp(TIMESTAMP + 8000000, true),
        };
        let my_feed = vec![my_price];

        let result = choose_synth_feeds_to_sign(&leader_feed, &my_feed);
        assert_eq!(result.len(), 1);
        assert!(matches!(
            result[0],
            ("SYNTH", ComparisonResult::DoNotSign(_))
        ));
    }

    #[test]
    fn should_sign_close_enough_generic_prices() {
        let leader_feed = vec![generic_feed("ADA/USD#RAW", 1000000)];
        let my_feed = vec![generic_feed("ADA/USD#RAW", 1010000)];

        let result = choose_generic_feeds_to_sign(&leader_feed, &my_feed);
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0], ("ADA/USD#RAW", ComparisonResult::Sign)));
    }

    #[test]
    fn should_not_sign_distant_generic_prices() {
        let leader_feed = vec![generic_feed("ADA/USD#RAW", 1000000)];
        let my_feed = vec![generic_feed("ADA/USD#RAW", 2000000)];

        let result = choose_generic_feeds_to_sign(&leader_feed, &my_feed);
        assert_eq!(result.len(), 1);
        assert!(matches!(
            result[0],
            ("ADA/USD#RAW", ComparisonResult::DoNotSign(_))
        ));
    }

    #[test]
    fn should_sign_close_enough_timestamp() {
        const TIMESTAMP: u64 = 1712723729359;
        let mut leader_price = generic_feed("ADA/USD#RAW", 1000000);
        leader_price.timestamp = TIMESTAMP;
        let leader_feed = vec![leader_price];

        let mut my_price = generic_feed("ADA/USD#RAW", 1000000);
        my_price.timestamp = TIMESTAMP + 5000;
        let my_feed = vec![my_price];

        let result = choose_generic_feeds_to_sign(&leader_feed, &my_feed);
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0], ("ADA/USD#RAW", ComparisonResult::Sign)));
    }

    #[test]
    fn should_not_sign_distant_timestmap() {
        const TIMESTAMP: u64 = 1712723729359;
        let mut leader_price = generic_feed("ADA/USD#RAW", 1000000);
        leader_price.timestamp = TIMESTAMP;
        let leader_feed = vec![leader_price];

        let mut my_price = generic_feed("ADA/USD#RAW", 1000000);
        my_price.timestamp = TIMESTAMP + 5000000;
        let my_feed = vec![my_price];

        let result = choose_generic_feeds_to_sign(&leader_feed, &my_feed);
        assert_eq!(result.len(), 1);
        assert!(matches!(
            result[0],
            ("ADA/USD#RAW", ComparisonResult::DoNotSign(_))
        ));
    }
}
