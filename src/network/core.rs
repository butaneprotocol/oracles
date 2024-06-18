use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    sync::Arc,
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use chacha20poly1305::{aead::Aead, AeadCore, Key, KeyInit, XChaCha20Poly1305};
use dashmap::DashMap;
use ed25519::{
    pkcs8::{DecodePrivateKey, DecodePublicKey},
    signature::Signer,
    KeypairBytes, PublicKeyBytes, Signature,
};
use ed25519_dalek::{SigningKey as PrivateKey, Verifier, VerifyingKey as PublicKey};
use minicbor::{bytes::ByteVec, Decode, Decoder, Encode, Encoder};
use minicbor_io::{AsyncReader, AsyncWriter};
use rand::thread_rng;
use tokio::{
    join,
    net::{
        tcp::{OwnedReadHalf, OwnedWriteHalf},
        TcpListener, TcpStream,
    },
    select,
    sync::{mpsc, watch, Mutex},
    task::JoinSet,
    time::{sleep, timeout},
};
use tokio_util::compat::{Compat, TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tracing::{error, info, trace, warn, Instrument};
use x25519_dalek::{self as ecdh, SharedSecret};

type Nonce = chacha20poly1305::aead::generic_array::GenericArray<u8, chacha20poly1305::consts::U24>;

use crate::{
    cbor::{CborEcdhPublicKey, CborSignature, CborVerifyingKey},
    config::{OracleConfig, PeerConfig},
    keys,
    raft::RaftMessage,
};

use super::{Message as AppMessage, NodeId};
type EncodeSink = AsyncWriter<Compat<OwnedWriteHalf>>;
type DecodeStream = AsyncReader<Compat<OwnedReadHalf>>;

const ORACLE_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Decode, Encode, Clone, Debug)]
struct OpenConnectionMessage {
    /// The app version of the other node
    #[n(0)]
    version: String,
    /// The other node's public key, used to identify them
    #[n(1)]
    id_public_key: CborVerifyingKey,
    /// An ephemeral public key, used for ECDH
    #[n(2)]
    ecdh_public_key: CborEcdhPublicKey,
    /// The ecdh_public_key, signed with the other node's private key
    #[n(3)]
    signature: CborSignature,
}

#[derive(Decode, Encode, Clone, Debug)]
struct ConfirmConnectionMessage {
    /// The app version of the other node
    #[n(0)]
    version: String,
    /// An ephemeral public key, used for ECDH
    #[n(1)]
    ecdh_public_key: CborEcdhPublicKey,
    /// The ecdh_public_key we sent, signed with the other node's private key
    #[n(2)]
    signature: CborSignature,
}

#[derive(Decode, Encode, Clone, Debug)]
struct ApplicationMessage {
    #[n(0)]
    nonce: ByteVec,
    #[n(1)]
    payload: ByteVec,
}

impl ApplicationMessage {
    pub fn encrypt(message: AppMessage, cipher: &XChaCha20Poly1305) -> Self {
        let nonce: Nonce = {
            let mut rng = thread_rng();
            XChaCha20Poly1305::generate_nonce(&mut rng)
        };
        let payload: Vec<u8> = {
            let mut encoder = Encoder::new(vec![]);
            encoder.encode(&message).expect("infallible");
            let bytes = encoder.into_writer();
            cipher.encrypt(&nonce, bytes.as_slice()).unwrap()
        };
        let nonce_bytes: Vec<u8> = nonce.into_iter().collect();
        Self {
            nonce: nonce_bytes.into(),
            payload: payload.into(),
        }
    }

    pub fn decrypt(self, cipher: &XChaCha20Poly1305) -> Result<AppMessage> {
        let nonce = Nonce::from_slice(&self.nonce);
        let decrypted_bytes: Vec<u8> = cipher
            .decrypt(nonce, self.payload.as_slice())
            .map_err(|_| anyhow!("could not decipher message"))?;
        Decoder::new(&decrypted_bytes)
            .decode()
            .context("could not deserialize message")
    }
}

#[derive(Decode, Encode, Clone, Debug)]
enum Message {
    #[n(0)]
    OpenConnection(#[n(0)] Box<OpenConnectionMessage>), // boxed because it's big
    #[n(1)]
    ConfirmConnection(#[n(0)] ConfirmConnectionMessage),
    #[n(2)]
    Application(#[n(0)] ApplicationMessage),
    #[n(3)]
    Disconnect(#[n(0)] String),
}

#[derive(Clone)]
struct Peer {
    id: NodeId,
    public_key: PublicKey,
    label: String,
    address: String,
}

type OutgoingMessageReceiver = mpsc::Receiver<(Option<NodeId>, AppMessage)>;
type IncomingMessageSender = mpsc::Sender<(NodeId, AppMessage)>;

#[derive(Clone)]
pub struct Core {
    pub id: NodeId,
    private_key: Arc<PrivateKey>,
    port: u16,
    peers: Arc<BTreeMap<NodeId, Peer>>,
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
            Arc::new(
                peers?
                    .into_iter()
                    .filter(|p| p.id != id)
                    .map(|p| (p.id.clone(), p))
                    .collect(),
            )
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

    pub fn peers_count(&self) -> usize {
        self.peers.len()
    }

    pub async fn handle_network(self) -> Result<()> {
        let mut set = JoinSet::new();

        let mut outgoing_message_txs = HashMap::new();

        let (outgoing_peers, incoming_peers): (Vec<_>, Vec<_>) = self
            .peers
            .iter()
            .map(|(_, peer)| peer)
            .partition(|p| self.should_initiate_connection_to(&p.id));

        // For each peer that we should connect to, spawn a task to connect to them
        for peer in outgoing_peers {
            let core = self.clone();
            let peer = peer.clone();

            trace!("This node will initiate connections to {}", peer.label);

            // Each peer gets a receiver that tells it when the app wants to send a message.
            // Hold onto the senders here
            let (outgoing_message_tx, outgoing_message_rx) = mpsc::channel(10);
            outgoing_message_txs.insert(peer.id.clone(), outgoing_message_tx);

            set.spawn(
                async move {
                    core.handle_outgoing_connection(peer, outgoing_message_rx)
                        .await
                }
                .in_current_span(),
            );
        }

        let outgoing_message_rxs = DashMap::new();
        for peer in incoming_peers {
            trace!("This node will expect connections from {}", peer.label);

            // Each peer gets a receiver that tells it when the app wants to send a message.
            // Build a map of these receivers for incoming connections.
            let (outgoing_message_tx, outgoing_message_rx) = mpsc::channel(10);
            outgoing_message_txs.insert(peer.id.clone(), outgoing_message_tx);
            outgoing_message_rxs.insert(peer.id.clone(), Mutex::new(outgoing_message_rx));
        }

        // One task listens for new connections and sends them to the appropriate peer task
        let core = self.clone();
        set.spawn(
            async move { core.accept_connections(outgoing_message_rxs).await }.in_current_span(),
        );

        // One task polls for outgoing messages, and tells the appropriate peer task to send them
        set.spawn(
            async move {
                self.send_messages(outgoing_message_txs).await;
            }
            .in_current_span(),
        );

        while let Some(x) = set.join_next().await {
            x?;
        }

        Ok(())
    }

    async fn send_messages(self, outgoing_message_txs: HashMap<NodeId, mpsc::Sender<AppMessage>>) {
        let mut outgoing_rx = self.outgoing_rx.lock_owned().await;
        let mut unhealthy_peers = HashSet::new();
        while let Some((to, message)) = outgoing_rx.recv().await {
            let recipients = match &to {
                Some(id) => {
                    // Sending to one node
                    let Some(sender) = outgoing_message_txs.get(id) else {
                        warn!("Tried sending message to unrecognized node {}", id);
                        continue;
                    };
                    vec![(id, sender)]
                }
                None => {
                    // Broadcasting to all nodes
                    outgoing_message_txs.iter().collect()
                }
            };
            for (id, sender) in recipients {
                match sender.try_send(message.clone()) {
                    Ok(()) => {
                        unhealthy_peers.remove(id);
                    }
                    Err(err) => {
                        if unhealthy_peers.insert(id.clone()) {
                            warn!("Could not send message to {}: {} (won't log this again until it recovers)", id, err)
                        }
                    }
                };
            }
        }
    }

    async fn accept_connections(
        self,
        outgoing_message_rxs: DashMap<NodeId, Mutex<mpsc::Receiver<AppMessage>>>,
    ) {
        let addr = format!("0.0.0.0:{}", self.port);
        let outgoing_message_rxs = Arc::new(outgoing_message_rxs);
        info!("Listening on: {}", addr);

        let listener = TcpListener::bind(addr).await.unwrap();
        while let Ok((stream, _)) = listener.accept().await {
            let core = self.clone();
            let rxs = outgoing_message_rxs.clone();
            tokio::spawn(
                async move {
                    core.handle_incoming_connection(stream, rxs).await;
                }
                .in_current_span(),
            );
        }
    }

    async fn handle_incoming_connection(
        self,
        stream: TcpStream,
        rxs: Arc<DashMap<NodeId, Mutex<mpsc::Receiver<AppMessage>>>>,
    ) {
        trace!(
            "Incoming connection from {}, waiting for OpenConnection message",
            stream.peer_addr().unwrap()
        );

        let mut them = format!("<unknown> ({})", stream.peer_addr().unwrap());

        let (read, write) = stream.into_split();
        let mut stream: DecodeStream = DecodeStream::new(read.compat());
        let mut sink: EncodeSink = EncodeSink::new(write.compat_write());

        let (peer, secret) = match self
            .handshake_incoming(&mut them, &mut stream, &mut sink)
            .await
            .context("error establishing shared secret")
        {
            Ok((peer, secret)) => (peer, secret),
            Err(error) => {
                warn!(them, "{:#}", error);
                try_send_disconnect(&them, &mut sink, format!("{:#}", error)).await;
                return;
            }
        };

        let Some(outgoing_message_rx_mutex) = rxs.get(&peer.id) else {
            error!(them, "Missing outgoing message receiver");
            try_send_disconnect(&them, &mut sink, "Missing outgoing message receiver".into()).await;
            return;
        };
        let mut outgoing_message_rx = match outgoing_message_rx_mutex.try_lock() {
            Ok(lock) => lock,
            Err(_) => {
                warn!(
                    them,
                    "Cannot establish a new incoming connection, we already have one"
                );
                try_send_disconnect(&them, &mut sink, "You are already connected".into()).await;
                return;
            }
        };

        self.handle_peer_connection(&peer, secret, sink, stream, &mut outgoing_message_rx)
            .await;
    }

    async fn handshake_incoming(
        &self,
        them: &mut String,
        stream: &mut DecodeStream,
        sink: &mut EncodeSink,
    ) -> Result<(Peer, SharedSecret)> {
        let message = match stream.read().await.context("error waiting for handshake")? {
            Some(Message::OpenConnection(message)) => message,
            Some(Message::Disconnect(reason)) => {
                return Err(anyhow!("other party disconnected immediately: {}", reason));
            }
            Some(other) => {
                return Err(anyhow!("expected OpenConnection, got {:?}", other));
            }
            None => {
                return Err(anyhow!("expected OpenConnection, got empty message"));
            }
        };
        trace!(them, "OpenConnection message received");

        // Grab the ecdh nonce they sent us
        let other_ecdh_public_key: ecdh::PublicKey = message.ecdh_public_key.into();

        // Figure out who they are based on the public key they sent us
        let id_public_key = message.id_public_key.into();
        let peer_id = compute_node_id(&id_public_key);
        let Some(peer) = self.peers.get(&peer_id) else {
            return Err(anyhow!("Unrecognized peer {}", peer_id));
        };

        them.clone_from(&peer.label);
        if message.version != ORACLE_VERSION {
            warn!(
                them,
                other_version = message.version,
                "Other node is running a different oracle version"
            )
        }

        // Confirm that they are who they say they are; they should have signed the ecdh nonce with their private key
        let signature: Signature = message.signature.into();
        id_public_key
            .verify(other_ecdh_public_key.as_bytes(), &signature)
            .context("signature does not match public key")?;

        // Confirm that we expect this node to reach out to us, instead of vice versa
        if !self.should_receive_connection_from(&peer_id) {
            return Err(anyhow!(
                "did not expect peer to initiate connection with us"
            ));
        }

        // Generate our own ECDH secret
        let ecdh_secret = {
            let rng = thread_rng();
            ecdh::EphemeralSecret::random_from_rng(rng)
        };

        // Respond to the other client's open request with our own
        let ecdh_public_key = ecdh::PublicKey::from(&ecdh_secret);
        let signature = self.private_key.sign(ecdh_public_key.as_bytes());

        let message = ConfirmConnectionMessage {
            version: ORACLE_VERSION.to_string(),
            ecdh_public_key: ecdh_public_key.into(),
            signature: signature.into(),
        };
        sink.write(Message::ConfirmConnection(message))
            .await
            .context("error sending ConfirmConnection message")?;
        trace!(them, "ConfirmConnection message sent");

        let peer = peer.clone();
        let secret = ecdh_secret.diffie_hellman(&other_ecdh_public_key);
        Ok((peer, secret))
    }

    async fn handle_outgoing_connection(
        self,
        peer: Peer,
        mut outgoing_message_rx: mpsc::Receiver<AppMessage>,
    ) {
        let them = peer.label.clone();
        let mut sleep_seconds = 1;
        loop {
            let stream = match self
                .open_connection(&peer)
                .await
                .context("error opening connection")
            {
                Ok(stream) => stream,
                Err(error) => {
                    warn!(them, "{:#}", error);
                    sleep(Duration::from_secs(sleep_seconds)).await;
                    if sleep_seconds < 8 {
                        sleep_seconds *= 2;
                    }
                    continue;
                }
            };

            let (read, write) = stream.into_split();

            let mut stream = DecodeStream::new(read.compat());
            let mut sink = EncodeSink::new(write.compat_write());

            let secret = match self
                .handshake_outgoing(&peer, &mut stream, &mut sink)
                .await
                .context("error establishing shared secret")
            {
                Ok(secret) => secret,
                Err(error) => {
                    warn!(them, "{:#}", error);
                    try_send_disconnect(&them, &mut sink, format!("{:#}", error)).await;
                    sleep(Duration::from_secs(sleep_seconds)).await;
                    if sleep_seconds < 8 {
                        sleep_seconds *= 2;
                    }
                    continue;
                }
            };

            sleep_seconds = 1;

            self.handle_peer_connection(&peer, secret, sink, stream, &mut outgoing_message_rx)
                .await;
        }
    }

    async fn open_connection(&self, peer: &Peer) -> Result<TcpStream> {
        let them = peer.label.clone();
        trace!(them, "Attempting to connect to {}", peer.id);
        let stream = TcpStream::connect(&peer.address)
            .await
            .context("error opening connection")?;

        trace!(
            them,
            "Opening connection to: {}",
            stream.peer_addr().unwrap()
        );
        stream
            .set_nodelay(true)
            .context("error setting TCP_NODELAY")?;
        trace!(them, "Set TCP_NODELAY");
        Ok(stream)
    }

    async fn handshake_outgoing(
        &self,
        peer: &Peer,
        stream: &mut DecodeStream,
        sink: &mut EncodeSink,
    ) -> Result<SharedSecret> {
        let them = peer.label.clone();

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
            id_public_key: id_public_key.into(),
            ecdh_public_key: ecdh_public_key.into(),
            signature: signature.into(),
        };
        sink.write(Message::OpenConnection(Box::new(message)))
            .await
            .context("error sending open message")?;
        trace!(
            them,
            "OpenConnection message sent, waiting for ConfirmConnection response"
        );

        // Wait for the other side to respond
        let message = match stream
            .read()
            .await
            .context("error waiting for handshake response")?
        {
            Some(Message::ConfirmConnection(message)) => message,
            Some(Message::Disconnect(reason)) => {
                return Err(anyhow!("Other side disconnected: {}", reason))
            }
            Some(other) => {
                return Err(anyhow!("Expected ConfirmConnection, got {:?}", other));
            }
            None => {
                return Err(anyhow!("Expected ConfirmConnection, got empty message"));
            }
        };
        trace!(them, "ConfirmConnection message received");
        if message.version != ORACLE_VERSION {
            warn!(
                them,
                other_version = message.version,
                "Other node is running a different oracle version"
            )
        }

        // They've sent us a signed nonce, let's confirm they are who they say they are
        let other_ecdh_key: ecdh::PublicKey = message.ecdh_public_key.into();
        let signature = message.signature.into();

        peer.public_key
            .verify(other_ecdh_key.as_bytes(), &signature)
            .context("signature does not match public key")?;

        // And now the handshake is done and we have our secret
        let secret = ecdh_secret.diffie_hellman(&other_ecdh_key);
        Ok(secret)
    }

    async fn handle_peer_connection(
        &self,
        peer: &Peer,
        secret: SharedSecret,
        mut sink: EncodeSink,
        mut stream: DecodeStream,
        outgoing_message_rx: &mut mpsc::Receiver<AppMessage>,
    ) {
        let them = peer.label.clone();
        info!(them, "Connected to {}", peer.id);

        // try to tell Raft that we are definitely connected
        let _ = self
            .incoming_tx
            .try_send((peer.id.clone(), AppMessage::Raft(RaftMessage::Connect)));

        let (disconnect_tx, mut disconnect_rx) = watch::channel(String::new());
        let disconnect_tx = Arc::new(disconnect_tx);

        let chacha_key = Key::from(secret.to_bytes());
        let chacha = XChaCha20Poly1305::new(&chacha_key);

        let send_chacha = chacha.clone();
        let send_disconnect_tx = disconnect_tx.clone();
        let mut send_disconnect_rx = disconnect_rx.clone();
        let send_task = async move {
            let disconnect_reason = loop {
                select! {
                    _ = send_disconnect_rx.changed() => {
                        break send_disconnect_rx.borrow().clone();
                    }
                    message = outgoing_message_rx.recv() => {
                        let Some(message) = message else {
                            break "Connection was closed".into();
                        };
                        let message = ApplicationMessage::encrypt(message, &send_chacha);
                        if let Err(e) = sink.write(Message::Application(message)).await {
                            break format!("Failed to send message: {}", e);
                        }
                    }
                }
            };
            warn!(them, "Ending sender task: {}", disconnect_reason);
            send_disconnect_tx.send_replace(disconnect_reason.clone());
            try_send_disconnect(&them, &mut sink, disconnect_reason).await;
        }
        .in_current_span();

        let incoming_message_tx = self.incoming_tx.clone();
        let them = peer.label.clone();
        let recv_task = async move {
            let disconnect_reason = loop {
                select! {
                    _ = disconnect_rx.changed() => {
                        break disconnect_rx.borrow().clone();
                    }
                    incoming = stream.read() => match incoming {
                        Ok(Some(Message::Application(message))) => {
                            let message = match message.decrypt(&chacha) {
                                Ok(message) => message,
                                Err(e) => break format!("Failed to decrypt incoming message: {:#}", e)
                            };
                            if let Err(e) = incoming_message_tx.send((peer.id.clone(), message)).await {
                                break format!("Failed to process incoming message: {}", e);
                            }
                        },
                        Ok(Some(Message::Disconnect(reason))) => {
                            warn!(them, "Peer has disconnected from us: {}", reason);
                            return;
                        }
                        Ok(Some(other)) => {
                            break format!("Expected Application message, got: {:?}", other);
                        }
                        Ok(None) => {
                            trace!(them, "Expected Application message, got empty message");
                            continue;
                        }
                        Err(e) => {
                            break format!("Error reading from stream: {}", e);
                        }
                    }
                }
            };
            warn!(them, "Ending receiver task: {}", disconnect_reason);
            disconnect_tx.send_replace(disconnect_reason);
        };

        join!(send_task, recv_task);

        // try to warn raft that we aren't connected anymore
        let _ = self
            .incoming_tx
            .try_send((peer.id.clone(), AppMessage::Raft(RaftMessage::Disconnect)));
    }

    fn should_initiate_connection_to(&self, peer_id: &NodeId) -> bool {
        &self.id < peer_id
    }

    fn should_receive_connection_from(&self, peer_id: &NodeId) -> bool {
        &self.id > peer_id
    }
}

async fn try_send_disconnect(them: &str, sink: &mut EncodeSink, reason: String) {
    match timeout(
        Duration::from_secs(3),
        sink.write(Message::Disconnect(reason)),
    )
    .await
    {
        Err(timeout) => warn!(them, "could not send disconnect message: {}", timeout),
        Ok(Err(send)) => warn!(them, "could not send disconnect message: {}", send),
        Ok(Ok(_)) => {}
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
    let key_path = keys::get_keys_directory()?.join("private.pem");
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
