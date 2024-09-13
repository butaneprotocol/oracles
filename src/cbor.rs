use ed25519_dalek::VerifyingKey;
use frost_ed25519::{
    keys::dkg, round1::SigningCommitments, round2::SignatureShare, Identifier, SigningPackage,
};
use minicbor::{
    bytes::{ByteArray, ByteVec},
    decode,
    encode::{self, Write},
    Decode, Decoder, Encode, Encoder,
};
use num_bigint::BigInt;
use num_rational::BigRational;

macro_rules! wrapper {
    ($outer:ident, $inner:ty) => {
        impl From<$inner> for $outer {
            fn from(value: $inner) -> Self {
                Self(value)
            }
        }
        impl From<$outer> for $inner {
            fn from(value: $outer) -> Self {
                value.0
            }
        }
    };
}

#[derive(Clone, Debug)]
pub struct CborVerifyingKey(VerifyingKey);
wrapper!(CborVerifyingKey, VerifyingKey);
impl<C> Encode<C> for CborVerifyingKey {
    fn encode<W: Write>(
        &self,
        e: &mut Encoder<W>,
        ctx: &mut C,
    ) -> Result<(), encode::Error<W::Error>> {
        let bytes: ByteArray<32> = self.0.to_bytes().into();
        bytes.encode(e, ctx)
    }
}
impl<'b, C> Decode<'b, C> for CborVerifyingKey {
    fn decode(d: &mut Decoder<'b>, ctx: &mut C) -> Result<Self, decode::Error> {
        let bytes: ByteArray<32> = d.decode_with(ctx)?;
        let value = VerifyingKey::from_bytes(&bytes).map_err(decode::Error::custom)?;
        Ok(Self(value))
    }
}

#[derive(Clone, Debug)]
pub struct CborEcdhPublicKey(x25519_dalek::PublicKey);
wrapper!(CborEcdhPublicKey, x25519_dalek::PublicKey);
impl<C> Encode<C> for CborEcdhPublicKey {
    fn encode<W: Write>(
        &self,
        e: &mut Encoder<W>,
        ctx: &mut C,
    ) -> Result<(), encode::Error<W::Error>> {
        let bytes: ByteArray<32> = self.0.to_bytes().into();
        bytes.encode(e, ctx)
    }
}
impl<'b, C> Decode<'b, C> for CborEcdhPublicKey {
    fn decode(d: &mut Decoder<'b>, ctx: &mut C) -> Result<Self, decode::Error> {
        let bytes: ByteArray<32> = d.decode_with(ctx)?;
        let value = (*bytes).into();
        Ok(Self(value))
    }
}

#[derive(Clone, Debug)]
pub struct CborSignature(ed25519::Signature);
wrapper!(CborSignature, ed25519::Signature);
impl<C> Encode<C> for CborSignature {
    fn encode<W: Write>(
        &self,
        e: &mut Encoder<W>,
        ctx: &mut C,
    ) -> Result<(), encode::Error<W::Error>> {
        let bytes: ByteArray<64> = self.0.to_bytes().into();
        bytes.encode(e, ctx)
    }
}
impl<'b, C> Decode<'b, C> for CborSignature {
    fn decode(d: &mut Decoder<'b>, ctx: &mut C) -> Result<Self, decode::Error> {
        let bytes: ByteArray<64> = d.decode_with(ctx)?;
        let value = ed25519::Signature::from_bytes(&bytes);
        Ok(Self(value))
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct CborIdentifier(Identifier);
wrapper!(CborIdentifier, Identifier);
impl<C> Encode<C> for CborIdentifier {
    fn encode<W: Write>(
        &self,
        e: &mut Encoder<W>,
        ctx: &mut C,
    ) -> Result<(), encode::Error<W::Error>> {
        let bytes: ByteVec = self.0.serialize().into();
        bytes.encode(e, ctx)
    }
}
impl<'b, C> Decode<'b, C> for CborIdentifier {
    fn decode(d: &mut Decoder<'b>, ctx: &mut C) -> Result<Self, decode::Error> {
        let bytes: ByteVec = d.decode_with(ctx)?;
        let value = Identifier::deserialize(&bytes).map_err(decode::Error::custom)?;
        Ok(Self(value))
    }
}

#[derive(Clone, Copy, Debug)]
pub struct CborSigningCommitments(SigningCommitments);
wrapper!(CborSigningCommitments, SigningCommitments);
impl<C> Encode<C> for CborSigningCommitments {
    fn encode<W: Write>(
        &self,
        e: &mut Encoder<W>,
        ctx: &mut C,
    ) -> Result<(), encode::Error<W::Error>> {
        let bytes: ByteVec = self.0.serialize().map_err(encode::Error::custom)?.into();
        bytes.encode(e, ctx)
    }
}
impl<'b, C> Decode<'b, C> for CborSigningCommitments {
    fn decode(d: &mut Decoder<'b>, ctx: &mut C) -> Result<Self, decode::Error> {
        let bytes: ByteVec = d.decode_with(ctx)?;
        let value = SigningCommitments::deserialize(&bytes).map_err(decode::Error::custom)?;
        Ok(Self(value))
    }
}

#[derive(Clone, Copy, Debug)]
pub struct CborSignatureShare(SignatureShare);
wrapper!(CborSignatureShare, SignatureShare);
impl<C> Encode<C> for CborSignatureShare {
    fn encode<W: Write>(
        &self,
        e: &mut Encoder<W>,
        ctx: &mut C,
    ) -> Result<(), encode::Error<W::Error>> {
        let bytes: ByteVec = self.0.serialize().into();
        bytes.encode(e, ctx)
    }
}
impl<'b, C> Decode<'b, C> for CborSignatureShare {
    fn decode(d: &mut Decoder<'b>, ctx: &mut C) -> Result<Self, decode::Error> {
        let bytes: ByteVec = d.decode_with(ctx)?;
        let value = SignatureShare::deserialize(&bytes).map_err(decode::Error::custom)?;
        Ok(Self(value))
    }
}

#[derive(Clone, Debug)]
pub struct CborSigningPackage(SigningPackage);
wrapper!(CborSigningPackage, SigningPackage);
impl<C> Encode<C> for CborSigningPackage {
    fn encode<W: Write>(
        &self,
        e: &mut Encoder<W>,
        ctx: &mut C,
    ) -> Result<(), encode::Error<W::Error>> {
        let bytes: ByteVec = self.0.serialize().map_err(encode::Error::custom)?.into();
        bytes.encode(e, ctx)
    }
}
impl<'b, C> Decode<'b, C> for CborSigningPackage {
    fn decode(d: &mut Decoder<'b>, ctx: &mut C) -> Result<Self, decode::Error> {
        let bytes: ByteVec = d.decode_with(ctx)?;
        let value = SigningPackage::deserialize(&bytes).map_err(decode::Error::custom)?;
        Ok(Self(value))
    }
}

#[derive(Clone, Debug)]
pub struct CborDkgRound1Package(dkg::round1::Package);
wrapper!(CborDkgRound1Package, dkg::round1::Package);
impl<C> Encode<C> for CborDkgRound1Package {
    fn encode<W: Write>(
        &self,
        e: &mut Encoder<W>,
        ctx: &mut C,
    ) -> Result<(), encode::Error<W::Error>> {
        let bytes: ByteVec = self.0.serialize().map_err(encode::Error::custom)?.into();
        bytes.encode(e, ctx)
    }
}
impl<'b, C> Decode<'b, C> for CborDkgRound1Package {
    fn decode(d: &mut Decoder<'b>, ctx: &mut C) -> Result<Self, decode::Error> {
        let bytes: ByteVec = d.decode_with(ctx)?;
        let value = dkg::round1::Package::deserialize(&bytes).map_err(decode::Error::custom)?;
        Ok(Self(value))
    }
}

#[derive(Clone, Debug)]
pub struct CborDkgRound2Package(dkg::round2::Package);
wrapper!(CborDkgRound2Package, dkg::round2::Package);
impl<C> Encode<C> for CborDkgRound2Package {
    fn encode<W: Write>(
        &self,
        e: &mut Encoder<W>,
        ctx: &mut C,
    ) -> Result<(), encode::Error<W::Error>> {
        let bytes: ByteVec = self.0.serialize().map_err(encode::Error::custom)?.into();
        bytes.encode(e, ctx)
    }
}
impl<'b, C> Decode<'b, C> for CborDkgRound2Package {
    fn decode(d: &mut Decoder<'b>, ctx: &mut C) -> Result<Self, decode::Error> {
        let bytes: ByteVec = d.decode_with(ctx)?;
        let value = dkg::round2::Package::deserialize(&bytes).map_err(decode::Error::custom)?;
        Ok(Self(value))
    }
}

pub struct CborBigRational(BigRational);
impl From<BigRational> for CborBigRational {
    fn from(value: BigRational) -> Self {
        Self(value)
    }
}
impl From<CborBigRational> for BigRational {
    fn from(value: CborBigRational) -> Self {
        value.0
    }
}

impl<C> Encode<C> for CborBigRational {
    fn encode<W: Write>(
        &self,
        e: &mut minicbor::Encoder<W>,
        _ctx: &mut C,
    ) -> Result<(), minicbor::encode::Error<W::Error>> {
        e.array(2)?
            .bytes(&self.0.numer().to_signed_bytes_le())?
            .bytes(&self.0.denom().to_signed_bytes_le())?;
        Ok(())
    }
}

impl<'a, C> Decode<'a, C> for CborBigRational {
    fn decode(
        d: &mut minicbor::Decoder<'a>,
        _ctx: &mut C,
    ) -> Result<Self, minicbor::decode::Error> {
        let [n_bytes, d_bytes]: [ByteVec; 2] = d.decode()?;
        let numer = BigInt::from_signed_bytes_le(&n_bytes);
        let denom = BigInt::from_signed_bytes_le(&d_bytes);
        Ok(Self(BigRational::new(numer, denom)))
    }
}
