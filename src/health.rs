use std::{collections::HashMap, sync::Arc};

use dashmap::DashMap;
use serde::Serialize;
use tide::Response;
use tokio::{sync::mpsc, task::JoinSet};
use tracing::{info, warn, Instrument};

use crate::network::{NetworkConfig, NodeId, Peer};

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Origin {
    Source(String),
    Peer(NodeId),
}

pub struct HealthInfo {
    pub origin: Origin,
    pub status: HealthStatus,
}

pub enum HealthStatus {
    Healthy,
    Unhealthy(String),
}

type HealthSource = mpsc::UnboundedReceiver<HealthInfo>;
#[derive(Clone)]
struct HealthState {
    id: NodeId,
    peers: Vec<Peer>,
    statuses: Arc<DashMap<Origin, HealthStatus>>,
}

#[derive(Clone)]
pub struct HealthSink(Option<mpsc::UnboundedSender<HealthInfo>>);
impl HealthSink {
    pub fn noop() -> Self {
        Self(None)
    }
    pub fn update(&self, origin: Origin, status: HealthStatus) {
        let Some(sink) = self.0.as_ref() else {
            return;
        };
        let result = sink.send(HealthInfo {
            origin: origin.clone(),
            status,
        });
        if let Err(err) = result {
            warn!("Could not update health for {:?}: {}", origin, err);
        }
    }
}

pub struct HealthServer {
    source: HealthSource,
    state: HealthState,
}
impl HealthServer {
    pub fn new(network_config: &NetworkConfig) -> (Self, HealthSink) {
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
                    peers,
                    statuses,
                },
            },
            sink,
        )
    }

    pub async fn run(self, port: u16) {
        let mut set = JoinSet::new();

        let mut source = self.source;
        let statuses = self.state.statuses.clone();
        set.spawn(
            async move {
                while let Some(info) = source.recv().await {
                    statuses.insert(info.origin, info.status);
                }
            }
            .in_current_span(),
        );

        let mut app = tide::with_state(self.state);
        app.at("/health").get(report_health);
        set.spawn(
            async move {
                info!("Health server starting on port {}", port);
                if let Err(error) = app.listen(("0.0.0.0", port)).await {
                    warn!("Health server stopped: {}", error);
                };
            }
            .in_current_span(),
        );

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
    pub connection: PeerConnectionStatus,
}

#[derive(Serialize)]
struct ServerHealth {
    pub id: NodeId,
    pub healthy: bool,
    pub peers: Vec<PeerStatus>,
    pub errors: HashMap<String, String>,
}

async fn report_health(req: tide::Request<HealthState>) -> tide::Result {
    let mut errors = HashMap::new();
    let state = req.state();
    for entry in state.statuses.iter() {
        let (origin, status) = entry.pair();
        if let HealthStatus::Unhealthy(reason) = status {
            let label = match origin {
                Origin::Source(name) => name.to_string(),
                Origin::Peer(id) => state
                    .peers
                    .iter()
                    .find(|n| n.id == *id)
                    .map_or_else(|| format!("{}", id), |p| p.label.clone()),
            };
            errors.insert(label, reason.clone());
        }
    }

    let peers = state
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
            PeerStatus {
                name: p.label.clone(),
                id: p.id.clone(),
                address: p.address.clone(),
                connection: status,
            }
        })
        .collect();

    let health = ServerHealth {
        id: state.id.clone(),
        healthy: errors.is_empty(),
        peers,
        errors,
    };
    let status = if health.healthy { 200 } else { 503 };
    let response = Response::builder(status)
        .content_type("application/json")
        .body(serde_json::to_string_pretty(&health)?)
        .build();
    Ok(response)
}
