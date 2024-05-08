use anyhow::Result;
use tokio::{sync::mpsc, task::JoinSet};
use tracing::{info, warn};

use crate::raft::RaftMessage;
use crate::{
    config::{OracleConfig, PeerConfig},
    signature_aggregator::signer::SignerMessage,
};
pub use channel::{NetworkChannel, NetworkReceiver, NetworkSender};
use core::{Core, Message};
use std::sync::Arc;
pub use test::TestNetwork;
pub use types::{IncomingMessage, OutgoingMessage, TargetId};

mod channel;
mod core;
mod test;
mod types;

pub struct Network {
    port: u16,
    peers: Vec<PeerConfig>,
    core: Core,
    incoming_message_receiver: mpsc::Receiver<(String, Message)>,
    outgoing_signer: Option<mpsc::Receiver<OutgoingMessage<SignerMessage>>>,
    incoming_signer: Option<mpsc::Sender<IncomingMessage<SignerMessage>>>,
    outgoing_raft: Option<mpsc::Receiver<OutgoingMessage<RaftMessage>>>,
    incoming_raft: Option<mpsc::Sender<IncomingMessage<RaftMessage>>>,
}

impl Network {
    pub fn new(id: &str, config: &OracleConfig) -> Self {
        let (incoming_message_sender, incoming_message_receiver) = mpsc::channel(10);
        let core = Core::new(id.to_string(), Arc::new(incoming_message_sender));
        Self {
            port: config.port,
            peers: config.peers.clone(),
            core,
            incoming_message_receiver,
            outgoing_signer: None,
            incoming_signer: None,
            outgoing_raft: None,
            incoming_raft: None,
        }
    }

    pub fn signer_channel(&mut self) -> NetworkChannel<SignerMessage> {
        let (outgoing_tx, outgoing_rx) = mpsc::channel(10);
        self.outgoing_signer = Some(outgoing_rx);
        let sender = NetworkSender::new(outgoing_tx);

        let (incoming_tx, incoming_rx) = mpsc::channel(10);
        self.incoming_signer = Some(incoming_tx);
        let receiver = NetworkReceiver::new(incoming_rx);

        NetworkChannel::new(sender, receiver)
    }

    pub fn raft_channel(&mut self) -> NetworkChannel<RaftMessage> {
        let (outgoing_tx, outgoing_rx) = mpsc::channel(10);
        self.outgoing_raft = Some(outgoing_rx);
        let sender = NetworkSender::new(outgoing_tx);

        let (incoming_tx, incoming_rx) = mpsc::channel(10);
        self.incoming_raft = Some(incoming_tx);
        let receiver = NetworkReceiver::new(incoming_rx);

        NetworkChannel::new(sender, receiver)
    }

    pub async fn listen(self) -> Result<()> {
        info!("Now listening on port {}", self.port);

        let mut set = JoinSet::new();

        if let Some(mut raft) = self.outgoing_raft {
            let core = self.core.clone();
            set.spawn(async move {
                while let Some(message) = raft.recv().await {
                    send_message(message, Message::Raft, &core).await;
                }
            });
        }
        if let Some(mut signer) = self.outgoing_signer {
            let core = self.core.clone();
            set.spawn(async move {
                while let Some(message) = signer.recv().await {
                    send_message(message, Message::Signer, &core).await;
                }
            });
        }

        let mut receiver = self.incoming_message_receiver;
        set.spawn(async move {
            while let Some((from, data)) = receiver.recv().await {
                match data {
                    Message::Raft(data) => {
                        receive_message(from, data, &self.incoming_raft).await;
                    }
                    Message::Signer(data) => {
                        receive_message(from, data, &self.incoming_signer).await;
                    }
                    _ => {}
                }
            }
        });

        set.spawn(async move {
            self.core
                .handle_network(self.port, self.peers)
                .await
                .unwrap();
        });

        while let Some(x) = set.join_next().await {
            x?;
        }
        Ok(())
    }
}

async fn send_message<T, F: Fn(T) -> Message>(message: OutgoingMessage<T>, wrap: F, core: &Core) {
    let wrapped = wrap(message.data);
    match message.to {
        Some(id) => {
            core.send(&id, wrapped).await.unwrap();
        }
        None => {
            core.broadcast(wrapped).await;
        }
    }
}

async fn receive_message<T>(
    from: String,
    data: T,
    sender: &Option<mpsc::Sender<IncomingMessage<T>>>,
) {
    let message = IncomingMessage {
        from: TargetId::new(from),
        data,
    };
    if let Some(sender) = sender {
        if let Err(error) = sender.send(message).await {
            warn!("error receiving message from {:?}: {}", error.0.from, error);
        }
    }
}
