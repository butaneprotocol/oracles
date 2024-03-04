use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::{net::{tcp::{OwnedReadHalf, OwnedWriteHalf}, TcpListener, TcpStream}, task::{JoinError, JoinSet}};
use tokio_serde::{formats::Cbor, Framed};
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};
use tracing::{info, trace, warn};

use crate::raft::RaftMessage;

type WrappedStream = FramedRead<OwnedReadHalf, LengthDelimitedCodec>;
type WrappedSink = FramedWrite<OwnedWriteHalf, LengthDelimitedCodec>;

type SerStream = Framed<WrappedStream, Message, (), Cbor<Message, ()>>;
type DeSink = Framed<WrappedSink, (), Message, Cbor<(), Message>>;


#[derive(Serialize, Deserialize, Debug)]
pub enum Message {
    Hello(String),
    Raft(RaftMessage),
}

#[derive(Clone)]
pub struct Network {
    id: String,

    raft_messages: Arc<tokio::sync::mpsc::Sender<RaftMessage>>,
    incoming_connections: Arc<dashmap::DashMap<String, ()>>,
    outgoing_connections: Arc<dashmap::DashMap<String, tokio::sync::mpsc::Sender<Message>>>,
}

impl Network {
    pub fn new(id: String, raft_messages: Arc<tokio::sync::mpsc::Sender<RaftMessage>>) -> Self {
        Network {
            id,
            raft_messages,
            incoming_connections: Arc::new(dashmap::DashMap::new()),
            outgoing_connections: Arc::new(dashmap::DashMap::new()),
        }
    }

    pub async fn handle_network(self, port: u16, peers: Vec<String>) -> Result<Self, JoinError> {
        let mut set = JoinSet::new();

        let network = self.clone();
        let local_addr = format!("localhost:{}", port);
        set.spawn(async move {
            network.accept_connections(local_addr.as_str()).await.unwrap();
        });
    
        for peer in peers {
            let network = self.clone();
            set.spawn(async move {
                network.connect_to(peer.as_str()).await.unwrap();
            });
        }

        while let Some(res) = set.join_next().await {
            if let Err(e) = res {
                return Err(e);
            }
        }
        Ok(self)
    }

    pub async fn accept_connections(self, addr: &str) -> std::io::Result<()> {
        info!(me = self.id, "Listening on: {}", addr);

        let listener = TcpListener::bind(addr).await?;
        while let Ok((stream, _)) = listener.accept().await {
            let node = self.clone();
            tokio::spawn(async move {
                node.handle_incoming_connection(stream).await;
            });
        }

        Ok(())
    }

    async fn handle_incoming_connection(self, stream: TcpStream) {
        trace!(me = self.id, "Incoming connection from: {}", stream.peer_addr().unwrap());

        let (read, write) = stream.into_split();
        let stream: WrappedStream = WrappedStream::new(read, LengthDelimitedCodec::new());
        let sink: WrappedSink = WrappedSink::new(write, LengthDelimitedCodec::new());
        let mut stream: SerStream = SerStream::new(stream, Cbor::default());
        let mut sink: DeSink = DeSink::new(sink, Cbor::default());

        let mut them = "".to_string();

        // On an incoming connection, expect to receive a `Hello` so we know the node ID,
        // then tell them our ID
        match stream.next().await {
            Some(Ok(Message::Hello(other))) => {
                them = other;
                trace!(me = self.id, them = them, "Incoming Hello received");
            }
            Some(Ok(other)) => {
                warn!(me = self.id, them = them, "Expected Hello, got {:?}", other);
                return;
            }
            Some(Err(e)) => {
                warn!(me = self.id, them = them, "Failed to parse: {}", e);
                return;
            }
            None => {
                warn!(me = self.id, them = them, "Incoming Connection disconnected before handshake");
                return;
            }
        }

        match sink.send(Message::Hello(self.id.clone())).await {
            Ok(_) => {
                trace!(me = self.id, them = them, "Incoming Hello Sent");
            }
            Err(e) => {
                warn!(me = self.id, them = them, "Failed to send hello: {}", e);
                return;
            }
        }

        trace!(me = self.id, "Handshake done, reading incoming messages...");
        self.incoming_connections.insert(them.clone(), ());
        // Once both connections have been established, tell raft about it
        if self.outgoing_connections.contains_key(&them) {
            self.raft_messages.clone().send(RaftMessage::Connect { node_id: them.clone() }).await.unwrap();
        }
        while let Some(msg) = stream.next().await {
            trace!("Received message {:?} from {}", msg, them);

            match msg {
                Ok(Message::Raft(raft)) => {
                    match self.raft_messages.send(raft).await {
                        Ok(_) => {
                            trace!(me = self.id, them = them, "Raft message sent");
                        }
                        Err(e) => {
                            warn!(me = self.id, them = them, "Failed to send raft message: {}", e);
                            break;
                        }
                    }
                }
                Ok(_) => {
                    warn!(me = self.id, them = them, "Unexpected message");
                    break;
                }
                Err(e) => {
                    warn!(me = self.id, them = them, "Failed to parse message: {}", e);
                    break;
                }
            }
        }
        self.incoming_connections.remove(&them);
        // NOTE: we *don't* send the raft disconnect message here; we should continue to send them heartbeats
        // so that we can notice when the socket disconnects
        warn!(me = self.id, them = them, "Incoming connection Disconnected");
    }  

    pub async fn connect_to(self, addr: &str) -> std::io::Result<()> {
        let mut connection_attempts = 0;
        let mut reconnections = 0;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            trace!(me = self.id.clone(), connection_attempts = connection_attempts, reconnections = reconnections, "Attempting to connect to: {}", addr);
            connection_attempts += 1;
            let connection = TcpStream::connect(addr).await;
            match connection {
                Ok(stream) => {
                    self.handle_outgoing_connection(stream).await;
                    warn!(me = self.id.clone(), "Outgoing peer {:?} disconnected", addr);
                    reconnections += 1; 
                    // If we disconnect, attempt to reconnect
                    continue
                }
                Err(_) => {
                    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
                    // Peer isn't online, try again
                    continue
                }
            }
        }
    }

    async fn handle_outgoing_connection(&self, stream: TcpStream) {
        trace!(me = self.id, "Outgoing connection to: {}", stream.peer_addr().unwrap());
        let (read, write) = stream.into_split();
        let stream: WrappedStream = WrappedStream::new(read, LengthDelimitedCodec::new());
        let mut stream: SerStream = SerStream::new(stream, Cbor::default());
        let sink: WrappedSink = WrappedSink::new(write, LengthDelimitedCodec::new());
        let mut sink: DeSink = DeSink::new(sink, Cbor::default());
        
        // First, send our ID, because they're waiting for it on connection
        match sink.send(Message::Hello(self.id.clone())).await {
            Ok(_) => {
                trace!(me = self.id, "Outgoing Hello sent");
            }
            Err(e) => {
                warn!(me = self.id, "Failed to send hello: {}", e);
                return;
            }
        }
        let them: String;
        // Now, read so we know their ID
        match stream.next().await {
            Some(Ok(Message::Hello(other))) => {
                them = other;
                trace!(me = self.id, them = them, "Outgoing Hello received");
            }
            Some(Ok(other)) => {
                warn!(me = self.id, "Expected Hello, got {:?}", other);
                return;
            }
            Some(Err(e)) => {
                warn!(me = self.id, "Failed to parse: {}", e);
                return;
            }
            None => {
                warn!(me = self.id, "Outgoing connection disconnected");
                return;
            }
        }

        // Now, setup a mpsc channel for outgoing messages
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        self.outgoing_connections.insert(them.clone(), tx.clone());
        // Once both connections have been established, tell raft about it
        if self.incoming_connections.contains_key(&them) {
            self.raft_messages.clone().send(RaftMessage::Connect { node_id: them.clone() }).await.unwrap();
        }
        while let Some(next) = rx.recv().await {
            trace!(me = self.id, "Sending message {:?} to {}", next, them);
            match sink.send(next).await {
                Ok(_) => {
                    trace!(me = self.id, "Outgoing Message sent");
                }
                Err(e) => {
                    warn!(me = self.id, "Failed to send message: {}", e);
                    break;
                }
            }
        }
        warn!(me = self.id, "Outgoing connection Disconnected");
        self.outgoing_connections.remove(&them);
        self.raft_messages.clone().send(RaftMessage::Disconnect { node_id: them.clone() }).await.unwrap();
    }

    pub async fn broadcast(&self, messages: Vec<(String, Message)>) -> Vec<(String, Result<(), tokio::sync::mpsc::error::SendError<Message>>)> {
        let mut results = vec![];
        for (peer, message) in messages {
            results.push((peer.clone(), self.send(&peer, message).await));
        }
        results
    }

    pub async fn send(&self, peer: &String, message: Message) -> Result<(), tokio::sync::mpsc::error::SendError<Message>> {
        if let Some(sender) = self.outgoing_connections.get(peer) {
            if let Err(e) = sender.send(message).await {
                warn!(me = self.id, them = peer, "Failed to send response: {}", e);
                return Err(e);
            }
            Ok(())
        } else {
            let mut keys = vec![];
            for kvp in self.outgoing_connections.iter() {
                keys.push(kvp.key().clone());
            }
            warn!(me = self.id, them = peer, nodes = format!("{:?}", keys), "No connection to peer to send {:?}", message);
            Err(tokio::sync::mpsc::error::SendError(message))
        }
    }
}