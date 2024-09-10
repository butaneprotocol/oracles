use std::time::Duration;

use num_rational::BigRational;
use num_traits::One;

pub struct GemaCalculator {
    smoothing: f64,
    round_period: Duration,
}

impl GemaCalculator {
    pub fn new(smoothing: f64, round_period: Duration) -> Self {
        Self {
            smoothing,
            round_period,
        }
    }
    pub fn smooth(
        &self,
        time_elapsed: Duration,
        prev_prices: &[BigRational],
        prices: Vec<BigRational>,
    ) -> Vec<BigRational> {
        if prev_prices.len() != prices.len() {
            // If we changed which collaterals are available for a synthetic, we should just not apply GEMA.
            return prices;
        }

        let periods = time_elapsed.as_secs_f64() / self.round_period.as_secs_f64();
        let factor = self.smoothing / (periods + 1.0).min(2.0);

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
}
