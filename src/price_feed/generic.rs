use minicbor::{Decode, Decoder, Encode, Encoder, decode, encode};
use num_bigint::BigUint;

use super::{
    PlutusCompatible,
    codec::{decode_struct, encode_struct},
};

#[derive(Clone, Debug)]
pub struct GenericPriceFeed {
    pub price: BigUint,
    pub name: String,
    pub timestamp: u64,
}

impl PlutusCompatible for GenericPriceFeed {
    fn to_plutus(&self) -> pallas_primitives::PlutusData {
        encode_struct(vec![
            self.price.to_plutus(),
            self.name.to_plutus(),
            self.timestamp.to_plutus(),
        ])
    }

    fn from_plutus(data: pallas_primitives::PlutusData) -> Result<Self, minicbor::decode::Error> {
        let [encoded_price, encoded_name, encoded_timestamp] = decode_struct(data)?;
        let price = BigUint::from_plutus(encoded_price)?;
        let name = String::from_plutus(encoded_name)?;
        let timestamp = u64::from_plutus(encoded_timestamp)?;
        Ok(Self {
            price,
            name,
            timestamp,
        })
    }
}

impl<C> Encode<C> for GenericPriceFeed {
    fn encode<W: encode::Write>(
        &self,
        e: &mut Encoder<W>,
        ctx: &mut C,
    ) -> Result<(), encode::Error<W::Error>> {
        self.to_plutus().encode(e, ctx)
    }
}

impl<'b, C> Decode<'b, C> for GenericPriceFeed {
    fn decode(d: &mut Decoder<'b>, ctx: &mut C) -> Result<Self, decode::Error> {
        Self::from_plutus(d.decode_with(ctx)?)
    }
}
