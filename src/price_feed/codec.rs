use minicbor::{decode, Decoder, Encoder};
use num_bigint::BigUint;
use pallas_primitives::{BigInt, Constr, MaybeIndefArray, PlutusData};
use rust_decimal::prelude::ToPrimitive;

pub trait PlutusCompatible: Sized {
    fn to_plutus(&self) -> PlutusData;
    fn from_plutus(data: PlutusData) -> Result<Self, decode::Error>;
}

impl<T: PlutusCompatible> PlutusCompatible for Vec<T> {
    fn to_plutus(&self) -> PlutusData {
        PlutusData::Array(MaybeIndefArray::Indef(
            self.iter().map(|i| i.to_plutus()).collect(),
        ))
    }
    fn from_plutus(data: PlutusData) -> Result<Self, decode::Error> {
        let PlutusData::Array(array) = data else {
            return Err(decode::Error::message(
                "Unexpected plutus type, expected array",
            ));
        };
        array
            .to_vec()
            .into_iter()
            .map(|d| T::from_plutus(d))
            .collect()
    }
}

impl PlutusCompatible for BigUint {
    fn to_plutus(&self) -> PlutusData {
        PlutusData::BigInt(match self.to_i64() {
            Some(val) => BigInt::Int(val.into()),
            None => BigInt::BigUInt(self.to_bytes_be().into()),
        })
    }

    fn from_plutus(data: PlutusData) -> Result<Self, decode::Error> {
        let PlutusData::BigInt(value) = data else {
            return Err(decode::Error::message(
                "Unexpected plutus type, expected bigint",
            ));
        };
        match value {
            BigInt::Int(val) => {
                let raw: i128 = val.into();
                Ok(BigUint::from(raw as u128))
            }
            BigInt::BigUInt(val) => Ok(BigUint::from_bytes_be(&val)),
            BigInt::BigNInt(_) => Err(decode::Error::message("Unexpected signed integer")),
        }
    }
}

impl PlutusCompatible for u64 {
    fn to_plutus(&self) -> PlutusData {
        BigUint::from(*self).to_plutus()
    }

    fn from_plutus(data: PlutusData) -> Result<Self, decode::Error> {
        let bigint = BigUint::from_plutus(data)?;
        let Some(value) = bigint.to_u64() else {
            return Err(decode::Error::message("Value out of range"));
        };
        Ok(value)
    }
}

impl PlutusCompatible for bool {
    fn to_plutus(&self) -> PlutusData {
        encode_enum(if *self { 1 } else { 0 }, None)
    }

    fn from_plutus(data: PlutusData) -> Result<Self, decode::Error> {
        match decode_enum(data)? {
            (1, None) => Ok(true),
            (0, None) => Ok(false),
            _ => Err(decode::Error::message("Unexpected bool value")),
        }
    }
}

impl PlutusCompatible for String {
    fn to_plutus(&self) -> PlutusData {
        PlutusData::BoundedBytes(self.as_bytes().to_vec().into())
    }

    fn from_plutus(data: PlutusData) -> Result<Self, decode::Error> {
        let PlutusData::BoundedBytes(bytes) = data else {
            return Err(decode::Error::message(
                "Unexpected plutus type, expected bytes",
            ));
        };
        String::from_utf8(bytes.to_vec()).map_err(|e| decode::Error::message(e.to_string()))
    }
}

pub fn serialize<T: PlutusCompatible>(data: &T) -> Vec<u8> {
    let plutus: PlutusData = data.to_plutus();
    let mut encoder = Encoder::new(vec![]);
    encoder.encode(&plutus).unwrap(); //infallible
    encoder.into_writer()
}

pub fn deserialize<T: PlutusCompatible>(bytes: &[u8]) -> Result<T, decode::Error> {
    let mut decoder = Decoder::new(bytes);
    let data: PlutusData = decoder.decode()?;
    T::from_plutus(data)
}

const CBOR_VARIANT_0: u64 = 121;

pub fn encode_struct(fields: Vec<PlutusData>) -> PlutusData {
    PlutusData::Constr(Constr {
        tag: CBOR_VARIANT_0,
        any_constructor: None,
        fields: MaybeIndefArray::Indef(fields),
    })
}

pub fn decode_struct<const N: usize>(data: PlutusData) -> Result<[PlutusData; N], decode::Error> {
    let PlutusData::Constr(Constr { tag, fields, .. }) = data else {
        return Err(decode::Error::message(
            "Unexpected plutus type, expected struct",
        ));
    };
    if tag != CBOR_VARIANT_0 {
        return Err(decode::Error::message(format!(
            "Invalid plutus struct tag {tag}"
        )));
    }
    fields.to_vec().try_into().map_err(|e: Vec<PlutusData>| {
        decode::Error::message(format!("Expected {N} field(s), found {}", e.len()))
    })
}

pub fn encode_enum(variant: u64, value: Option<PlutusData>) -> PlutusData {
    PlutusData::Constr(Constr {
        tag: CBOR_VARIANT_0 + variant,
        any_constructor: None,
        fields: if let Some(val) = value {
            MaybeIndefArray::Indef(vec![val])
        } else {
            MaybeIndefArray::Def(vec![])
        },
    })
}

pub fn decode_enum(data: PlutusData) -> Result<(u64, Option<PlutusData>), decode::Error> {
    let PlutusData::Constr(Constr { tag, fields, .. }) = data else {
        return Err(decode::Error::message(
            "Unexpected plutus type, expected struct",
        ));
    };
    let Some(variant) = tag.checked_sub(CBOR_VARIANT_0) else {
        return Err(decode::Error::message("Invalid plutus struct tag"));
    };
    let mut data = fields.to_vec();
    if data.len() > 1 {
        return Err(decode::Error::message("Enum has too much data"));
    }

    Ok((variant, data.pop()))
}

pub fn cbor_encode_in_list<S: serde::Serializer, T: PlutusCompatible>(
    feed: &T,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let bytes = {
        let data = PlutusData::Array(MaybeIndefArray::Indef(vec![feed.to_plutus()]));
        let mut encoder = Encoder::new(vec![]);
        encoder.encode(data).expect("encoding is infallible");
        encoder.into_writer()
    };
    serializer.serialize_str(&hex::encode(bytes))
}
