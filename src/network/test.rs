use std::collections::HashMap;

use tokio::sync::mpsc;

use crate::network::{
    IncomingMessage, NetworkChannel, NetworkReceiver, NetworkSender, OutgoingMessage, TargetId,
};

pub struct TestNetwork<T: Clone> {
    outgoing: HashMap<TargetId, mpsc::Receiver<OutgoingMessage<T>>>,
    incoming: HashMap<TargetId, mpsc::Sender<IncomingMessage<T>>>,
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

    pub fn connect(&mut self) -> (TargetId, NetworkChannel<T>) {
        let target_id = TargetId::new(self.outgoing.len().to_string());

        let (outgoing_tx, outgoing_rx) = mpsc::channel(10);
        self.outgoing.insert(target_id.clone(), outgoing_rx);
        let sender = NetworkSender::new(outgoing_tx);

        let (incoming_tx, incoming_rx) = mpsc::channel(10);
        self.incoming.insert(target_id.clone(), incoming_tx);
        let receiver = NetworkReceiver::new(incoming_rx);

        let channel = NetworkChannel::new(sender, receiver);
        (target_id, channel)
    }

    pub async fn drain_one(&mut self, from: &TargetId) -> Vec<OutgoingMessage<T>> {
        let mut sent = vec![];
        let receiver = self.outgoing.get_mut(from).unwrap();
        while let Ok(message) = receiver.try_recv() {
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
            sent.push(message);
        }
        sent
    }

    pub async fn drain(&mut self) -> bool {
        let targets: Vec<TargetId> = self.outgoing.keys().cloned().collect();
        let mut something_sent = false;
        loop {
            let mut something_sent_this_round = false;
            for target_id in &targets {
                if !self.drain_one(target_id).await.is_empty() {
                    something_sent = true;
                    something_sent_this_round = true;
                }
            }
            if !something_sent_this_round {
                return something_sent;
            }
        }
    }
}