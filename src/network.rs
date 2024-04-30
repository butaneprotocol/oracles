use anyhow::Result;
use tokio::{sync::mpsc, task::JoinSet};
use tracing::{info, warn};

use crate::networking::Message as OldMessage;
use crate::networking::Network as OldNetwork;
use crate::{
    config::{OracleConfig, PeerConfig},
    signature_aggregator::signer::SignerMessage,
};
pub use channel::{NetworkChannel, NetworkReceiver, NetworkSender};
pub use core::{IncomingMessage, OutgoingMessage, TargetId};
pub use test::TestNetwork;

mod channel;
mod core;
mod test;

pub struct Network {
    port: u16,
    peers: Vec<PeerConfig>,
    old: crate::networking::Network,
    incoming_message_receiver: mpsc::Receiver<(String, OldMessage)>,
    outgoing_signer: Option<mpsc::Receiver<OutgoingMessage<SignerMessage>>>,
    incoming_signer: Option<mpsc::Sender<IncomingMessage<SignerMessage>>>,
}

impl Network {
    pub fn new(
        config: &OracleConfig,
        old: crate::networking::Network,
        incoming_message_receiver: mpsc::Receiver<(String, crate::networking::Message)>,
    ) -> Self {
        Self {
            port: config.port,
            peers: config.peers.clone(),
            old,
            incoming_message_receiver,
            outgoing_signer: None,
            incoming_signer: None,
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

    pub async fn listen(self) -> Result<()> {
        info!("Now listening on port {}", self.port);

        let mut set = JoinSet::new();

        if let Some(mut signer) = self.outgoing_signer {
            let old = self.old.clone();
            set.spawn(async move {
                while let Some(message) = signer.recv().await {
                    send_message(message, OldMessage::Signer, &old).await;
                }
            });
        }

        let mut receiver = self.incoming_message_receiver;
        set.spawn(async move {
            while let Some((from, data)) = receiver.recv().await {
                if let OldMessage::Signer(data) = data {
                    receive_message(from, data, &self.incoming_signer).await;
                }
            }
        });

        set.spawn(async move {
            self.old
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

async fn send_message<T, F: Fn(T) -> OldMessage>(
    message: OutgoingMessage<T>,
    wrap: F,
    network: &OldNetwork,
) {
    let wrapped = wrap(message.data);
    match message.to {
        Some(id) => {
            network.send(&id, wrapped).await.unwrap();
        }
        None => {
            network.broadcast(wrapped).await;
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
