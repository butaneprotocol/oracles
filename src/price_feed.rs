use num_bigint::BigUint;
use pallas_primitives::conway::{BigInt, Constr, PlutusData};
use rust_decimal::{prelude::ToPrimitive, Decimal};

#[derive(Clone, Debug)]
pub struct PriceFeedEntry {
    pub price: Decimal,
    pub data: PriceFeed,
}

#[derive(Debug)]
pub struct SignedPriceFeed {
    pub data: PlutusData,
    pub signature: Vec<u8>,
}
impl From<SignedPriceFeed> for PlutusData {
    fn from(value: SignedPriceFeed) -> Self {
        encode_struct(vec![
            value.data,
            PlutusData::BoundedBytes(value.signature.into()),
        ])
    }
}

#[derive(Clone, Debug)]
pub struct PriceFeed {
    pub collateral_prices: Vec<BigUint>,
    pub synthetic: String,
    pub denominator: BigUint,
    pub validity: Validity,
}
impl From<PriceFeed> for PlutusData {
    fn from(value: PriceFeed) -> Self {
        encode_struct(vec![
            PlutusData::Array(value.collateral_prices.iter().map(encode_bigint).collect()),
            PlutusData::BoundedBytes(value.synthetic.as_bytes().to_vec().into()),
            encode_bigint(&value.denominator),
            value.validity.into(),
        ])
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Validity {
    lower_bound: IntervalBound,
    upper_bound: IntervalBound,
}
impl Default for Validity {
    fn default() -> Self {
        Self {
            lower_bound: IntervalBound {
                bound_type: IntervalBoundType::NegativeInfinity,
                is_inclusive: false,
            },
            upper_bound: IntervalBound {
                bound_type: IntervalBoundType::PositiveInfinity,
                is_inclusive: false,
            },
        }
    }
}
impl From<Validity> for PlutusData {
    fn from(val: Validity) -> Self {
        encode_struct(vec![val.lower_bound.into(), val.upper_bound.into()])
    }
}

#[derive(Clone, Copy, Debug)]
pub struct IntervalBound {
    bound_type: IntervalBoundType,
    is_inclusive: bool,
}
impl From<IntervalBound> for PlutusData {
    fn from(val: IntervalBound) -> Self {
        encode_struct(vec![
            val.bound_type.into(),
            encode_enum(if val.is_inclusive { 1 } else { 0 }, None),
        ])
    }
}

#[derive(Clone, Copy, Debug)]
pub enum IntervalBoundType {
    NegativeInfinity,
    #[allow(unused)]
    Finite(u64),
    PositiveInfinity,
}
impl From<IntervalBoundType> for PlutusData {
    fn from(val: IntervalBoundType) -> Self {
        match val {
            IntervalBoundType::NegativeInfinity => encode_enum(0, None),
            IntervalBoundType::Finite(val) => encode_enum(1, Some(encode_bigint(&val.into()))),
            IntervalBoundType::PositiveInfinity => encode_enum(2, None),
        }
    }
}

const CBOR_VARIANT_0: u64 = 121;

fn encode_bigint(value: &BigUint) -> PlutusData {
    PlutusData::BigInt(match value.to_i64() {
        Some(val) => BigInt::Int(val.into()),
        None => BigInt::BigUInt(value.to_bytes_be().into()),
    })
}

fn encode_struct(fields: Vec<PlutusData>) -> PlutusData {
    PlutusData::Constr(Constr {
        tag: CBOR_VARIANT_0,
        any_constructor: None,
        fields,
    })
}

fn encode_enum(variant: u64, value: Option<PlutusData>) -> PlutusData {
    PlutusData::Constr(Constr {
        tag: CBOR_VARIANT_0 + variant,
        any_constructor: None,
        fields: if let Some(val) = value {
            vec![val]
        } else {
            vec![]
        },
    })
}
