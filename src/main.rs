use std::sync::Arc;

use dashmap::DashMap;
use networking::Message;
use raft::{RaftMessage, RaftState};
use futures::{SinkExt, StreamExt};
use tokio::{net::{
    tcp::{OwnedReadHalf, OwnedWriteHalf},
    TcpListener,
    TcpStream,
}, sync::mpsc};
use clap::Parser;
use tokio_serde::{formats::Cbor, Framed};
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};
use tracing::{info, trace, warn, Level};
use tracing_subscriber::FmtSubscriber;

pub mod networking;
pub mod raft;

type WrappedStream = FramedRead<OwnedReadHalf, LengthDelimitedCodec>;
type WrappedSink = FramedWrite<OwnedWriteHalf, LengthDelimitedCodec>;

type SerStream = Framed<WrappedStream, Message, (), Cbor<Message, ()>>;
type DeSink = Framed<WrappedSink, (), Message, Cbor<(), Message>>;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[clap(short, long)]
    id: Option<String>,

    #[clap(short, long)]
    port: u16,
    // A list of peers to connect to
    #[clap(long)]
    peers: Vec<String>
}

#[derive(Clone)]
struct Node {
    id: String,
    quorum: usize,
    incoming_raft_messages: Arc<mpsc::Sender<RaftMessage>>,
    heartbeat: std::time::Duration,
    timeout: std::time::Duration,
    nodes: Arc<DashMap<String, mpsc::Sender<Message>>>,
}

impl Node {
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
        while let Some(msg) = stream.next().await {
            trace!("Received message {:?} from {}", msg, them);

            match msg {
                Ok(Message::Raft(raft)) => {
                    match self.incoming_raft_messages.send(raft).await {
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
                    reconnections += 1; 
                    // If we disconnect, attempt to reconnect
                    continue
                }
                Err(_) => {
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
        self.nodes.insert(them.clone(), tx.clone());
        self.incoming_raft_messages.clone().send(RaftMessage::Connect { node_id: them.clone() }).await.unwrap();
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
        self.nodes.remove(&them);
    }

    async fn handle_raft(&mut self, mut rx: mpsc::Receiver<RaftMessage>) {
        let mut state = RaftState::new(self.id.clone(), self.quorum, self.heartbeat, self.timeout);

        loop {
            let next_message = rx.try_recv();
            let timestamp = std::time::Instant::now();
            
            let responses = match next_message {
                Ok(msg) => state.receive(timestamp, msg),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => state.tick(timestamp),
                Err(err) => {
                    warn!(me = self.id, "Failed to receive message: {}", err);
                    vec![]
                }
            };

            // Send out any responses
            for (peer, response) in responses {
                if let Err(_) = self.send(&peer, Message::Raft(response)).await {
                    state.receive(timestamp, RaftMessage::Disconnect { node_id: peer.clone() });
                }
            }
            // Yield back to the scheduler, so that other tasks can run
            tokio::task::yield_now().await;
        }
    }

    async fn send(&mut self, peer: &String, message: Message) -> Result<(), mpsc::error::SendError<Message>> {
        if let Some(sender) = self.nodes.get(peer) {
            if let Err(e) = sender.send(message).await {
                warn!(me = self.id, them = peer, "Failed to send response: {}", e);
                self.nodes.remove(peer);
                Err(e)
            } else {
                Ok(())
            }
        } else {
            warn!(me = self.id, them = peer, "No connection to peer");
            // TODO: anyhow?
            Err(mpsc::error::SendError(message))
        }
    }
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    let subscriber = FmtSubscriber::builder()
        // all spans/events with a level higher than TRACE (e.g, debug, info, warn, etc.)
        // will be written to stdout.
        .with_max_level(Level::INFO)
        // completes the builder.
        .finish();
    tracing::subscriber::set_global_default(subscriber)
        .expect("setting default subscriber failed");

    let id = args.id.unwrap_or(uuid::Uuid::new_v4().to_string());
    info!(me = id, "Node starting...");
    let (tx, rx) = tokio::sync::mpsc::channel(10);

    let node = Node {
        id: id.clone(),
        quorum: (args.peers.len() + 1 + 1) / 2,
        timeout: std::time::Duration::from_millis((500) as u64),
        heartbeat: std::time::Duration::from_millis(100),
        incoming_raft_messages: Arc::new(tx),
        nodes: Arc::new(DashMap::new()),
    };

    // Set up the peer-to-peer connections
    let local_addr = format!("localhost:{}", args.port);
    let mut tasks = vec![];

    {
        let mut node = node.clone();
        let raft_task = tokio::spawn(async move {
            node.handle_raft(rx).await;
        });
        tasks.push(raft_task);
    }

    {
        let node = node.clone();
        let listener_task = tokio::spawn(async move {
            node.accept_connections(local_addr.as_str()).await.unwrap();
        });
        tasks.push(listener_task);
    }

    for peer in args.peers {
        let node = node.clone();
        let connect_task = tokio::spawn(async move {
            node.connect_to(peer.as_str()).await.unwrap();
        });
        tasks.push(connect_task);
    }

    for task in tasks {
        task.await.unwrap();
    }
}
