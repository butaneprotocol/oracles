use anyhow::{Context, Result};
use bech32::{Bech32, Hrp};
use frost_ed25519::keys::{KeyPackage, PublicKeyPackage};
use pallas_crypto::hash::Hasher;
use std::{
    env::{self, VarError},
    fs,
    path::{Path, PathBuf},
};

pub fn get_keys_directory() -> Result<PathBuf> {
    let dir: PathBuf = match env::var("KEYS_DIRECTORY") {
        Ok(path) => path.into(),
        Err(VarError::NotUnicode(path)) => path.into(),
        Err(VarError::NotPresent) => "keys".into(),
    };
    if dir.is_absolute() {
        return Ok(dir);
    }
    let pwd = env::current_dir().context("could not find keys directory")?;
    Ok(pwd.join(dir))
}

pub fn read_frost_keys(
    keys_dir: &Path,
    public_key_hash: &str,
) -> Result<(KeyPackage, PublicKeyPackage)> {
    let frost_key_path = keys_dir.join(public_key_hash);

    let private_key_path = frost_key_path.join("frost.skey");
    let private_key_bytes = fs::read(&private_key_path).context(format!(
        "Could not find frost private key in {}",
        private_key_path.display()
    ))?;
    let private_key =
        KeyPackage::deserialize(&private_key_bytes).context("Could not decode private key")?;

    let public_key_path = frost_key_path.join("frost.vkey");
    let public_key_bytes = fs::read(&public_key_path).context(format!(
        "Could not find frost public key in {}",
        public_key_path.display()
    ))?;
    let public_key =
        PublicKeyPackage::deserialize(&public_key_bytes).context("Could not decode public key")?;

    Ok((private_key, public_key))
}

pub fn write_frost_keys(
    keys_dir: &Path,
    private_key: KeyPackage,
    public_key: PublicKeyPackage,
) -> Result<String> {
    let verifying_key = public_key.verifying_key();
    let address = compute_address(&verifying_key.serialize()?)?;
    let frost_key_path = keys_dir.join(&address);
    fs::create_dir_all(&frost_key_path)?;

    let private_key_bytes = private_key.serialize()?;
    let public_key_bytes = public_key.serialize()?;
    fs::write(frost_key_path.join("frost.skey"), private_key_bytes)?;
    fs::write(frost_key_path.join("frost.vkey"), public_key_bytes)?;
    Ok(address)
}

const ADDR_HRP: Hrp = Hrp::parse_unchecked("addr");

fn compute_address(verifying_key: &[u8]) -> Result<String> {
    let blake2b = Hasher::<224>::hash(verifying_key);
    let mut bytes = vec![0x61];
    bytes.extend_from_slice(blake2b.as_ref());
    Ok(bech32::encode::<Bech32>(ADDR_HRP, &bytes)?)
}
