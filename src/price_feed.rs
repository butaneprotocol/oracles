use std::time::{SystemTime, UNIX_EPOCH};

use minicbor::{
    data::{Tag, Type},
    decode,
    encode::{self, Write},
    CborLen, Decode, Decoder, Encode, Encoder,
};
use num_bigint::BigUint;
use num_rational::BigRational;
use pallas_primitives::conway::{BigInt, Constr, PlutusData};
use rust_decimal::prelude::ToPrimitive;

pub fn serialize<T: Into<PlutusData>>(data: T) -> Vec<u8> {
    let plutus: PlutusData = data.into();
    let mut encoder = Encoder::new(vec![]);
    encoder.encode(&plutus).unwrap(); //infallible
    encoder.into_writer()
}

pub fn deserialize<'a, T: Decode<'a, ()>>(bytes: &'a [u8]) -> Result<T, minicbor::decode::Error> {
    let mut decoder = Decoder::new(bytes);
    decoder.decode()
}

#[derive(Decode, Encode, Clone, Debug)]
pub struct SignedEntries {
    #[n(0)]
    pub timestamp: SystemTime,
    #[n(1)]
    pub entries: Vec<SignedEntry>,
}

#[derive(Decode, Encode, Clone, Debug)]
pub struct SignedEntry {
    #[n(0)]
    pub price: f64,
    #[n(1)]
    pub data: SignedPriceFeed,
    #[n(2)]
    pub timestamp: Option<SystemTime>,
}

#[derive(Clone, Debug)]
pub struct PriceFeedEntry {
    pub price: BigRational,
    pub data: PriceFeed,
}

#[derive(Clone, Debug)]
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
impl<C> Encode<C> for SignedPriceFeed {
    fn encode<W: Write>(
        &self,
        e: &mut Encoder<W>,
        ctx: &mut C,
    ) -> Result<(), encode::Error<W::Error>> {
        let plutus_data: PlutusData = self.into();
        plutus_data.encode(e, ctx)
    }
}
impl<'b, C> Decode<'b, C> for SignedPriceFeed {
    fn decode(d: &mut Decoder<'b>, ctx: &mut C) -> Result<Self, decode::Error> {
        decode_struct_begin(d)?;

        let data = d.decode_with(ctx)?;
        let signature = d.bytes()?.to_vec();

        decode_struct_end(d)?;

        Ok(SignedPriceFeed { data, signature })
    }
}
impl<C> CborLen<C> for SignedPriceFeed {
    fn cbor_len(&self, ctx: &mut C) -> usize {
        let mut bytes = vec![];
        minicbor::encode_with(self, &mut bytes, ctx).expect("infallible");
        bytes.len()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PriceFeed {
    pub collateral_names: Option<Vec<String>>,
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

impl<C> Encode<C> for PriceFeed {
    fn encode<W: Write>(
        &self,
        e: &mut Encoder<W>,
        ctx: &mut C,
    ) -> Result<(), encode::Error<W::Error>> {
        let plutus_data: PlutusData = self.into();
        plutus_data.encode(e, ctx)
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
            collateral_names: None,
            collateral_prices,
            synthetic,
            denominator,
            validity,
        })
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
    let datum = match d.array()? {
        Some(0) => None,
        Some(1) => d.decode_with(ctx)?,
        Some(_) => return Err(decode::Error::message("Enum has too much data")),
        None => {
            if let Type::Break = d.datatype()? {
                d.skip()?;
                None
            } else {
                let datum = d.decode_with(ctx)?;
                let Type::Break = d.datatype()? else {
                    return Err(decode::Error::message("Enum has too much data"));
                };
                d.skip()?;
                Some(datum)
            }
        }
    };
    Ok((variant - CBOR_VARIANT_0, datum))
}

#[cfg(test)]
mod tests {
    use num_bigint::BigUint;

    use super::{deserialize, serialize, IntervalBound, PriceFeed, Validity};

    #[test]
    fn should_serialize_infinite_validity() {
        let validity = Validity::default();
        let round_tripped = deserialize(&serialize(validity)).unwrap();
        assert_eq!(validity, round_tripped);
    }

    #[test]
    fn should_serialize_finite_validity() {
        let validity = Validity {
            lower_bound: IntervalBound::unix_timestamp(1000000, true),
            upper_bound: IntervalBound::unix_timestamp(1005000, true),
        };
        let round_tripped = deserialize(&serialize(validity)).unwrap();
        assert_eq!(validity, round_tripped);
    }

    #[test]
    fn should_serialize_price_feed() {
        // Note: the only thing that doesn't round trip is collateral_names
        let feed = PriceFeed {
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
