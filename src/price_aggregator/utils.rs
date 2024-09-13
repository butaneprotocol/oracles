use num_bigint::BigInt;
use num_rational::BigRational;
use num_traits::ToPrimitive as _;
use rust_decimal::Decimal;
use serde::Serializer;

pub fn decimal_to_rational(value: Decimal) -> BigRational {
    let numer = BigInt::from(value.mantissa());
    let denom = BigInt::from(10i128.pow(value.scale()));
    BigRational::new(numer, denom)
}

pub fn serialize_rational_as_decimal<S: Serializer>(
    value: &BigRational,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let float = value.to_f64().unwrap();
    serializer.collect_str(&float.to_string())
}
