use std::{collections::BTreeMap, net::SocketAddr, sync::Arc};

use axum::{
    Json, Router,
    extract::{ConnectInfo, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tokio::{
    net::TcpListener,
    sync::{mpsc, watch},
    task::JoinSet,
};
use tracing::{info, warn};

use crate::{
    config::{NetworkConfig, Peer},
    keys,
    network::NodeId,
    raft::{RaftClient, RaftLeader},
};

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Origin {
    Source(String),
    SyntheticConfig(String),
    Currency(String),
    Peer(NodeId),
}

pub enum HealthStatus {
    Healthy,
    Unhealthy(String),
}

enum HealthMessage {
    Status {
        origin: Origin,
        status: HealthStatus,
    },
    PeerVersion {
        peer: NodeId,
        version: String,
    },
    DkgStatus {
        peer: NodeId,
        status: String,
    },
}

type HealthSource = mpsc::UnboundedReceiver<HealthMessage>;
#[derive(Clone)]
struct HealthState {
    id: NodeId,
    frost: Option<FrostStatus>,
    peers: Vec<Peer>,
    leader_source: Arc<watch::Receiver<RaftLeader>>,
    peer_versions: Arc<DashMap<NodeId, String>>,
    peer_dkg_statuses: Arc<DashMap<NodeId, String>>,
    statuses: Arc<DashMap<Origin, HealthStatus>>,
    raft_client: Arc<RaftClient>,
}
impl HealthState {
    fn peer(&self, id: &NodeId) -> &Peer {
        self.peers.iter().find(|p| p.id == *id).unwrap()
    }
}

#[derive(Clone)]
pub struct HealthSink(Option<mpsc::UnboundedSender<HealthMessage>>);
impl HealthSink {
    pub fn noop() -> Self {
        Self(None)
    }
    pub fn update(&self, origin: Origin, status: HealthStatus) {
        let message = HealthMessage::Status {
            origin: origin.clone(),
            status,
        };
        self.try_send(
            message,
            &format!("Could not update health for {:?}", origin),
        );
    }
    pub fn track_peer_version(&self, peer: &NodeId, version: &str) {
        let message = HealthMessage::PeerVersion {
            peer: peer.clone(),
            version: version.to_string(),
        };
        self.try_send(message, &format!("Could not track version for {}", peer));
    }
    pub fn update_dkg(&self, peer: &NodeId, status: String) {
        let message = HealthMessage::DkgStatus {
            peer: peer.clone(),
            status,
        };
        self.try_send(message, &format!("Could not track DKG status for {}", peer));
    }
    fn try_send(&self, message: HealthMessage, error_desc: &str) {
        let Some(sink) = self.0.as_ref() else {
            return;
        };
        let result = sink.send(message);
        if let Err(err) = result {
            warn!("{}: {}", error_desc, err);
        }
    }
}

pub struct HealthServer {
    source: HealthSource,
    state: HealthState,
}
impl HealthServer {
    pub fn new(
        frost_address: Option<&String>,
        network_config: &NetworkConfig,
        leader_source: watch::Receiver<RaftLeader>,
        raft_client: RaftClient,
    ) -> (Self, HealthSink) {
        let frost = frost_address.and_then(|hash| {
            let keys_dir = keys::get_keys_directory().ok()?;
            let (_, public_key) = keys::read_frost_keys(&keys_dir, hash).ok()?;
            Some(FrostStatus {
                hash: hash.clone(),
                key: hex::encode(public_key.verifying_key().serialize().ok()?),
            })
        });
        let (sink, source) = mpsc::unbounded_channel();
        let statuses = Arc::new(DashMap::new());
        let mut peers = network_config.peers.clone();
        peers.sort_by_cached_key(|p| p.label.clone());
        for peer in &peers {
            statuses.insert(
                Origin::Peer(peer.id.clone()),
                HealthStatus::Unhealthy("Not yet connected".into()),
            );
        }
        let sink = HealthSink(Some(sink));
        (
            Self {
                source,
                state: HealthState {
                    id: network_config.id.clone(),
                    frost,
                    leader_source: Arc::new(leader_source),
                    peers,
                    peer_versions: Arc::new(DashMap::new()),
                    peer_dkg_statuses: Arc::new(DashMap::new()),
                    statuses,
                    raft_client: Arc::new(raft_client),
                },
            },
            sink,
        )
    }

    pub async fn run(self, port: u16) {
        let mut set = JoinSet::new();

        let mut source = self.source;
        let peer_versions = self.state.peer_versions.clone();
        let peer_dkg_statuses = self.state.peer_dkg_statuses.clone();
        let statuses = self.state.statuses.clone();
        set.spawn(async move {
            while let Some(info) = source.recv().await {
                match info {
                    HealthMessage::Status { origin, status } => {
                        statuses.insert(origin, status);
                    }
                    HealthMessage::PeerVersion { peer, version } => {
                        peer_versions.insert(peer, version);
                    }
                    HealthMessage::DkgStatus { peer, status } => {
                        peer_dkg_statuses.insert(peer, status);
                    }
                };
            }
        });

        let app = Router::new()
            .route("/health", get(report_health))
            .route("/force-election", post(force_election))
            .with_state(self.state);
        set.spawn(async move {
            info!("Health server starting on port {}", port);
            let listener = match TcpListener::bind(("0.0.0.0", port)).await {
                Ok(l) => l,
                Err(error) => {
                    warn!("Could not start health server: {}", error);
                    return;
                }
            };
            if let Err(error) = axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            {
                warn!("Health server stopped: {}", error);
            }
        });

        while let Some(res) = set.join_next().await {
            if let Err(error) = res {
                warn!("{:?}", error);
            }
        }
    }
}

#[derive(Serialize)]
enum PeerConnectionStatus {
    Connected,
    WaitingForIncomingConnection,
    TryingToConnect,
}

#[derive(Serialize)]
struct PeerStatus {
    pub name: String,
    pub id: NodeId,
    pub address: String,
    pub version: Option<String>,
    pub connection: PeerConnectionStatus,
}

#[derive(Clone, Serialize)]
struct FrostStatus {
    key: String,
    hash: String,
}

#[derive(Serialize)]
struct ServerHealth {
    pub id: NodeId,
    pub healthy: bool,
    pub raft_leader: Option<String>,
    pub frost: Option<FrostStatus>,
    pub peers: Vec<PeerStatus>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub dkg: BTreeMap<String, String>,
    pub errors: BTreeMap<String, String>,
}

async fn report_health(State(state): State<HealthState>) -> impl IntoResponse {
    let mut errors = BTreeMap::new();
    for entry in state.statuses.iter() {
        let (origin, status) = entry.pair();
        if let HealthStatus::Unhealthy(reason) = status {
            let label = match origin {
                Origin::Source(name) => name.clone(),
                Origin::SyntheticConfig(name) => format!("{name} config"),
                Origin::Currency(name) => name.clone(),
                Origin::Peer(id) => state.peer(id).label.clone(),
            };
            errors.insert(label, reason.clone());
        }
    }

    let mut peers = vec![];
    let mut dkg = BTreeMap::new();

    for peer in &state.peers {
        let connected = state
            .statuses
            .get(&Origin::Peer(peer.id.clone()))
            .is_some_and(|s| matches!(s.value(), HealthStatus::Healthy));
        let status = if connected {
            PeerConnectionStatus::Connected
        } else if state.id.should_initiate_connection_to(&peer.id) {
            PeerConnectionStatus::TryingToConnect
        } else {
            PeerConnectionStatus::WaitingForIncomingConnection
        };
        let version = state.peer_versions.get(&peer.id).map(|v| v.clone());
        peers.push(PeerStatus {
            name: peer.label.clone(),
            id: peer.id.clone(),
            address: peer.address.clone(),
            version,
            connection: status,
        });

        if connected && let Some(status) = state.peer_dkg_statuses.get(&peer.id) {
            dkg.insert(peer.label.clone(), status.clone());
        }
    }
    if let Some(status) = state.peer_dkg_statuses.get(&state.id) {
        dkg.insert("me".to_string(), status.clone());
    }

    let raft_leader = match state.leader_source.borrow().clone() {
        RaftLeader::Myself => Some("me".into()),
        RaftLeader::Other(peer_id) => Some(state.peer(&peer_id).label.to_string()),
        RaftLeader::Unknown => None,
    };

    let health = ServerHealth {
        id: state.id.clone(),
        healthy: errors.is_empty(),
        raft_leader,
        frost: state.frost.clone(),
        peers,
        dkg,
        errors,
    };
    let status = if health.healthy {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(health))
}

#[derive(Deserialize)]
struct RunElectionRequest {
    pub term: usize,
}

async fn force_election(
    State(state): State<HealthState>,
    ConnectInfo(remote_addr): ConnectInfo<SocketAddr>,
    Json(req): Json<RunElectionRequest>,
) -> impl IntoResponse {
    if !remote_addr.ip().is_loopback() {
        warn!(
            remote_addr = %remote_addr,
            "Rejected remote force-election request from non-loopback address"
        );
        return StatusCode::FORBIDDEN;
    }

    state.raft_client.assume_leadership(req.term);
    StatusCode::ACCEPTED
}
