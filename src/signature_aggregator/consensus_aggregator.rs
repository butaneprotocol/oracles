use std::time::Duration;

use anyhow::{anyhow, Result};
use frost_ed25519::keys::{KeyPackage, PublicKeyPackage};
use tokio::{
    select,
    sync::{mpsc, watch},
    time::sleep,
};
use tracing::{warn, Instrument};

use crate::{
    config::OracleConfig,
    keys,
    network::{NetworkChannel, NetworkReceiver, NodeId},
    price_feed::{PriceFeedEntry, SignedEntries},
    raft::RaftLeader,
};

use super::signer::{Signer, SignerEvent, SignerMessage};

pub struct ConsensusSignatureAggregator {
    signer: Signer,
    leader_source: watch::Receiver<RaftLeader>,
    message_source: NetworkReceiver<SignerMessage>,
}
impl ConsensusSignatureAggregator {
    pub fn new(
        config: &OracleConfig,
        id: NodeId,
        channel: NetworkChannel<SignerMessage>,
        price_source: watch::Receiver<Vec<PriceFeedEntry>>,
        leader_source: watch::Receiver<RaftLeader>,
        signed_entries_sink: mpsc::Sender<(NodeId, SignedEntries)>,
    ) -> Result<Self> {
        let (key, public_key) = Self::load_keys(config)?;
        let (outgoing_message_sink, message_source) = channel.split();
        let signer = Signer::new(
            id,
            key,
            public_key,
            price_source,
            outgoing_message_sink,
            signed_entries_sink,
        );

        Ok(Self {
            signer,
            leader_source,
            message_source,
        })
    }

    pub async fn run(self) {
        let (event_sink, mut event_source) = mpsc::channel(10);

        // Every 10 seconds, if we're the leader, send the signer a "round started" event
        let leader = self.leader_source.clone();
        let sink = event_sink.clone();
        let new_round_task = async move {
            loop {
                sleep(Duration::from_secs(10)).await;
                if !matches!(*leader.borrow(), RaftLeader::Myself) {
                    continue;
                }
                if let Err(err) = sink.send(SignerEvent::RoundStarted).await {
                    warn!("Failed to start new round: {}", err);
                    break;
                }
            }
        }
        .in_current_span();

        // Any time the current leader changes, send the signer a "leader changed" event
        let mut leader = self.leader_source;
        let sink = event_sink.clone();
        let leader_changed_task = async move {
            while let Ok(()) = leader.changed().await {
                let new_leader = leader.borrow().clone();
                if let Err(err) = sink.send(SignerEvent::LeaderChanged(new_leader)).await {
                    warn!("Failed to update leader: {}", err);
                    break;
                }
            }
        }
        .in_current_span();

        // Any time someone sends us a message, send the signer a "message" event
        let mut message_source = self.message_source;
        let message_received_task = async move {
            while let Some(incoming) = message_source.recv().await {
                if let Err(err) = event_sink
                    .send(SignerEvent::Message(incoming.from, incoming.data))
                    .await
                {
                    warn!("Failed to receive message: {}", err);
                    break;
                }
            }
        }
        .in_current_span();

        let mut signer = self.signer;
        // Forward any events to the signer
        let handle_events_task = async move {
            while let Some(event) = event_source.recv().await {
                warn!("Received event in CSA: {:#?}", event);
                signer.process(event).await;
            }
        }
        .in_current_span();

        select! {
            res = new_round_task => res,
            res = leader_changed_task => res,
            res = message_received_task => res,
            res = handle_events_task => res,
        }
    }

    fn load_keys(config: &OracleConfig) -> Result<(KeyPackage, PublicKeyPackage)> {
        let keys_dir = keys::get_keys_directory()?;
        let public_key_hash = config.frost_address.as_ref().ok_or_else(|| {
            anyhow!("No frost_address found in config. Please generate frost keys.")
        })?;
        keys::read_frost_keys(&keys_dir, public_key_hash)
    }
}
