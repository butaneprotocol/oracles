use std::{env, fs, time::Duration};

use anyhow::{Context, Result};
use frost_ed25519::keys::{KeyPackage, PublicKeyPackage};
use tokio::{
    select,
    sync::{
        mpsc::{self, Sender},
        watch::Receiver,
    },
    time::sleep,
};
use tracing::{warn, Instrument};

use crate::{
    network::{NetworkChannel, NetworkReceiver, NodeId},
    price_feed::{PriceFeedEntry, SignedPriceFeedEntry},
    raft::RaftLeader,
};

use super::signer::{Signer, SignerEvent, SignerMessage};

pub struct ConsensusSignatureAggregator {
    signer: Signer,
    leader_source: Receiver<RaftLeader>,
    message_source: NetworkReceiver<SignerMessage>,
}
impl ConsensusSignatureAggregator {
    pub fn new(
        id: NodeId,
        channel: NetworkChannel<SignerMessage>,
        price_source: Receiver<Vec<PriceFeedEntry>>,
        leader_source: Receiver<RaftLeader>,
        signed_price_sink: Sender<Vec<SignedPriceFeedEntry>>,
    ) -> Result<Self> {
        let (key, public_key) = Self::load_keys()?;
        let (outgoing_message_sink, message_source) = channel.split();
        let signer = Signer::new(
            id,
            key,
            public_key,
            price_source,
            outgoing_message_sink,
            signed_price_sink,
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

    fn load_keys() -> Result<(KeyPackage, PublicKeyPackage)> {
        let key_path = env::var("FROST_KEY_PATH").context("FROST_KEY_PATH not set")?;
        let key_bytes = fs::read(key_path).context("could not load frost private key")?;
        let key = KeyPackage::deserialize(&key_bytes)?;
        let public_key_path =
            env::var("FROST_PUBLIC_KEY_PATH").context("FROST_PUBLIC_KEY_PATH not set")?;
        let public_key_bytes =
            fs::read(public_key_path).context("could not load frost public key")?;
        let public_key = PublicKeyPackage::deserialize(&public_key_bytes)?;
        Ok((key, public_key))
    }
}
