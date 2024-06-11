use ed25519_dalek::VerifyingKey;
use frost_ed25519::{
    round1::SigningCommitments, round2::SignatureShare, Identifier, SigningPackage,
};
use minicbor::{
    bytes::{ByteArray, ByteVec},
    decode,
    encode::{self, Write},
    Decode, Decoder, Encode, Encoder,
};

#[derive(Clone, Debug)]
pub struct CborVerifyingKey(VerifyingKey);
impl From<VerifyingKey> for CborVerifyingKey {
    fn from(value: VerifyingKey) -> Self {
        Self(value)
    }
}
impl From<CborVerifyingKey> for VerifyingKey {
    fn from(value: CborVerifyingKey) -> Self {
        value.0
    }
}
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
impl From<x25519_dalek::PublicKey> for CborEcdhPublicKey {
    fn from(value: x25519_dalek::PublicKey) -> Self {
        Self(value)
    }
}
impl From<CborEcdhPublicKey> for x25519_dalek::PublicKey {
    fn from(value: CborEcdhPublicKey) -> Self {
        value.0
    }
}
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
impl From<ed25519::Signature> for CborSignature {
    fn from(value: ed25519::Signature) -> Self {
        Self(value)
    }
}
impl From<CborSignature> for ed25519::Signature {
    fn from(value: CborSignature) -> Self {
        value.0
    }
}
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
impl From<Identifier> for CborIdentifier {
    fn from(value: Identifier) -> Self {
        Self(value)
    }
}
impl From<CborIdentifier> for Identifier {
    fn from(value: CborIdentifier) -> Self {
        value.0
    }
}
impl<C> Encode<C> for CborIdentifier {
    fn encode<W: Write>(
        &self,
        e: &mut Encoder<W>,
        ctx: &mut C,
    ) -> Result<(), encode::Error<W::Error>> {
        let bytes: ByteArray<32> = self.0.serialize().into();
        bytes.encode(e, ctx)
    }
}
impl<'b, C> Decode<'b, C> for CborIdentifier {
    fn decode(d: &mut Decoder<'b>, ctx: &mut C) -> Result<Self, decode::Error> {
        let bytes: ByteArray<32> = d.decode_with(ctx)?;
        let value = Identifier::deserialize(&bytes).map_err(decode::Error::custom)?;
        Ok(Self(value))
    }
}

#[derive(Clone, Copy, Debug)]
pub struct CborSigningCommitments(SigningCommitments);
impl From<SigningCommitments> for CborSigningCommitments {
    fn from(value: SigningCommitments) -> Self {
        Self(value)
    }
}
impl From<CborSigningCommitments> for SigningCommitments {
    fn from(value: CborSigningCommitments) -> Self {
        value.0
    }
}
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
impl From<SignatureShare> for CborSignatureShare {
    fn from(value: SignatureShare) -> Self {
        Self(value)
    }
}
impl From<CborSignatureShare> for SignatureShare {
    fn from(value: CborSignatureShare) -> Self {
        value.0
    }
}
impl<C> Encode<C> for CborSignatureShare {
    fn encode<W: Write>(
        &self,
        e: &mut Encoder<W>,
        ctx: &mut C,
    ) -> Result<(), encode::Error<W::Error>> {
        let bytes: ByteArray<32> = self.0.serialize().into();
        bytes.encode(e, ctx)
    }
}
impl<'b, C> Decode<'b, C> for CborSignatureShare {
    fn decode(d: &mut Decoder<'b>, ctx: &mut C) -> Result<Self, decode::Error> {
        let bytes: ByteArray<32> = d.decode_with(ctx)?;
        let value = SignatureShare::deserialize(*bytes).map_err(decode::Error::custom)?;
        Ok(Self(value))
    }
}

#[derive(Clone, Debug)]
pub struct CborSigningPackage(SigningPackage);
impl From<SigningPackage> for CborSigningPackage {
    fn from(value: SigningPackage) -> Self {
        Self(value)
    }
}
impl From<CborSigningPackage> for SigningPackage {
    fn from(value: CborSigningPackage) -> Self {
        value.0
    }
}
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
