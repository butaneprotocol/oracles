use anyhow::Result;
use tracing::info;

use crate::config::{OracleConfig, PeerConfig};

pub struct Network {
    port: u16,
    peers: Vec<PeerConfig>,
    old: crate::networking::Network,
}

impl Network {
    pub fn new(config: &OracleConfig, old: crate::networking::Network) -> Self {
        Self {
            port: config.port,
            peers: config.peers.clone(),
            old,
        }
    }

    pub async fn listen(self) -> Result<()> {
        info!("Now listening on port {}", self.port);
        self.old.handle_network(self.port, self.peers).await?;
        Ok(())
    }
}
