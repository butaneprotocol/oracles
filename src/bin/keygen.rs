use std::{env::current_dir, fs};

use anyhow::Result;
use clap::Parser;
use frost_ed25519::keys::{self, IdentifierList, KeyPackage};
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
    let (shares, pubkey_package) = keys::generate_with_dealer(
        args.max_signers,
        args.min_signers,
        IdentifierList::Default,
        rng,
    )?;

    let keys_path = current_dir()?.join("keys");

    fs::create_dir_all(&keys_path)?;

    let pubkey_path = keys_path.join("frost_public");
    fs::write(pubkey_path, pubkey_package.serialize()?)?;

    for (index, share) in shares.into_values().enumerate() {
        let privkey_path = keys_path.join(format!("frost_private_{}", index));
        let privkey_package: KeyPackage = share.try_into()?;
        fs::write(privkey_path, privkey_package.serialize()?)?;
    }

    Ok(())
}
