use std::fs;

use anyhow::{Context, Result};
use ed25519::{
    pkcs8::{DecodePrivateKey, DecodePublicKey},
    KeypairBytes, PublicKeyBytes,
};
use ed25519_dalek::{SigningKey as PrivateKey, VerifyingKey as PublicKey};
use tracing::info;

use crate::{
    config::{OracleConfig, PeerConfig},
    keys,
};

use super::NodeId;

pub struct NetworkConfig {
    pub id: NodeId,
    pub private_key: PrivateKey,
    pub port: u16,
    pub peers: Vec<Peer>,
}

impl NetworkConfig {
    pub fn load(config: &OracleConfig) -> Result<Self> {
        let private_key = read_private_key()?;
        let id = compute_node_id(&private_key.verifying_key());
        info!("This node has ID {}", id);

        let peers: Result<Vec<Peer>> = config.peers.iter().map(parse_peer).collect();
        let mut peers: Vec<Peer> = peers?.into_iter().filter(|p| p.id != id).collect();
        peers.sort_by_cached_key(|k| k.id.clone());
        Ok(Self {
            id,
            private_key,
            port: if config.network_port == 0 {
                config.port
            } else {
                config.network_port
            },
            peers,
        })
    }
}

#[derive(Clone)]
pub struct Peer {
    pub id: NodeId,
    pub public_key: PublicKey,
    pub label: String,
    pub address: String,
}

fn parse_peer(config: &PeerConfig) -> Result<Peer> {
    let public_key = {
        let key_bytes = PublicKeyBytes::from_public_key_pem(&config.public_key)?;
        PublicKey::from_bytes(&key_bytes.0)?
    };
    let id = compute_node_id(&public_key);
    let label = config.label.as_ref().unwrap_or(&config.address).clone();
    Ok(Peer {
        id,
        public_key,
        label,
        address: config.address.clone(),
    })
}

fn read_private_key() -> Result<PrivateKey> {
    let key_path = keys::get_keys_directory()?.join("private.pem");
    let key_pem_file = fs::read_to_string(&key_path).context(format!(
        "Could not load private key from {}",
        key_path.display()
    ))?;
    let decoded = KeypairBytes::from_pkcs8_pem(&key_pem_file)?;
    let private_key = PrivateKey::from_bytes(&decoded.secret_key);
    Ok(private_key)
}

pub fn compute_node_id(public_key: &PublicKey) -> NodeId {
    NodeId::new(hex::encode(public_key.as_bytes()))
}
