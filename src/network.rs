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
pub use test::TestNetwork;
pub use types::{IncomingMessage, NodeId, OutgoingMessage};

mod channel;
mod core;
mod test;
mod types;

type MpscPair<T> = (
    mpsc::Sender<IncomingMessage<T>>,
    mpsc::Receiver<OutgoingMessage<T>>,
);

pub struct Network {
    pub id: NodeId,
    port: u16,
    peers: Vec<PeerConfig>,
    core: Core,
    incoming_message_receiver: mpsc::Receiver<(NodeId, Message)>,
    raft: Option<MpscPair<RaftMessage>>,
    signer: Option<MpscPair<SignerMessage>>,
}

impl Network {
    pub fn new(id: &str, config: &OracleConfig) -> Self {
        let id = NodeId::new(id.to_string());
        let (incoming_message_sender, incoming_message_receiver) = mpsc::channel(10);
        let core = Core::new(id.clone(), incoming_message_sender);
        Self {
            id,
            port: config.port,
            peers: config.peers.clone(),
            core,
            incoming_message_receiver,
            signer: None,
            raft: None,
        }
    }

    pub fn signer_channel(&mut self) -> NetworkChannel<SignerMessage> {
        create_channel(&mut self.signer)
    }

    pub fn raft_channel(&mut self) -> NetworkChannel<RaftMessage> {
        create_channel(&mut self.raft)
    }

    pub async fn listen(self) -> Result<()> {
        info!("Now listening on port {}", self.port);

        let mut set = JoinSet::new();

        let raft_sender = send_messages(&mut set, &self.core, self.raft, Message::Raft);
        let signer_sender = send_messages(&mut set, &self.core, self.signer, Message::Signer);

        let mut receiver = self.incoming_message_receiver;
        set.spawn(async move {
            while let Some((from, data)) = receiver.recv().await {
                match data {
                    Message::Raft(data) => {
                        receive_message(from, data, &raft_sender).await;
                    }
                    Message::Signer(data) => {
                        receive_message(from, data, &signer_sender).await;
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

fn create_channel<T>(holder: &mut Option<MpscPair<T>>) -> NetworkChannel<T> {
    let (outgoing_tx, outgoing_rx) = mpsc::channel(10);
    let sender = NetworkSender::new(outgoing_tx);

    let (incoming_tx, incoming_rx) = mpsc::channel(10);
    let receiver = NetworkReceiver::new(incoming_rx);

    holder.replace((incoming_tx, outgoing_rx));

    NetworkChannel::new(sender, receiver)
}

fn send_messages<T, F>(
    set: &mut JoinSet<()>,
    core: &Core,
    holder: Option<MpscPair<T>>,
    wrap: F,
) -> Option<mpsc::Sender<IncomingMessage<T>>>
where
    T: Send + 'static,
    F: Send + 'static + Fn(T) -> Message,
{
    if let Some((sender, mut receiver)) = holder {
        let core = core.clone();
        set.spawn(async move {
            while let Some(message) = receiver.recv().await {
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
        });
        Some(sender)
    } else {
        None
    }
}

async fn receive_message<T>(
    from: NodeId,
    data: T,
    sender: &Option<mpsc::Sender<IncomingMessage<T>>>,
) {
    let message = IncomingMessage { from, data };
    if let Some(sender) = sender {
        if let Err(error) = sender.send(message).await {
            warn!("error receiving message from {:?}: {}", error.0.from, error);
        }
    }
}
