use std::time::Duration;

use num_bigint::BigUint;
use num_rational::{BigRational, Ratio};
use num_traits::One;

pub struct GemaCalculator {
    periods: usize,
    round_duration: Duration,
}

impl GemaCalculator {
    pub fn new(periods: usize, round_duration: Duration) -> Self {
        Self {
            periods,
            round_duration,
        }
    }
    pub fn smooth_synthetic_price(
        &self,
        time_elapsed: Duration,
        prev_prices: &[BigRational],
        prices: Vec<BigRational>,
    ) -> Vec<BigRational> {
        if prev_prices.len() != prices.len() {
            // If we changed which collaterals are available for a synthetic, we should just not apply GEMA.
            return prices;
        }

        // The "time_elapsed" is between the end of some round (when results were published)
        // and the start of this round (when prices are gathered).
        // Approximate how many periods have elapsed,
        // and round so that slight timing differences don't affect the output
        let periods_elapsed = time_elapsed.as_secs_f64() / self.round_duration.as_secs_f64();
        let periods_elapsed = periods_elapsed.round();

        let periods = (self.periods as f64) + periods_elapsed;
        let factor: f64 = 2.0 / (periods + 1.0).max(2.0);

        let Some(prev_price_weight) = BigRational::from_float(factor) else {
            // this only happens if an NaN or Infinity sneaks into our calculations somewhere
            return prices;
        };

        let curr_price_weight = BigRational::one() - &prev_price_weight;

        prices
            .into_iter()
            .zip(prev_prices.iter())
            .map(|(price, prev_price)| {
                if price < *prev_price {
                    // Reflect downturns immediately
                    return price;
                }
                (price * &curr_price_weight) + (prev_price * &prev_price_weight)
            })
            .collect()
    }

    pub fn smooth_price(
        &self,
        time_elapsed: Duration,
        prev_price: &BigUint,
        price: BigUint,
    ) -> BigUint {
        // The "time_elapsed" is between the end of some round (when results were published)
        // and the start of this round (when prices are gathered).
        // Approximate how many periods have elapsed,
        // and round so that slight timing differences don't affect the output
        let periods_elapsed = time_elapsed.as_secs_f64() / self.round_duration.as_secs_f64();
        let periods_elapsed = periods_elapsed.round();

        let periods = (self.periods as f64) + periods_elapsed;
        let factor: f64 = 2.0 / (periods + 1.0).max(2.0);

        let Some(prev_price_weight) = Ratio::from_float(factor) else {
            // this only happens if an NaN or Infinity sneaks into our calculations somewhere
            return price;
        };
        let Some(prev_numer) = prev_price_weight.numer().to_biguint() else {
            return price;
        };
        let Some(prev_denom) = prev_price_weight.denom().to_biguint() else {
            return price;
        };

        let curr_price_weight = BigRational::one() - &prev_price_weight;
        let Some(curr_numer) = curr_price_weight.numer().to_biguint() else {
            return price;
        };
        let Some(curr_denom) = curr_price_weight.denom().to_biguint() else {
            return price;
        };

        if price < *prev_price {
            // Reflect downturns immediately
            return price;
        }
        (price * curr_numer / curr_denom) + (prev_price * prev_numer / prev_denom)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use num_bigint::BigUint;
    use num_rational::BigRational;

    use super::GemaCalculator;

    fn rational(numer: i128, denom: i128) -> BigRational {
        BigRational::new(numer.into(), denom.into())
    }

    #[test]
    fn should_only_smooth_increased_prices() {
        let periods = 2;
        let round_duration = Duration::from_secs(5);
        let calculator = GemaCalculator::new(periods, round_duration);

        let prev_prices = vec![rational(1, 1), rational(1, 1)];
        let curr_prices = vec![rational(2, 1), rational(1, 2)];

        let prices = calculator.smooth_synthetic_price(round_duration, &prev_prices, curr_prices);
        assert_eq!(prices, vec![rational(3, 2), rational(1, 2)]);
    }

    #[test]
    fn should_count_rounds_based_on_time_elapsed() {
        let periods = 2;
        let round_duration = Duration::from_secs(5);
        let calculator = GemaCalculator::new(periods, round_duration);

        let prev_prices = vec![rational(1, 1), rational(1, 1)];
        let curr_prices = vec![rational(2, 1), rational(1, 2)];

        // both these durations round to 5 seconds (one round)
        let prices_4sec = calculator.smooth_synthetic_price(
            Duration::from_secs(4),
            &prev_prices,
            curr_prices.clone(),
        );
        let prices_6sec = calculator.smooth_synthetic_price(
            Duration::from_secs(6),
            &prev_prices,
            curr_prices.clone(),
        );
        assert_eq!(prices_4sec, prices_6sec);

        // this duration rounds to 10 seconds (two rounds),
        // so we should assume that we missed a round and not smooth as much
        let prices_8sec =
            calculator.smooth_synthetic_price(Duration::from_secs(8), &prev_prices, curr_prices);
        assert!(prices_6sec[0] < prices_8sec[0]);
    }

    #[test]
    fn should_smooth_individual_price() {
        let periods = 2;
        let round_duration = Duration::from_secs(5);
        let calculator = GemaCalculator::new(periods, round_duration);

        let prev_price = BigUint::from(1000000u64);
        let curr_price = BigUint::from(2000000u64);

        let price = calculator.smooth_price(round_duration, &prev_price, curr_price);
        assert_eq!(price, BigUint::from(1500000u64));
    }
}
