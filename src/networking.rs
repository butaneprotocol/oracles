use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::{
    net::{
        tcp::{OwnedReadHalf, OwnedWriteHalf},
        TcpListener, TcpStream,
    },
    task::{JoinError, JoinSet},
};
use tokio_serde::{formats::Cbor, Framed};
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};
use tracing::{info, trace, warn, Instrument};

use crate::{config::PeerConfig, raft::RaftMessage, signature_aggregator::signer::SignerMessage};

type WrappedStream = FramedRead<OwnedReadHalf, LengthDelimitedCodec>;
type WrappedSink = FramedWrite<OwnedWriteHalf, LengthDelimitedCodec>;

type SerStream = Framed<WrappedStream, Message, (), Cbor<Message, ()>>;
type DeSink = Framed<WrappedSink, (), Message, Cbor<(), Message>>;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum Message {
    Hello(String),
    Raft(RaftMessage),
    Signer(SignerMessage),
}

/// A network is a fully connected set of peers
#[derive(Clone)]
pub struct Network {
    id: String,

    // An MPSC channel that can dispatch messages to the raft state machine
    raft_messages: Arc<tokio::sync::mpsc::Sender<RaftMessage>>,
    message_sender: Arc<tokio::sync::mpsc::Sender<(String, Message)>>,

    // Instead of trying to handle the crossing-paths problem, we just maintain one incoming and one outgoing connection for each peer
    // A set of incoming connections
    incoming_connections: Arc<dashmap::DashMap<String, ()>>,
    // A set of outgoing connections, and the channel used to send them messages
    outgoing_connections: Arc<dashmap::DashMap<String, tokio::sync::mpsc::Sender<Message>>>,
}

impl Network {
    pub fn new(
        id: String,
        raft_messages: Arc<tokio::sync::mpsc::Sender<RaftMessage>>,
        message_sender: Arc<tokio::sync::mpsc::Sender<(String, Message)>>,
    ) -> Self {
        Network {
            id,
            raft_messages,
            message_sender,
            incoming_connections: Arc::new(dashmap::DashMap::new()),
            outgoing_connections: Arc::new(dashmap::DashMap::new()),
        }
    }

    /// Spawn threads to listen for new connections, and connect to each other peer (with retry)
    pub async fn handle_network(
        self,
        port: u16,
        peers: Vec<PeerConfig>,
    ) -> Result<Self, JoinError> {
        let mut set = JoinSet::new();

        // Start a listener for incoming connections on the given port
        // each incoming connection will do a handshake to identify the nodes, and start a pump to handle incoming messages
        let network = self.clone();
        let local_addr = format!("0.0.0.0:{}", port);
        set.spawn(
            async move {
                network
                    .accept_connections(local_addr.as_str())
                    .await
                    .unwrap();
            }
            .in_current_span(),
        );

        // Start a thread for each peer, which will attempt to connect (and reconnect on disconnection)
        // while connected, it will maintain a MPSC queue in outgoing_connections that other machines can use to send messages
        for peer in peers {
            let network = self.clone();
            set.spawn(
                async move {
                    network.connect_to(&peer.address).await.unwrap();
                }
                .in_current_span(),
            );
        }

        // Wait for them all to finish (they won't)
        while let Some(res) = set.join_next().await {
            res?
        }
        Ok(self)
    }

    // Accept all incoming connections
    pub async fn accept_connections(self, addr: &str) -> std::io::Result<()> {
        info!("Listening on: {}", addr);

        let listener = TcpListener::bind(addr).await?;
        while let Ok((stream, _)) = listener.accept().await {
            let node = self.clone();
            // Each incoming connection spawns its own thread to handling incoming messages
            tokio::spawn(async move {
                node.handle_incoming_connection(stream).await;
            });
        }

        Ok(())
    }

    // Handle messages from an incoming TCP stream
    async fn handle_incoming_connection(self, stream: TcpStream) {
        trace!("Incoming connection from: {}", stream.peer_addr().unwrap());

        let (read, write) = stream.into_split();
        let stream: WrappedStream = WrappedStream::new(read, LengthDelimitedCodec::new());
        let sink: WrappedSink = WrappedSink::new(write, LengthDelimitedCodec::new());
        let mut stream: SerStream = SerStream::new(stream, Cbor::default());
        let mut sink: DeSink = DeSink::new(sink, Cbor::default());

        let mut them = "".to_string();

        // Incoming connections *first receive* a `Hello`, then send our own
        // Outgoing connections will do the reverse
        match stream.next().await {
            Some(Ok(Message::Hello(other))) => {
                them = other;
                trace!(them = them, "Incoming Hello received");
            }
            Some(Ok(other)) => {
                warn!(them = them, "Expected Hello, got {:?}", other);
                return;
            }
            Some(Err(e)) => {
                warn!(them = them, "Failed to parse: {}", e);
                return;
            }
            None => {
                warn!(
                    them = them,
                    "Incoming Connection disconnected before handshake"
                );
                return;
            }
        }

        match sink.send(Message::Hello(self.id.clone())).await {
            Ok(_) => {
                trace!(them = them, "Incoming Hello Sent");
            }
            Err(e) => {
                warn!(them = them, "Failed to send hello: {}", e);
                return;
            }
        }

        trace!("Handshake done, reading incoming messages...");

        self.incoming_connections.insert(them.clone(), ());
        // Once both connections have been established, tell raft about it so it starts doing heartbeats / reaching consensus
        // If we only get an incoming connection, we don't want raft to start trying to send messages; and if we only get an outgoing connection,
        // then we may not be able to reach consensus yet
        if self.outgoing_connections.contains_key(&them) {
            self.raft_messages
                .clone()
                .send(RaftMessage::Connect {
                    node_id: them.clone(),
                })
                .await
                .unwrap();
        }

        // Then, so long as we're receiving messages, we can dispatch them to the right machine
        while let Some(msg) = stream.next().await {
            trace!("Received message {:?} from {}", msg, them);

            if let Ok(message) = &msg {
                if let Err(e) = self
                    .message_sender
                    .send((them.clone(), message.clone()))
                    .await
                {
                    warn!(them = them, "Failed to send message: {}", e);
                    break;
                }
            }

            match msg {
                Ok(Message::Raft(raft)) => {
                    if let Err(e) = self.raft_messages.send(raft).await {
                        warn!(them = them, "Failed to send raft message: {}", e);
                        break;
                    }
                }
                Ok(Message::Signer(_)) => {
                    // handled elsewhere
                }
                // If someone tries to Hello us again, break it off
                Ok(_) => {
                    warn!(them = them, "Unexpected message");
                    break;
                }
                // Someone is sending us messages that we can't parse
                Err(e) => {
                    warn!(them = them, "Failed to parse message: {}", e);
                    break;
                }
            }
        }
        self.incoming_connections.remove(&them);
        // NOTE: we *don't* send the raft disconnect message here; we should continue to send them heartbeats
        // so that we can notice when the socket disconnects
        warn!(them = them, "Incoming connection Disconnected");
    }

    // Connect to a given peer, and reconnect if disconnected
    pub async fn connect_to(self, addr: &str) -> std::io::Result<()> {
        let mut connection_attempts = 0;
        let mut reconnections = 0;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            trace!(
                connection_attempts = connection_attempts,
                reconnections = reconnections,
                "Attempting to connect to: {}",
                addr
            );
            connection_attempts += 1;
            let connection = TcpStream::connect(addr).await;
            match connection {
                Ok(stream) => {
                    // If we did manage to connect, then hand off to this method to actually handle sending messages
                    self.handle_outgoing_connection(stream).await;
                    warn!("Outgoing peer {:?} disconnected", addr);
                    reconnections += 1;
                    // If we disconnect, attempt to reconnect
                    continue;
                }
                Err(_) => {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    // Peer isn't online, try again
                    continue;
                }
            }
        }
    }

    // Handle sending messages to a specific connection
    async fn handle_outgoing_connection(&self, stream: TcpStream) {
        trace!("Outgoing connection to: {}", stream.peer_addr().unwrap());

        let (read, write) = stream.into_split();

        let stream: WrappedStream = WrappedStream::new(read, LengthDelimitedCodec::new());
        let mut stream: SerStream = SerStream::new(stream, Cbor::default());

        let sink: WrappedSink = WrappedSink::new(write, LengthDelimitedCodec::new());
        let mut sink: DeSink = DeSink::new(sink, Cbor::default());

        // First, send our ID, because they're waiting for it on connection
        match sink.send(Message::Hello(self.id.clone())).await {
            Ok(_) => {
                trace!("Outgoing Hello sent");
            }
            Err(e) => {
                warn!("Failed to send hello: {}", e);
                return;
            }
        }
        let them: String;
        // Now, read the expected `Hello` coming back, so we know their ID
        match stream.next().await {
            Some(Ok(Message::Hello(other))) => {
                them = other;
                trace!(them = them, "Outgoing Hello received");
            }
            Some(Ok(other)) => {
                warn!("Expected Hello, got {:?}", other);
                return;
            }
            Some(Err(e)) => {
                warn!("Failed to parse: {}", e);
                return;
            }
            None => {
                warn!("Outgoing connection disconnected");
                return;
            }
        }

        // Now, setup a mpsc channel for outgoing messages
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        self.outgoing_connections.insert(them.clone(), tx.clone());

        // Once both connections have been established, tell raft about it
        // until we can both send and receive, it doesn't make sense to consider them for raft elections, for example
        if self.incoming_connections.contains_key(&them) {
            self.raft_messages
                .clone()
                .send(RaftMessage::Connect {
                    node_id: them.clone(),
                })
                .await
                .unwrap();
        }

        // So long as we have someone try to receive, and the socket isn't broken, we can send messages to the socket
        while let Some(next) = rx.recv().await {
            trace!("Sending message {:?} to {}", next, them);
            if let Err(e) = sink.send(next).await {
                warn!("Failed to send message: {}", e);
                break;
            }
        }
        // We got disconnected, so let the Raft protocol know to stop sending messages, and then return so we can try to connect again
        warn!("Outgoing connection Disconnected");
        self.outgoing_connections.remove(&them);
        self.raft_messages
            .clone()
            .send(RaftMessage::Disconnect {
                node_id: them.clone(),
            })
            .await
            .unwrap();
    }

    // send a message, by looking up the outgoing connection mpsc and sending to it
    pub async fn send(
        &self,
        peer: &str,
        message: Message,
    ) -> Result<(), tokio::sync::mpsc::error::SendError<Message>> {
        if let Some(sender) = self.outgoing_connections.get(peer) {
            if let Err(e) = sender.send(message).await {
                warn!(them = peer, "Failed to send response: {}", e);
                return Err(e);
            }
            Ok(())
        } else {
            let mut keys = vec![];
            for kvp in self.outgoing_connections.iter() {
                keys.push(kvp.key().clone());
            }
            warn!(
                them = peer,
                nodes = format!("{:?}", keys),
                "No connection to peer to send {:?}",
                message
            );
            Err(tokio::sync::mpsc::error::SendError(message))
        }
    }

    pub async fn broadcast(&self, message: Message) {
        for kvp in self.outgoing_connections.iter() {
            let peer = kvp.key();
            let sender = kvp.value();
            if let Err(e) = sender.send(message.clone()).await {
                warn!(
                    them = peer,
                    "No connection to peer to broadcast {:?}: {}", message, e
                );
            };
        }
    }
}
