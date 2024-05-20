use std::{collections::HashMap, env, fs, sync::Arc, time::Duration};

use anyhow::{anyhow, Context, Result};
use chacha20poly1305::{aead::Aead, AeadCore, Key, KeyInit, XChaCha20Poly1305};
use dashmap::DashMap;
use ed25519::{
    pkcs8::{DecodePrivateKey, DecodePublicKey},
    signature::Signer,
    KeypairBytes, PublicKeyBytes, Signature,
};
use ed25519_dalek::{SigningKey as PrivateKey, Verifier, VerifyingKey as PublicKey};
use futures::{SinkExt, StreamExt};
use rand::thread_rng;
use serde::{Deserialize, Serialize};
use tokio::{
    io::Interest,
    net::{
        tcp::{OwnedReadHalf, OwnedWriteHalf},
        TcpListener, TcpStream,
    },
    select,
    sync::{mpsc, Mutex},
    task::JoinSet,
    time::sleep,
};
use tokio_serde::{formats::Cbor, Framed};
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};
use tracing::{info, trace, warn, Instrument};
use x25519_dalek as ecdh;

type Nonce = chacha20poly1305::aead::generic_array::GenericArray<u8, chacha20poly1305::consts::U24>;

use crate::{
    config::{OracleConfig, PeerConfig},
    keys::get_keys_directory,
    raft::RaftMessage,
};

use super::{Message as AppMessage, NodeId};
type WrappedStream = FramedRead<OwnedReadHalf, LengthDelimitedCodec>;
type WrappedSink = FramedWrite<OwnedWriteHalf, LengthDelimitedCodec>;

type SerStream = Framed<WrappedStream, Message, (), Cbor<Message, ()>>;
type DeSink = Framed<WrappedSink, (), Message, Cbor<(), Message>>;

const ORACLE_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Serialize, Deserialize, Clone, Debug)]
struct OpenConnectionMessage {
    /// The app version of the other node
    version: String,
    /// The other node's public key, used to identify them
    id_public_key: PublicKey,
    /// An ephemeral public key, used for ECDH
    ecdh_public_key: ecdh::PublicKey,
    /// The ecdh_public_key, signed with the other node's private key
    signature: Signature,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct ConfirmConnectionMessage {
    /// The ecdh_public_key we sent, signed with the other node's private key
    signature: Signature,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct ApplicationMessage {
    nonce: Vec<u8>,
    payload: Vec<u8>,
}

impl ApplicationMessage {
    pub fn encrypt(message: AppMessage, cipher: &XChaCha20Poly1305) -> Self {
        let nonce: Nonce = {
            let mut rng = thread_rng();
            XChaCha20Poly1305::generate_nonce(&mut rng)
        };
        let payload: Vec<u8> = {
            let bytes = serde_cbor::to_vec(&message).unwrap();
            cipher.encrypt(&nonce, bytes.as_slice()).unwrap()
        };
        let nonce_bytes: Vec<u8> = nonce.into_iter().collect();
        Self {
            nonce: nonce_bytes,
            payload,
        }
    }

    pub fn decrypt(self, cipher: &XChaCha20Poly1305) -> Result<AppMessage> {
        let nonce = Nonce::from_slice(&self.nonce);
        let decrypted_bytes: Vec<u8> = cipher
            .decrypt(nonce, self.payload.as_slice())
            .map_err(|_| anyhow!("could not decipher message"))?;
        serde_cbor::from_slice(&decrypted_bytes).context("could not deserialize message")
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
enum Message {
    OpenConnection(Box<OpenConnectionMessage>), // boxed because it's big
    ConfirmConnection(ConfirmConnectionMessage),
    Application(ApplicationMessage),
}

#[derive(Clone)]
struct Peer {
    id: NodeId,
    public_key: PublicKey,
    label: String,
    address: String,
}

struct IncomingConnection {
    ecdh_public_key: ecdh::PublicKey,
    stream: SerStream,
}
struct OutgoingConnection {
    ecdh_secret: ecdh::EphemeralSecret,
    sink: DeSink,
}

type OutgoingMessageReceiver = mpsc::Receiver<(Option<NodeId>, AppMessage)>;
type IncomingMessageSender = mpsc::Sender<(NodeId, AppMessage)>;

#[derive(Clone)]
pub struct Core {
    pub id: NodeId,
    private_key: Arc<PrivateKey>,
    port: u16,
    peers: Arc<Vec<Peer>>,
    outgoing_rx: Arc<Mutex<OutgoingMessageReceiver>>,
    incoming_tx: Arc<IncomingMessageSender>,
}

impl Core {
    pub fn new(
        config: &OracleConfig,
        outgoing_rx: OutgoingMessageReceiver,
        incoming_tx: IncomingMessageSender,
    ) -> Result<Self> {
        let private_key = Arc::new(read_private_key()?);
        let id = compute_node_id(&private_key.verifying_key());
        info!("This node has ID {}", id);

        let peers = {
            let peers: Result<Vec<Peer>> = config.peers.iter().map(parse_peer).collect();
            Arc::new(peers?)
        };
        Ok(Self {
            id,
            private_key,
            port: config.port,
            peers,
            outgoing_rx: Arc::new(Mutex::new(outgoing_rx)),
            incoming_tx: Arc::new(incoming_tx),
        })
    }

    pub async fn handle_network(self) -> Result<()> {
        let mut set = JoinSet::new();

        let incoming_connection_txs = DashMap::new();
        let mut outgoing_message_txs = HashMap::new();

        // Spawn one task per peer that's responsible for all comms with that peer
        for peer in self.peers.iter() {
            let core = self.clone();
            let peer = peer.clone();

            // Each peer gets a receiver that tells it when a new incoming connection from that peer is open.
            // Hold onto the senders here
            let (incoming_connection_tx, incoming_connection_rx) = mpsc::channel(10);
            incoming_connection_txs.insert(peer.id.clone(), incoming_connection_tx);

            // Each peer also gets a receiver that tells it when the app wants to send a message.
            // Hold onto the senders here
            let (outgoing_message_tx, outgoing_message_rx) = mpsc::channel(10);
            outgoing_message_txs.insert(peer.id.clone(), outgoing_message_tx);

            set.spawn(
                async move {
                    core.handle_peer(peer, incoming_connection_rx, outgoing_message_rx)
                        .await
                }
                .in_current_span(),
            );
        }

        // One task listens for new connections and sends them to the appropriate peer task
        let core = self.clone();
        set.spawn(
            async move { core.accept_connections(incoming_connection_txs).await }.in_current_span(),
        );

        // One task polls for outgoing messages, and tells the appropriate peer task to send them
        set.spawn(
            async move {
                self.send_messages(outgoing_message_txs).await;
            }
            .in_current_span(),
        );

        while let Some(x) = set.join_next().await {
            x?
        }

        Ok(())
    }

    async fn send_messages(self, outgoing_message_txs: HashMap<NodeId, mpsc::Sender<AppMessage>>) {
        let mut outgoing_rx = self.outgoing_rx.lock_owned().await;
        while let Some((to, message)) = outgoing_rx.recv().await {
            match to {
                Some(id) => {
                    // Sending to one node
                    let Some(sender) = outgoing_message_txs.get(&id) else {
                        warn!("Tried sending message to unrecognized node {}", id);
                        continue;
                    };
                    if let Err(e) = sender.send(message).await {
                        warn!("Could not send message to node {}: {:?}", id, e);
                    }
                }
                None => {
                    // Broadcasting to all nodes
                    for (id, sender) in outgoing_message_txs.iter() {
                        if let Err(e) = sender.send(message.clone()).await {
                            warn!("Could not send message to node {}: {:?}", id, e);
                        }
                    }
                }
            }
        }
    }

    async fn accept_connections(
        self,
        incoming_connection_txs: DashMap<NodeId, mpsc::Sender<IncomingConnection>>,
    ) {
        let addr = format!("0.0.0.0:{}", self.port);
        let incoming_connection_txs = Arc::new(incoming_connection_txs);
        info!("Listening on: {}", addr);

        let listener = TcpListener::bind(addr).await.unwrap();
        while let Ok((stream, _)) = listener.accept().await {
            let core = self.clone();
            let txs = incoming_connection_txs.clone();
            // Each incoming connection spawns its own thread to handling incoming messages
            tokio::spawn(
                async move {
                    core.handle_incoming_connection(stream, txs).await;
                }
                .in_current_span(),
            );
        }
    }

    async fn handle_incoming_connection(
        self,
        stream: TcpStream,
        txs: Arc<DashMap<NodeId, mpsc::Sender<IncomingConnection>>>,
    ) {
        trace!("Incoming connection from: {}", stream.peer_addr().unwrap());

        let (read, write) = stream.into_split();
        let stream: WrappedStream = WrappedStream::new(read, LengthDelimitedCodec::new());
        let sink: WrappedSink = WrappedSink::new(write, LengthDelimitedCodec::new());
        let mut stream: SerStream = SerStream::new(stream, Cbor::default());
        let mut sink: DeSink = DeSink::new(sink, Cbor::default());

        let message = match stream.next().await {
            Some(Ok(Message::OpenConnection(message))) => message,
            Some(Ok(other)) => {
                warn!("Expected Hello, got {:?}", other);
                return;
            }
            Some(Err(e)) => {
                warn!("Failed to parse: {:?}", e);
                return;
            }
            None => {
                warn!("Incoming Connection disconnected before handshake");
                return;
            }
        };

        if message.version != ORACLE_VERSION {
            warn!(
                other_version = message.version,
                "Other node is running a different oracle version"
            )
        }

        // Grab the ecdh nonce they sent us
        let ecdh_public_key = message.ecdh_public_key;

        // Figure out who they are based on the public key they sent us
        let id_public_key = message.id_public_key;
        let peer_id = compute_node_id(&id_public_key);
        let Some(peer) = self.peers.iter().find(|p| p.id == peer_id) else {
            warn!("Unrecognized peer {}", peer_id);
            return;
        };
        let them = peer.label.clone();

        // Confirm that they are who they say they are; they should have signed the ecdh nonce with their private key
        let signature = message.signature;
        if let Err(e) = id_public_key.verify(ecdh_public_key.as_bytes(), &signature) {
            warn!(them, "Signature does not match public key: {}", e);
            return;
        }

        // Notify whoever's listening that we have a new connection
        // (We look up the "incoming connection" sender, so if we don't recognize them we fail here)
        let Some(connection_tx) = txs.get(&peer_id) else {
            warn!(them, "Other node not recognized");
            return;
        };
        if let Err(e) = connection_tx
            .send(IncomingConnection {
                ecdh_public_key,
                stream,
            })
            .await
        {
            warn!(them, "Could not send incoming connection: {:?}", e);
            return;
        }

        // And finally, acknowledge them (and prove who we are) by signing their nonce
        let signature = self.private_key.sign(ecdh_public_key.as_bytes());
        match sink
            .send(Message::ConfirmConnection(ConfirmConnectionMessage {
                signature,
            }))
            .await
        {
            Ok(_) => {
                trace!("Outgoing Hello sent");
            }
            Err(e) => {
                warn!("Failed to send hello: {:?}", e);
            }
        }
    }

    async fn handle_peer(
        self,
        peer: Peer,
        mut incoming_connection_rx: mpsc::Receiver<IncomingConnection>,
        outgoing_message_rx: mpsc::Receiver<AppMessage>,
    ) {
        let outgoing_message_rx = Mutex::new(outgoing_message_rx);
        let mut sleep_secs = 1u64;
        loop {
            // Every time the outgoing or incoming connections are closed, we need to reconnect to both.
            // Because we store both connections in local variables inside this loop,
            // they disconnect at the end of every iteration and reconnect at the start.
            let outgoing_connection = match self.connect_to_peer(&peer).await {
                Ok(conn) => conn,
                Err(e) => {
                    warn!("error connecting to {}: {:#}", peer.label, e);
                    sleep(Duration::from_secs(sleep_secs)).await;
                    sleep_secs = if sleep_secs >= 8 { 8 } else { sleep_secs * 2 };
                    continue;
                }
            };
            sleep_secs = 1u64;
            let incoming_connection = select! {
                incoming = incoming_connection_rx.recv() => {
                    let Some(conn) = incoming else {
                        // if this receiver is closed, the system must be shutting down
                        return;
                    };
                    conn
                }
                _ = wait_for_disconnect(&outgoing_connection.sink) => {
                    // While waiting for an incoming connection, the outgoing one disconnected.
                    // Restart the loop to try connecting again.
                    continue;
                }
            };

            // Ok! We finally have incoming and outgoing connections set up.
            // Now we can actually handle messages from across the way
            let mut outgoing_message_rx = outgoing_message_rx.lock().await;
            self.handle_peer_connections(
                &peer,
                incoming_connection,
                outgoing_connection,
                &mut outgoing_message_rx,
            )
            .await;
        }
    }

    async fn connect_to_peer(&self, peer: &Peer) -> Result<OutgoingConnection> {
        trace!("Attempting to connect to {} ({})", peer.id, peer.label);
        let stream = TcpStream::connect(&peer.address)
            .await
            .context("error opening connection")?;
        trace!("Outgoing connection to: {}", stream.peer_addr().unwrap());

        let (read, write) = stream.into_split();

        let stream: WrappedStream = WrappedStream::new(read, LengthDelimitedCodec::new());
        let mut stream: SerStream = SerStream::new(stream, Cbor::default());

        let sink: WrappedSink = WrappedSink::new(write, LengthDelimitedCodec::new());
        let mut sink: DeSink = DeSink::new(sink, Cbor::default());

        // Generate our secret for ECDH
        let ecdh_secret = {
            let rng = thread_rng();
            ecdh::EphemeralSecret::random_from_rng(rng)
        };

        // Generate the message with our ephemeral nonce and our public key
        let id_public_key = self.private_key.verifying_key();
        let ecdh_public_key = ecdh::PublicKey::from(&ecdh_secret);
        let signature = self.private_key.sign(ecdh_public_key.as_bytes());

        let message = OpenConnectionMessage {
            version: ORACLE_VERSION.to_string(),
            id_public_key,
            ecdh_public_key,
            signature,
        };
        sink.send(Message::OpenConnection(Box::new(message)))
            .await
            .context("error sending open message")?;
        trace!("Outgoing Open request sent");

        // Wait for the other side to respond
        let message = match stream.next().await {
            Some(Ok(Message::ConfirmConnection(message))) => message,
            Some(Ok(other)) => {
                return Err(anyhow!("expected ConfirmConnection, got {:?}", other));
            }
            Some(Err(e)) => {
                return Err(anyhow!("failed to parse: {:?}", e));
            }
            None => {
                return Err(anyhow!("outgoing connection disconnected before handshake"));
            }
        };

        // They've signed our nonce, let's confirm they did it right
        let signature = message.signature;
        peer.public_key
            .verify(ecdh_public_key.as_bytes(), &signature)
            .context("signature does not match public key")?;

        // and we're all set!
        Ok(OutgoingConnection { ecdh_secret, sink })
    }

    async fn handle_peer_connections(
        &self,
        peer: &Peer,
        incoming_connection: IncomingConnection,
        outgoing_connection: OutgoingConnection,
        outgoing_message_rx: &mut mpsc::Receiver<AppMessage>,
    ) {
        info!("Connected to {} ({})", peer.id, peer.label);
        // try to tell Raft that we are definitely connected
        let _ = self
            .incoming_tx
            .try_send((peer.id.clone(), AppMessage::Raft(RaftMessage::Connect)));

        let shared_secret = outgoing_connection
            .ecdh_secret
            .diffie_hellman(&incoming_connection.ecdh_public_key);
        let chacha_key = Key::from(shared_secret.to_bytes());
        let chacha = XChaCha20Poly1305::new(&chacha_key);

        let mut sink = outgoing_connection.sink;
        let send_chacha = chacha.clone();
        let them = peer.id.clone();
        let send_task = async move {
            while let Some(message) = outgoing_message_rx.recv().await {
                let message = ApplicationMessage::encrypt(message, &send_chacha);
                if let Err(e) = sink.send(Message::Application(message)).await {
                    warn!(them = %them, "Failed to send message: {:?}", e);
                    break;
                }
            }
        };

        let mut stream = incoming_connection.stream;
        let incoming_message_tx = self.incoming_tx.clone();
        let them = peer.label.clone();
        let recv_task = async move {
            while let Some(msg) = stream.next().await {
                match msg {
                    Ok(Message::Application(message)) => {
                        let message = match message.decrypt(&chacha) {
                            Ok(message) => message,
                            Err(e) => {
                                warn!(them, "Failed to decrypt incoming message: {:#}", e);
                                break;
                            }
                        };
                        if let Err(e) = incoming_message_tx.send((peer.id.clone(), message)).await {
                            warn!(them, "Failed to send message: {:?}", e);
                            break;
                        }
                    }
                    // If someone tries to Hello us again, break it off
                    Ok(other) => {
                        warn!(them, "Unexpected message: {:?}", other);
                        break;
                    }
                    // Someone is sending us messages that we can't parse
                    Err(e) => {
                        warn!(them, "Failed to parse message: {:?}", e);
                        break;
                    }
                }
            }
            warn!(them, "Incoming connection Disconnected");
        };

        // Run until either the sender or receiver task stops running, then return so we can reconnect
        select! {
            _ = send_task => {},
            _ = recv_task => {},
        };

        // try to warn raft that we aren't connected anymore
        let _ = self
            .incoming_tx
            .try_send((peer.id.clone(), AppMessage::Raft(RaftMessage::Disconnect)));
    }
}

async fn wait_for_disconnect(sink: &DeSink) {
    let transport = sink.get_ref().get_ref();
    loop {
        sleep(Duration::from_millis(500)).await;
        let Ok(ready) = transport.ready(Interest::WRITABLE).await else {
            // if we get an error checking the stream's state, just assume it's closed
            return;
        };
        if ready.is_write_closed() || ready.is_error() {
            return;
        }
    }
}

fn parse_peer(config: &PeerConfig) -> Result<Peer> {
    let public_key = {
        let key_bytes = PublicKeyBytes::from_public_key_pem(&config.public_key)?;
        PublicKey::from_bytes(&key_bytes.0)?
    };
    let id = compute_node_id(&public_key);
    let label = config.label.as_ref().unwrap_or(&config.address).clone();
    Ok(Peer {
        id,
        public_key,
        label,
        address: config.address.clone(),
    })
}

fn read_private_key() -> Result<PrivateKey> {
    let key_path = get_keys_directory()?.join("private.pem");
    let key_pem_file = fs::read_to_string(&key_path).context(format!(
        "Could not load private key from {}",
        key_path.display()
    ))?;
    let decoded = KeypairBytes::from_pkcs8_pem(&key_pem_file)?;
    let private_key = PrivateKey::from_bytes(&decoded.secret_key);
    Ok(private_key)
}

fn compute_node_id(public_key: &PublicKey) -> NodeId {
    NodeId::new(hex::encode(public_key.as_bytes()))
}
