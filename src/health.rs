use std::{collections::HashMap, sync::Arc};

use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::get, Json, Router};
use dashmap::DashMap;
use serde::Serialize;
use tokio::{
    net::TcpListener,
    sync::{mpsc, watch},
    task::JoinSet,
};
use tracing::{info, warn};

use crate::{
    config::{NetworkConfig, Peer},
    network::NodeId,
    raft::RaftLeader,
};

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Origin {
    Source(String),
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
}

type HealthSource = mpsc::UnboundedReceiver<HealthMessage>;
#[derive(Clone)]
struct HealthState {
    id: NodeId,
    peers: Vec<Peer>,
    leader_source: Arc<watch::Receiver<RaftLeader>>,
    peer_versions: Arc<DashMap<NodeId, String>>,
    statuses: Arc<DashMap<Origin, HealthStatus>>,
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
        network_config: &NetworkConfig,
        leader_source: watch::Receiver<RaftLeader>,
    ) -> (Self, HealthSink) {
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
                    leader_source: Arc::new(leader_source),
                    peers,
                    peer_versions: Arc::new(DashMap::new()),
                    statuses,
                },
            },
            sink,
        )
    }

    pub async fn run(self, port: u16) {
        let mut set = JoinSet::new();

        let mut source = self.source;
        let peer_versions = self.state.peer_versions.clone();
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
                };
            }
        });

        let app = Router::new()
            .route("/health", get(report_health))
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
            if let Err(error) = axum::serve(listener, app).await {
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

#[derive(Serialize)]
struct ServerHealth {
    pub id: NodeId,
    pub healthy: bool,
    pub raft_leader: Option<String>,
    pub peers: Vec<PeerStatus>,
    pub errors: HashMap<String, String>,
}

async fn report_health(State(state): State<HealthState>) -> impl IntoResponse {
    let mut errors = HashMap::new();
    for entry in state.statuses.iter() {
        let (origin, status) = entry.pair();
        if let HealthStatus::Unhealthy(reason) = status {
            let label = match origin {
                Origin::Source(name) => name,
                Origin::Peer(id) => &state.peer(id).label,
            };
            errors.insert(label.to_string(), reason.clone());
        }
    }

    let peers: Vec<PeerStatus> = state
        .peers
        .iter()
        .map(|p| {
            let connected = state
                .statuses
                .get(&Origin::Peer(p.id.clone()))
                .is_some_and(|s| matches!(s.value(), HealthStatus::Healthy));
            let status = if connected {
                PeerConnectionStatus::Connected
            } else if state.id.should_initiate_connection_to(&p.id) {
                PeerConnectionStatus::TryingToConnect
            } else {
                PeerConnectionStatus::WaitingForIncomingConnection
            };
            let version = state.peer_versions.get(&p.id).map(|v| v.clone());
            PeerStatus {
                name: p.label.clone(),
                id: p.id.clone(),
                address: p.address.clone(),
                version,
                connection: status,
            }
        })
        .collect();

    let raft_leader = match state.leader_source.borrow().clone() {
        RaftLeader::Myself => Some("me".into()),
        RaftLeader::Other(peer_id) => Some(state.peer(&peer_id).label.to_string()),
        RaftLeader::Unknown => None,
    };

    let health = ServerHealth {
        id: state.id.clone(),
        healthy: errors.is_empty(),
        raft_leader,
        peers,
        errors,
    };
    let status = if health.healthy {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(health))
}
