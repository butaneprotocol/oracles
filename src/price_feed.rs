use minicbor::{
    data::{Tag, Type},
    decode, Decode, Decoder,
};
use num_bigint::BigUint;
use pallas_primitives::conway::{BigInt, Constr, PlutusData};
use rust_decimal::{prelude::ToPrimitive, Decimal};

#[derive(Clone, Debug)]
pub struct PriceFeedEntry {
    pub price: Decimal,
    pub data: PriceFeed,
}

#[derive(Debug)]
pub struct SignedPriceFeedEntry {
    pub price: Decimal,
    pub data: SignedPriceFeed,
}

#[derive(Debug)]
pub struct SignedPriceFeed {
    pub data: PriceFeed,
    pub signature: Vec<u8>,
}
impl From<&SignedPriceFeed> for PlutusData {
    fn from(value: &SignedPriceFeed) -> Self {
        let encoded_data: PlutusData = (&value.data).into();
        encode_struct(vec![
            encoded_data,
            PlutusData::BoundedBytes(value.signature.clone().into()),
        ])
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PriceFeed {
    pub collateral_prices: Vec<BigUint>,
    pub synthetic: String,
    pub denominator: BigUint,
    pub validity: Validity,
}
impl From<&PriceFeed> for PlutusData {
    fn from(value: &PriceFeed) -> Self {
        encode_struct(vec![
            PlutusData::Array(value.collateral_prices.iter().map(encode_bigint).collect()),
            PlutusData::BoundedBytes(value.synthetic.as_bytes().to_vec().into()),
            encode_bigint(&value.denominator),
            value.validity.into(),
        ])
    }
}

impl<'b, C> Decode<'b, C> for PriceFeed {
    fn decode(d: &mut Decoder<'b>, ctx: &mut C) -> Result<Self, decode::Error> {
        decode_struct_begin(d)?;

        let mut collateral_prices = vec![];
        let cols: Vec<BigInt> = d.decode_with(ctx)?;
        for col in cols {
            collateral_prices.push(decode_bigint(&col)?);
        }

        let synthetic = std::str::from_utf8(d.bytes()?)
            .map_err(|err| decode::Error::message(err.to_string()))?
            .to_string();

        let denominator = decode_bigint(&d.decode_with(ctx)?)?;

        let validity = d.decode_with(ctx)?;

        decode_struct_end(d)?;

        Ok(PriceFeed {
            collateral_prices,
            synthetic,
            denominator,
            validity,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
impl<'b, C> Decode<'b, C> for Validity {
    fn decode(d: &mut Decoder<'b>, ctx: &mut C) -> Result<Self, decode::Error> {
        decode_struct_begin(d)?;
        let lower_bound = d.decode_with(ctx)?;
        let upper_bound = d.decode_with(ctx)?;
        decode_struct_end(d)?;

        Ok(Validity {
            lower_bound,
            upper_bound,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
impl<'b, C> Decode<'b, C> for IntervalBound {
    fn decode(d: &mut Decoder<'b>, ctx: &mut C) -> Result<Self, decode::Error> {
        decode_struct_begin(d)?;
        let bound_type = d.decode_with(ctx)?;
        let is_inclusive: bool = match decode_enum::<C, ()>(d, ctx)? {
            (1, _) => true,
            (0, _) => false,
            _ => return Err(decode::Error::message("Unexpected bool value")),
        };
        decode_struct_end(d)?;

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
impl From<IntervalBoundType> for PlutusData {
    fn from(val: IntervalBoundType) -> Self {
        match val {
            IntervalBoundType::NegativeInfinity => encode_enum(0, None),
            IntervalBoundType::Finite(val) => encode_enum(1, Some(encode_bigint(&val.into()))),
            IntervalBoundType::PositiveInfinity => encode_enum(2, None),
        }
    }
}
impl<'b, C> Decode<'b, C> for IntervalBoundType {
    fn decode(d: &mut Decoder<'b>, ctx: &mut C) -> Result<Self, decode::Error> {
        match decode_enum(d, ctx)? {
            (0, None) => Ok(IntervalBoundType::NegativeInfinity),
            (1, Some(val)) => Ok(IntervalBoundType::Finite(val)),
            (2, None) => Ok(IntervalBoundType::PositiveInfinity),
            _ => Err(decode::Error::message("Unexpected IntervalBoundType value")),
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
fn decode_bigint(value: &BigInt) -> Result<BigUint, decode::Error> {
    match value {
        BigInt::Int(val) => {
            let raw: i128 = (*val).into();
            Ok(BigUint::from(raw as u128))
        }
        BigInt::BigUInt(val) => Ok(BigUint::from_bytes_be(val)),
        BigInt::BigNInt(_) => Err(decode::Error::message("Unexpected signed integer")),
    }
}

fn encode_struct(fields: Vec<PlutusData>) -> PlutusData {
    PlutusData::Constr(Constr {
        tag: CBOR_VARIANT_0,
        any_constructor: None,
        fields,
    })
}

fn decode_struct_begin(d: &mut Decoder) -> Result<(), decode::Error> {
    let Tag::Unassigned(CBOR_VARIANT_0) = d.tag()? else {
        return Err(decode::Error::message("Missing plutus struct tag"));
    };
    d.array()?;
    Ok(())
}
fn decode_struct_end(d: &mut Decoder) -> Result<(), decode::Error> {
    let Type::Break = d.datatype()? else {
        return Err(decode::Error::message("Expected end of struct"));
    };
    d.skip()
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

fn decode_enum<'b, C, V: Decode<'b, C>>(
    d: &mut Decoder<'b>,
    ctx: &mut C,
) -> Result<(u64, Option<V>), decode::Error> {
    let Tag::Unassigned(variant) = d.tag()? else {
        return Err(decode::Error::message("Missing plutus struct tag"));
    };
    let result = match d.array()? {
        Some(0) => (variant - CBOR_VARIANT_0, None),
        _ => (variant - CBOR_VARIANT_0, Some(d.decode_with(ctx)?)),
    };
    Ok(result)
}
