use std::{env::current_dir, fs};

use anyhow::Result;
use clap::Parser;
use frost_ed25519::keys::{self as frost_keys, IdentifierList, KeyPackage};
use oracles::keys;
use rand::thread_rng;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[clap(long)]
    max_signers: u16,
    #[clap(long)]
    min_signers: u16,
}

pub fn main() -> Result<()> {
    let args = Args::parse();

    let rng = thread_rng();
    let (shares, pubkey_package) = frost_keys::generate_with_dealer(
        args.max_signers,
        args.min_signers,
        IdentifierList::Default,
        rng,
    )?;

    let keys_path = current_dir()?.join("keys");
    let mut key_hash = None;
    for (index, share) in shares.into_values().enumerate() {
        let keys_dir = keys_path.join(format!("node{}", index));
        fs::create_dir_all(&keys_dir)?;
        let privkey_package: KeyPackage = share.try_into()?;
        key_hash = Some(keys::write_frost_keys(
            &keys_dir,
            privkey_package,
            pubkey_package.clone(),
        )?);
    }

    println!("The new frost public key is: {}", key_hash.unwrap());

    Ok(())
}
