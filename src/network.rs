use anyhow::Result;
use opentelemetry::Context;
use tokio::{sync::mpsc, task::JoinSet};
use tracing::{Span, info, warn};
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::config::NetworkConfig;
use crate::raft::RaftMessage;
use crate::signature_aggregator::signer::SignerMessage;
use crate::{dkg::KeygenMessage, health::HealthSink};
pub use channel::{NetworkChannel, NetworkReceiver, NetworkSender};
use core::Core;
use std::any::type_name;
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

// We have a submodule named "core", which breaks the minicbor macros.
// Declare this in a nested context so our "core" isn't visible
mod derive_workaround {
    use minicbor::{Decode, Encode};

    use crate::{
        dkg::KeygenMessage, raft::RaftMessage, signature_aggregator::signer::SignerMessage,
    };

    #[derive(Decode, Encode, Clone, Debug)]
    pub enum Message {
        #[n(0)]
        Keygen(#[n(0)] KeygenMessage),
        #[n(1)]
        Raft(#[n(0)] RaftMessage),
        #[n(2)]
        Signer(#[n(0)] SignerMessage),
    }
}
pub use derive_workaround::Message;

pub struct Network {
    pub id: NodeId,
    port: u16,
    core: Core,
    outgoing_sender: mpsc::Sender<(Option<NodeId>, Message, Context)>,
    incoming_receiver: mpsc::Receiver<(NodeId, Message, Context)>,
    keygen: Option<MpscPair<KeygenMessage>>,
    raft: Option<MpscPair<RaftMessage>>,
    signer: Option<MpscPair<SignerMessage>>,
}

impl Network {
    pub fn new(config: &NetworkConfig, health_sink: HealthSink) -> Self {
        let (outgoing_sender, outgoing_receiver) = mpsc::channel(10);
        let (incoming_sender, incoming_receiver) = mpsc::channel(10);
        let core = Core::new(config, health_sink, outgoing_receiver, incoming_sender);
        let id = config.id.clone();
        Self {
            id,
            port: config.port,
            core,
            outgoing_sender,
            incoming_receiver,
            keygen: None,
            signer: None,
            raft: None,
        }
    }

    pub fn keygen_channel(&mut self) -> NetworkChannel<KeygenMessage> {
        create_channel(&mut self.keygen)
    }

    pub fn raft_channel(&mut self) -> NetworkChannel<RaftMessage> {
        create_channel(&mut self.raft)
    }

    pub fn signer_channel(&mut self) -> NetworkChannel<SignerMessage> {
        create_channel(&mut self.signer)
    }

    pub async fn listen(self) -> Result<()> {
        info!("Now listening on port {}", self.port);

        let mut set = JoinSet::new();

        let sender = self.outgoing_sender;
        let keygen_sender = send_messages(&mut set, &sender, self.keygen, Message::Keygen);
        let raft_sender = send_messages(&mut set, &sender, self.raft, Message::Raft);
        let signer_sender = send_messages(&mut set, &sender, self.signer, Message::Signer);

        let mut receiver = self.incoming_receiver;
        set.spawn(async move {
            while let Some((from, data, context)) = receiver.recv().await {
                match data {
                    Message::Keygen(data) => {
                        receive_message(from, data, context, &keygen_sender);
                    }
                    Message::Raft(data) => {
                        receive_message(from, data, context, &raft_sender);
                    }
                    Message::Signer(data) => {
                        receive_message(from, data, context, &signer_sender);
                    }
                }
            }
        });

        set.spawn(async move {
            self.core.handle_network().await.unwrap();
        });

        while let Some(x) = set.join_next().await {
            x?;
        }
        Ok(())
    }
}

fn create_channel<T>(holder: &mut Option<MpscPair<T>>) -> NetworkChannel<T> {
    let (outgoing_tx, outgoing_rx) = mpsc::channel(2000);
    let sender = NetworkSender::new(outgoing_tx);

    let (incoming_tx, incoming_rx) = mpsc::channel(2000);
    let receiver = NetworkReceiver::new(incoming_rx);

    holder.replace((incoming_tx, outgoing_rx));

    NetworkChannel::new(sender, receiver)
}

fn send_messages<T, F>(
    set: &mut JoinSet<()>,
    core_sender: &mpsc::Sender<(Option<NodeId>, Message, Context)>,
    holder: Option<MpscPair<T>>,
    wrap: F,
) -> Option<mpsc::Sender<IncomingMessage<T>>>
where
    T: Send + 'static,
    F: Send + 'static + Fn(T) -> Message,
{
    let core_sender = core_sender.clone();
    if let Some((sender, mut receiver)) = holder {
        set.spawn(async move {
            while let Some(message) = receiver.recv().await {
                let wrapped = wrap(message.data);
                core_sender
                    .send((message.to, wrapped, message.span.context()))
                    .await
                    .unwrap();
            }
        });
        Some(sender)
    } else {
        None
    }
}

fn receive_message<T>(
    from: NodeId,
    data: T,
    context: Context,
    sender: &Option<mpsc::Sender<IncomingMessage<T>>>,
) {
    let span = Span::current();
    span.set_parent(context);
    let message = IncomingMessage {
        from: from.clone(),
        data,
        span: span.clone(),
    };
    if let Some(sender) = sender {
        if let Err(error) = sender.try_send(message) {
            span.in_scope(|| {
                warn!(
                    "error processing {} message from {}: {}",
                    type_name::<T>(),
                    from,
                    error
                );
            });
        }
    }
}
