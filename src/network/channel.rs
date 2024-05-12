use tokio::sync::mpsc::{Receiver, Sender};
use tracing::warn;

use super::types::{IncomingMessage, NodeId, OutgoingMessage};

#[derive(Clone)]
pub struct NetworkSender<T>(Sender<OutgoingMessage<T>>);

impl<T> NetworkSender<T> {
    pub fn new(sender: Sender<OutgoingMessage<T>>) -> Self {
        Self(sender)
    }

    pub async fn send(&self, to: NodeId, message: T) {
        if let Err(error) = self
            .0
            .send(OutgoingMessage {
                to: Some(to),
                data: message,
            })
            .await
        {
            warn!("Could not send message: {}", error);
        }
    }

    pub async fn broadcast(&self, message: T) {
        if let Err(error) = self
            .0
            .send(OutgoingMessage {
                to: None,
                data: message,
            })
            .await
        {
            warn!("Could not broadcast message: {}", error);
        }
    }
}

pub struct NetworkReceiver<T>(Receiver<IncomingMessage<T>>);

impl<T> NetworkReceiver<T> {
    pub fn new(receiver: Receiver<IncomingMessage<T>>) -> Self {
        Self(receiver)
    }

    pub async fn recv(&mut self) -> Option<IncomingMessage<T>> {
        self.0.recv().await
    }

    pub async fn try_recv(&mut self) -> Option<IncomingMessage<T>> {
        match self.0.try_recv() {
            Ok(message) => Some(message),
            _ => None,
        }
    }
}

pub struct NetworkChannel<T>(NetworkSender<T>, NetworkReceiver<T>);

impl<T> NetworkChannel<T> {
    pub fn new(sender: NetworkSender<T>, receiver: NetworkReceiver<T>) -> Self {
        Self(sender, receiver)
    }
    pub fn split(self) -> (NetworkSender<T>, NetworkReceiver<T>) {
        (self.0, self.1)
    }
}
