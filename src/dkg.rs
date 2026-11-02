mod logic;
#[cfg(test)]
mod tests;

use std::path::PathBuf;

use logic::GeneratedKeys;
pub use logic::KeygenMessage;
use tracing::info;

use crate::{
    config::OracleConfig,
    keys,
    network::{ChannelRole, Network, NetworkChannel, NodeId},
};
use anyhow::{Result, anyhow};

pub struct Keygen {
    id: NodeId,
    channel: NetworkChannel<KeygenMessage>,
    max_signers: u16,
    min_signers: u16,
    keys_dir: PathBuf,
}

impl Keygen {
    pub fn new(config: &OracleConfig, network: &mut Network) -> Result<Self> {
        let id = network.id.clone();
        let channel = network.keygen_channel();
        let max_signers = config
            .network
            .peers
            .iter()
            .filter(|peer| peer.role.matches(ChannelRole::Dkg))
            .count() as u16
            + 1;
        let min_signers = config
            .keygen
            .min_signers
            .ok_or_else(|| anyhow!("Must specify min_signers"))?;
        let keys_dir = keys::get_keys_directory()?;
        Ok(Self {
            id,
            channel,
            max_signers,
            min_signers,
            keys_dir,
        })
    }

    pub async fn run(self) -> Result<()> {
        let Self {
            id,
            channel,
            max_signers,
            min_signers,
            keys_dir,
        } = self;

        info!(
            "Running in DKG mode. Will generate frost keys into {}.",
            keys_dir.display()
        );

        let GeneratedKeys(private_key, public_key) =
            logic::run(id, channel, max_signers, min_signers).await?;
        let address = keys::write_frost_keys(&keys_dir, private_key, public_key)?;
        info!("The new frost address is: {address}");

        Ok(())
    }
}
