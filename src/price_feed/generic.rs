use num_bigint::BigUint;

use super::{
    codec::{decode_struct, encode_struct},
    PlutusCompatible,
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
