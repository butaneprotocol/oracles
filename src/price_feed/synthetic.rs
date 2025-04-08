use std::time::{SystemTime, UNIX_EPOCH};

use minicbor::{Decode, Decoder, Encode, Encoder, decode, encode};
use num_bigint::BigUint;
use num_rational::BigRational;
use pallas_primitives::PlutusData;

use super::{
    PlutusCompatible,
    codec::{decode_enum, decode_struct, encode_enum, encode_struct},
};

#[derive(Clone, Debug)]
pub struct SyntheticPriceData {
    pub price: BigRational,
    pub feed: SyntheticPriceFeed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyntheticPriceFeed {
    pub collateral_names: Option<Vec<String>>,
    pub collateral_prices: Vec<BigUint>,
    pub synthetic: String,
    pub denominator: BigUint,
    pub validity: Validity,
}

impl PlutusCompatible for SyntheticPriceFeed {
    fn to_plutus(&self) -> PlutusData {
        encode_struct(vec![
            self.collateral_prices.to_plutus(),
            self.synthetic.to_plutus(),
            self.denominator.to_plutus(),
            self.validity.to_plutus(),
        ])
    }

    fn from_plutus(data: PlutusData) -> Result<Self, minicbor::decode::Error> {
        let [
            encoded_collateral_prices,
            encoded_synthetic,
            encoded_denominator,
            encoded_validity,
        ] = decode_struct(data)?;

        let collateral_prices = Vec::from_plutus(encoded_collateral_prices)?;
        let synthetic = String::from_plutus(encoded_synthetic)?;
        let denominator = BigUint::from_plutus(encoded_denominator)?;
        let validity = Validity::from_plutus(encoded_validity)?;
        Ok(SyntheticPriceFeed {
            collateral_names: None,
            collateral_prices,
            synthetic,
            denominator,
            validity,
        })
    }
}

impl<C> Encode<C> for SyntheticPriceFeed {
    fn encode<W: encode::Write>(
        &self,
        e: &mut Encoder<W>,
        ctx: &mut C,
    ) -> Result<(), encode::Error<W::Error>> {
        self.to_plutus().encode(e, ctx)
    }
}

impl<'b, C> Decode<'b, C> for SyntheticPriceFeed {
    fn decode(d: &mut Decoder<'b>, ctx: &mut C) -> Result<Self, decode::Error> {
        Self::from_plutus(d.decode_with(ctx)?)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Validity {
    pub lower_bound: IntervalBound,
    pub upper_bound: IntervalBound,
}
impl Default for Validity {
    fn default() -> Self {
        Self {
            lower_bound: IntervalBound::start_of_time(false),
            upper_bound: IntervalBound::end_of_time(false),
        }
    }
}

impl PlutusCompatible for Validity {
    fn to_plutus(&self) -> PlutusData {
        encode_struct(vec![
            self.lower_bound.to_plutus(),
            self.upper_bound.to_plutus(),
        ])
    }

    fn from_plutus(data: PlutusData) -> Result<Self, decode::Error> {
        let [encoded_lower_bound, encoded_upper_bound] = decode_struct(data)?;
        let lower_bound = IntervalBound::from_plutus(encoded_lower_bound)?;
        let upper_bound = IntervalBound::from_plutus(encoded_upper_bound)?;
        Ok(Validity {
            lower_bound,
            upper_bound,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IntervalBound {
    pub bound_type: IntervalBoundType,
    pub is_inclusive: bool,
}
impl IntervalBound {
    pub fn start_of_time(is_inclusive: bool) -> Self {
        Self {
            bound_type: IntervalBoundType::NegativeInfinity,
            is_inclusive,
        }
    }
    pub fn end_of_time(is_inclusive: bool) -> Self {
        Self {
            bound_type: IntervalBoundType::PositiveInfinity,
            is_inclusive,
        }
    }
    pub fn moment(moment: SystemTime, is_inclusive: bool) -> Self {
        let timestamp = moment
            .duration_since(UNIX_EPOCH)
            .expect("it is currently after 1970")
            .as_millis() as u64;
        Self::unix_timestamp(timestamp, is_inclusive)
    }
    pub fn unix_timestamp(timestamp: u64, is_inclusive: bool) -> Self {
        Self {
            bound_type: IntervalBoundType::Finite(timestamp),
            is_inclusive,
        }
    }
}

impl PlutusCompatible for IntervalBound {
    fn to_plutus(&self) -> PlutusData {
        encode_struct(vec![
            self.bound_type.to_plutus(),
            self.is_inclusive.to_plutus(),
        ])
    }

    fn from_plutus(data: PlutusData) -> Result<Self, minicbor::decode::Error> {
        let [encoded_bound_type, encoded_is_inclusive] = decode_struct(data)?;
        let bound_type = IntervalBoundType::from_plutus(encoded_bound_type)?;
        let is_inclusive = bool::from_plutus(encoded_is_inclusive)?;
        Ok(IntervalBound {
            bound_type,
            is_inclusive,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntervalBoundType {
    NegativeInfinity,
    #[allow(unused)]
    Finite(u64),
    PositiveInfinity,
}
impl PlutusCompatible for IntervalBoundType {
    fn to_plutus(&self) -> PlutusData {
        match self {
            Self::NegativeInfinity => encode_enum(0, None),
            Self::Finite(val) => encode_enum(1, Some(val.to_plutus())),
            Self::PositiveInfinity => encode_enum(2, None),
        }
    }
    fn from_plutus(data: PlutusData) -> Result<Self, decode::Error> {
        match decode_enum(data)? {
            (0, None) => Ok(IntervalBoundType::NegativeInfinity),
            (1, Some(val)) => {
                let time = u64::from_plutus(val)?;
                Ok(IntervalBoundType::Finite(time))
            }
            (2, None) => Ok(IntervalBoundType::PositiveInfinity),
            _ => Err(decode::Error::message("Unexpected IntervalBoundType value")),
        }
    }
}

#[cfg(test)]
mod tests {
    use num_bigint::BigUint;

    use super::{
        super::codec::{deserialize, serialize},
        IntervalBound, SyntheticPriceFeed, Validity,
    };

    #[test]
    fn should_serialize_infinite_validity() {
        let validity = Validity::default();
        let round_tripped = deserialize(&serialize(&validity)).unwrap();
        assert_eq!(validity, round_tripped);
    }

    #[test]
    fn should_serialize_finite_validity() {
        let validity = Validity {
            lower_bound: IntervalBound::unix_timestamp(1000000, true),
            upper_bound: IntervalBound::unix_timestamp(1005000, true),
        };
        let round_tripped = deserialize(&serialize(&validity)).unwrap();
        assert_eq!(validity, round_tripped);
    }

    #[test]
    fn should_serialize_price_feed() {
        // Note: the only thing that doesn't round trip is collateral_names
        let feed = SyntheticPriceFeed {
            collateral_names: None,
            collateral_prices: vec![BigUint::from(1u32), BigUint::from(u128::MAX) * 2u32],
            synthetic: "HIPI".into(),
            denominator: BigUint::from(31337u32),
            validity: Validity::default(),
        };
        let round_tripped = deserialize(&serialize(&feed)).unwrap();
        assert_eq!(feed, round_tripped);
    }
}
