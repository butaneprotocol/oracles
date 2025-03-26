use std::time::Duration;

use anyhow::{anyhow, Result};
use frost_ed25519::keys::{KeyPackage, PublicKeyPackage};
use tokio::{
    select,
    sync::{mpsc, watch},
    time::sleep,
};
use tracing::{info_span, warn};
use uuid::Uuid;

use crate::{
    config::OracleConfig,
    keys,
    network::{NetworkChannel, NetworkReceiver, NodeId},
    price_feed::{PriceData, SignedEntries},
    raft::RaftLeader,
};

use super::signer::{Signer, SignerEvent, SignerMessage};

pub struct ConsensusSignatureAggregator {
    signer: Signer,
    leader_source: watch::Receiver<RaftLeader>,
    message_source: NetworkReceiver<SignerMessage>,
    round_duration: Duration,
}
impl ConsensusSignatureAggregator {
    pub fn new(
        config: &OracleConfig,
        channel: NetworkChannel<SignerMessage>,
        price_source: watch::Receiver<PriceData>,
        leader_source: watch::Receiver<RaftLeader>,
        signed_entries_sink: mpsc::Sender<(NodeId, SignedEntries)>,
    ) -> Result<Self> {
        let (key, public_key) = Self::load_keys(config)?;
        let (outgoing_message_sink, message_source) = channel.split();
        let signer = Signer::new(
            config.id.clone(),
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
            round_duration: config.round_duration,
        })
    }

    pub async fn run(self) {
        let (event_sink, mut event_source) = mpsc::channel(10);

        // Every 10 seconds, if we're the leader, send the signer a "round started" event.
        // 5 seconds after that, send a "grace period expired" event.
        let leader = self.leader_source.clone();
        let sink = event_sink.clone();
        let new_round_task = async move {
            let half_round_duration = self.round_duration / 2;
            loop {
                sleep(half_round_duration).await;
                if !matches!(*leader.borrow(), RaftLeader::Myself) {
                    continue;
                }
                let round = Uuid::new_v4().to_string();
                let span = info_span!("new_round", round);
                if let Err(err) = sink.send(SignerEvent::RoundStarted(round.clone())).await {
                    span.in_scope(|| warn!("Failed to start new round: {}", err));
                    break;
                }
                drop(span);

                sleep(half_round_duration).await;
                if !matches!(*leader.borrow(), RaftLeader::Myself) {
                    continue;
                }
                let span = info_span!("grace_period_timeout", round);
                if let Err(err) = sink.send(SignerEvent::RoundGracePeriodEnded(round)).await {
                    span.in_scope(|| warn!("Failed to end grace period for round: {}", err));
                    break;
                }
            }
        };

        // Any time the current leader changes, send the signer a "leader changed" event
        let mut leader = self.leader_source;
        let sink = event_sink.clone();
        let leader_changed_task = async move {
            while let Ok(()) = leader.changed().await {
                let span = info_span!("new_round");
                let new_leader = leader.borrow().clone();
                if let Err(err) = sink.send(SignerEvent::LeaderChanged(new_leader)).await {
                    span.in_scope(|| warn!("Failed to update leader: {}", err));
                    break;
                }
            }
        };

        // Any time someone sends us a message, send the signer a "message" event
        let mut message_source = self.message_source;
        let message_received_task = async move {
            while let Some(incoming) = message_source.recv().await {
                let span = info_span!(parent: incoming.span, "message_received");
                if let Err(err) = event_sink
                    .send(SignerEvent::Message(
                        incoming.from,
                        incoming.data,
                        span.clone(),
                    ))
                    .await
                {
                    span.in_scope(|| warn!("Failed to receive message: {}", err));
                    break;
                }
            }
        };

        let mut signer = self.signer;
        // Forward any events to the signer
        let handle_events_task = async move {
            while let Some(event) = event_source.recv().await {
                signer.process(event).await;
            }
        };

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
