use std::collections::HashMap;

use tokio::sync::mpsc;

use crate::network::{
    IncomingMessage, NetworkChannel, NetworkReceiver, NetworkSender, NodeId, OutgoingMessage,
};

pub struct TestNetwork<T: Clone> {
    outgoing: HashMap<NodeId, mpsc::Receiver<OutgoingMessage<T>>>,
    incoming: HashMap<NodeId, mpsc::Sender<IncomingMessage<T>>>,
}

impl<T: Clone> Default for TestNetwork<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Clone> TestNetwork<T> {
    pub fn new() -> Self {
        Self {
            outgoing: HashMap::new(),
            incoming: HashMap::new(),
        }
    }

    pub fn connect(&mut self) -> (NodeId, NetworkChannel<T>) {
        let node_id = NodeId::new(self.outgoing.len().to_string());

        let (outgoing_tx, outgoing_rx) = mpsc::channel(10);
        self.outgoing.insert(node_id.clone(), outgoing_rx);
        let sender = NetworkSender::new(outgoing_tx);

        let (incoming_tx, incoming_rx) = mpsc::channel(10);
        self.incoming.insert(node_id.clone(), incoming_tx);
        let receiver = NetworkReceiver::new(incoming_rx);

        let channel = NetworkChannel::new(sender, receiver);
        (node_id, channel)
    }

    pub fn reconnect(&mut self, node_id: &NodeId) -> NetworkChannel<T> {
        let (outgoing_tx, outgoing_rx) = mpsc::channel(10);
        self.outgoing.insert(node_id.clone(), outgoing_rx);
        let sender = NetworkSender::new(outgoing_tx);

        let (incoming_tx, incoming_rx) = mpsc::channel(10);
        self.incoming.insert(node_id.clone(), incoming_tx);
        let receiver = NetworkReceiver::new(incoming_rx);

        NetworkChannel::new(sender, receiver)
    }

    pub async fn next_message_from(&mut self, from: &NodeId) -> Option<OutgoingMessage<T>> {
        let receiver = self.outgoing.get_mut(from).unwrap();
        let message = receiver.recv().await?;
        self.send(from, &message).await;
        Some(message)
    }

    pub async fn drain_one(&mut self, from: &NodeId) -> Vec<OutgoingMessage<T>> {
        let mut sent = vec![];
        while let Ok(message) = self.outgoing.get_mut(from).unwrap().try_recv() {
            self.send(from, &message).await;
            sent.push(message);
        }
        sent
    }

    pub async fn drain(&mut self) -> bool {
        let targets: Vec<NodeId> = self.outgoing.keys().cloned().collect();
        let mut something_sent = false;
        loop {
            let mut something_sent_this_round = false;
            for node_id in &targets {
                if !self.drain_one(node_id).await.is_empty() {
                    something_sent = true;
                    something_sent_this_round = true;
                }
            }
            if !something_sent_this_round {
                return something_sent;
            }
        }
    }

    async fn send(&mut self, from: &NodeId, message: &OutgoingMessage<T>) {
        let recipients = self
            .incoming
            .iter()
            .filter(|(id, _)| {
                from != *id && (message.to.is_none() || message.to.as_ref().unwrap() == *id)
            })
            .map(|(_, s)| s);
        for recipient in recipients {
            let message = IncomingMessage {
                from: from.clone(),
                data: message.data.clone(),
            };
            recipient.send(message).await.unwrap();
        }
    }
}
