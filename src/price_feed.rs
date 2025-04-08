use std::time::SystemTime;

pub use codec::{
    PlutusCompatible, cbor_encode_in_list, deserialize, serialize, system_time_to_iso,
};
use codec::{decode_struct, encode_struct};
pub use generic::GenericPriceFeed;
use minicbor::{CborLen, Decode, Decoder, Encode, Encoder, decode, encode};
use pallas_primitives::PlutusData;
pub use synthetic::{
    IntervalBound, IntervalBoundType, SyntheticPriceData, SyntheticPriceFeed, Validity,
};

mod codec;
mod generic;
mod synthetic;

#[derive(Clone, Debug)]
pub struct PriceData {
    pub synthetics: Vec<SyntheticPriceData>,
    pub generics: Vec<GenericPriceFeed>,
}

#[derive(Decode, Encode, Clone, Debug)]
pub struct SignedEntries {
    #[n(0)]
    pub timestamp: SystemTime,
    #[n(1)]
    pub synthetics: Vec<SyntheticEntry>,
    #[n(2)]
    #[cbor(default)]
    pub generics: Vec<GenericEntry>,
}

#[derive(Decode, Encode, Clone, Debug)]
pub struct SyntheticEntry {
    #[n(0)]
    pub price: f64,
    #[n(1)]
    pub feed: Signed<SyntheticPriceFeed>,
    #[n(2)]
    pub timestamp: Option<SystemTime>,
}

#[derive(Decode, Encode, Clone, Debug)]
pub struct GenericEntry {
    #[n(0)]
    pub feed: Signed<GenericPriceFeed>,
    #[n(1)]
    pub timestamp: SystemTime,
}

#[derive(Clone, Debug)]
pub struct Signed<T> {
    pub data: T,
    pub signature: Vec<u8>,
}

impl<T: PlutusCompatible> PlutusCompatible for Signed<T> {
    fn to_plutus(&self) -> PlutusData {
        let encoded_data = self.data.to_plutus();
        encode_struct(vec![
            encoded_data,
            PlutusData::BoundedBytes(self.signature.clone().into()),
        ])
    }

    fn from_plutus(data: PlutusData) -> Result<Self, minicbor::decode::Error> {
        let [encoded_data, encoded_signature] = decode_struct(data)?;
        let data = T::from_plutus(encoded_data)?;
        let PlutusData::BoundedBytes(sig_bytes) = encoded_signature else {
            return Err(minicbor::decode::Error::message("Unexpected signature"));
        };
        let signature = sig_bytes.into();
        Ok(Self { data, signature })
    }
}

impl<C, T: PlutusCompatible> Encode<C> for Signed<T> {
    fn encode<W: encode::Write>(
        &self,
        e: &mut Encoder<W>,
        ctx: &mut C,
    ) -> Result<(), encode::Error<W::Error>> {
        self.to_plutus().encode(e, ctx)
    }
}

impl<'b, C, T: PlutusCompatible> Decode<'b, C> for Signed<T> {
    fn decode(d: &mut Decoder<'b>, ctx: &mut C) -> Result<Self, decode::Error> {
        Self::from_plutus(d.decode_with(ctx)?)
    }
}

impl<C, T: PlutusCompatible> CborLen<C> for Signed<T> {
    fn cbor_len(&self, ctx: &mut C) -> usize {
        let mut bytes = vec![];
        minicbor::encode_with(self, &mut bytes, ctx).expect("infallible");
        bytes.len()
    }
}
