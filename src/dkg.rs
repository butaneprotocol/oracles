mod logic;
#[cfg(test)]
mod tests;

use logic::GeneratedKeys;
pub use logic::KeygenMessage;

use crate::{config::OracleConfig, health::HealthSink, keys, network::Network};
use anyhow::{Context, Result, anyhow};
use tokio::{select, task::spawn_blocking};
use tracing::info;

pub async fn run(config: &OracleConfig) -> Result<()> {
    let keys_dir = keys::get_keys_directory()?;

    // Log where the keys will be saved
    info!(
        "Running in DKG mode. Will generate frost keys into {}.",
        keys_dir.display()
    );

    let mut network = Network::new(&config.network, HealthSink::noop());

    let id = network.id.clone();
    let channel = network.keygen_channel();
    let max_signers = config.network.peers.len() as u16 + 1;
    let min_signers = config
        .keygen
        .min_signers
        .ok_or_else(|| anyhow!("Must specify min_signers"))?;

    select! {
        result = network.listen() => {
            result?;
            return Err(anyhow!("Network shut down prematurely"));
        }
        result = logic::run(id, channel, max_signers, min_signers) => {
            let GeneratedKeys(private_key, public_key) = result?;
            let address = keys::write_frost_keys(&keys_dir, private_key, public_key).context("Could not save frost keys")?;
            info!("The new frost address is: {}", address);

            // Wait for keyboard input, so these are convenient to shut down while testing locally.
            // On a real server, this just waits "forever".
            spawn_blocking(|| {
                info!("Press enter to shut down.");
                std::io::stdin().read_line(&mut String::new()).unwrap();
            })
            .await?;
        }
    }

    Ok(())
}
