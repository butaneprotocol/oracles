use crate::price_feed::{IntervalBound, IntervalBoundType, PriceFeed, Validity};

pub fn should_sign_all(leader_feed: &[PriceFeed], my_feed: &[PriceFeed]) -> Result<(), String> {
    if leader_feed.len() != my_feed.len() {
        return Err("Mismatched price feed count. Is this server misconfigured?".into());
    }
    for (leader, mine) in leader_feed.iter().zip(my_feed) {
        should_sign(leader, mine)?;
    }
    Ok(())
}

fn should_sign(leader_feed: &PriceFeed, my_feed: &PriceFeed) -> Result<(), String> {
    if leader_feed.synthetic != my_feed.synthetic {
        return Err(format!(
            "mismatched synthetics: leader has {}, we have {}",
            leader_feed.synthetic, my_feed.synthetic
        ));
    }
    if !is_validity_close_enough(&leader_feed.validity, &my_feed.validity) {
        return Err(format!(
            "mismatched validity: leader has {:?}, we have {:?}",
            leader_feed.validity, my_feed.validity
        ));
    }
    if leader_feed.collateral_prices.len() != my_feed.collateral_prices.len() {
        return Err(format!(
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
            return Err(format!(
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
    Ok(())
}
fn is_validity_close_enough(leader_validity: &Validity, my_validity: &Validity) -> bool {
    are_bounds_close_enough(&leader_validity.lower_bound, &my_validity.lower_bound)
        && are_bounds_close_enough(&leader_validity.upper_bound, &my_validity.upper_bound)
}

fn are_bounds_close_enough(leader_bound: &IntervalBound, my_bound: &IntervalBound) -> bool {
    if let IntervalBoundType::Finite(leader_moment) = leader_bound.bound_type {
        if let IntervalBoundType::Finite(my_moment) = my_bound.bound_type {
            let difference = leader_moment.max(my_moment) - leader_moment.min(my_moment);
            // allow up to 60 seconds difference between bounds
            return difference < 1000 * 60;
        }
    }
    leader_bound == my_bound
}

#[cfg(test)]
mod tests {
    use num_bigint::BigUint;

    use super::should_sign_all;
    use crate::price_feed::{IntervalBound, PriceFeed, Validity};

    fn price_feed(
        synthetic: &str,
        collateral_names: &[&str],
        collateral_prices: &[u64],
        denominator: u64,
    ) -> PriceFeed {
        PriceFeed {
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

    #[test]
    fn should_sign_close_enough_collateral_prices() {
        let leader_feed = vec![price_feed("SYNTH", &["COLL"], &[2], 1)];
        let my_feed = vec![price_feed("SYNTH", &["COLL"], &[199], 100)];

        assert!(should_sign_all(&leader_feed, &my_feed).is_ok());
    }

    #[test]
    fn should_not_sign_distant_collateral_prices() {
        let leader_feed = vec![price_feed("SYNTH", &["COLL"], &[2], 1)];
        let my_feed = vec![price_feed("SYNTH", &["COLL"], &[3], 2)];

        assert!(should_sign_all(&leader_feed, &my_feed).is_err());
    }

    #[test]
    fn should_sign_close_enough_validity() {
        const TIMESTAMP: u64 = 1712723729359;
        let mut leader_price = price_feed("SYNTH", &["COLL"], &[1], 1);
        leader_price.validity = Validity {
            lower_bound: IntervalBound::unix_timestamp(TIMESTAMP, true),
            upper_bound: IntervalBound::unix_timestamp(TIMESTAMP + 3000000, true),
        };
        let mut my_price = price_feed("SYNTH", &["COLL"], &[1], 1);
        my_price.validity = Validity {
            lower_bound: IntervalBound::unix_timestamp(TIMESTAMP + 5000, true),
            upper_bound: IntervalBound::unix_timestamp(TIMESTAMP + 3005000, true),
        };

        assert!(should_sign_all(&[leader_price], &[my_price]).is_ok());
    }

    #[test]
    fn should_not_sign_distant_validity() {
        const TIMESTAMP: u64 = 1712723729359;
        let mut leader_price = price_feed("SYNTH", &["COLL"], &[1], 1);
        leader_price.validity = Validity {
            lower_bound: IntervalBound::unix_timestamp(TIMESTAMP, true),
            upper_bound: IntervalBound::unix_timestamp(TIMESTAMP + 3000000, true),
        };
        let mut my_price = price_feed("SYNTH", &["COLL"], &[1], 1);
        my_price.validity = Validity {
            lower_bound: IntervalBound::unix_timestamp(TIMESTAMP + 5000000, true),
            upper_bound: IntervalBound::unix_timestamp(TIMESTAMP + 8000000, true),
        };

        assert!(should_sign_all(&[leader_price], &[my_price]).is_err());
    }
}
