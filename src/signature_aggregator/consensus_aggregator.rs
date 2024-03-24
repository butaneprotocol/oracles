use std::{fs, path::Path, time::Duration};

use anyhow::Result;
use frost_ed25519::keys::{KeyPackage, PublicKeyPackage};
use tokio::{
    sync::{
        mpsc::{self, Sender},
        watch::Receiver,
    },
    time::sleep,
};

use crate::{
    networking::Network,
    price_feed::{PriceFeedEntry, SignedPriceFeedEntry},
    raft::RaftLeader,
};

use super::signer::{OutgoingMessage, Signer};

#[allow(dead_code)]
pub struct ConsensusSignatureAggregator {
    signer: Signer,
    leader_source: Receiver<RaftLeader>,
    message_source: mpsc::Receiver<OutgoingMessage>,
    network: Network,
}
impl ConsensusSignatureAggregator {
    pub fn new(
        id: String,
        network: Network,
        key_path: &Path,
        public_key_path: &Path,
        price_source: Receiver<Vec<PriceFeedEntry>>,
        leader_source: Receiver<RaftLeader>,
        signed_price_sink: Sender<Vec<SignedPriceFeedEntry>>,
    ) -> Result<Self> {
        let (key, public_key) = Self::load_keys(key_path, public_key_path)?;
        let (message_sink, message_source) = mpsc::channel(10);
        let signer = Signer::new(
            id,
            key,
            public_key,
            price_source,
            message_sink,
            signed_price_sink,
        );

        Ok(Self {
            signer,
            leader_source,
            message_source,
            network,
        })
    }

    pub async fn run(&mut self) {
        // TODO
        loop {
            sleep(Duration::from_secs(5)).await;
        }
    }

    fn load_keys(
        key_path: &Path,
        public_key_path: &Path,
    ) -> Result<(KeyPackage, PublicKeyPackage)> {
        let key_bytes = fs::read(key_path)?;
        let key = KeyPackage::deserialize(&key_bytes)?;
        let public_key_bytes = fs::read(public_key_path)?;
        let public_key = PublicKeyPackage::deserialize(&public_key_bytes)?;
        Ok((key, public_key))
    }
}
