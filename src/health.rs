use std::{collections::HashMap, sync::Arc};

use dashmap::DashMap;
use serde::Serialize;
use tide::Response;
use tokio::{sync::mpsc, task::JoinSet};
use tracing::{info, warn, Instrument};

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Origin {
    Source(String),
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
type HealthMap = Arc<DashMap<Origin, HealthStatus>>;

#[derive(Clone)]
pub struct HealthSink(mpsc::UnboundedSender<HealthInfo>);
impl HealthSink {
    pub fn update(&self, origin: Origin, status: HealthStatus) {
        let result = self.0.send(HealthInfo {
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
    statuses: HealthMap,
}
impl HealthServer {
    pub fn new() -> (Self, HealthSink) {
        let (sink, source) = mpsc::unbounded_channel();
        let sink = HealthSink(sink);
        (
            Self {
                source,
                statuses: Arc::new(DashMap::new()),
            },
            sink,
        )
    }

    pub async fn run(self, port: u16) {
        let mut set = JoinSet::new();

        let mut source = self.source;
        let statuses = self.statuses.clone();
        set.spawn(
            async move {
                while let Some(info) = source.recv().await {
                    statuses.insert(info.origin, info.status);
                }
            }
            .in_current_span(),
        );

        let mut app = tide::with_state(self.statuses);
        app.at("/health").get(report_health);
        set.spawn(
            async move {
                info!("Health server starting on port {}", port);
                if let Err(error) = app.listen(("127.0.0.1", port)).await {
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
struct ServerHealth {
    pub healthy: bool,
    pub errors: HashMap<String, String>,
}

async fn report_health(req: tide::Request<HealthMap>) -> tide::Result {
    let mut errors = HashMap::new();
    for entry in req.state().iter() {
        let (origin, status) = entry.pair();
        if let HealthStatus::Unhealthy(reason) = status {
            errors.insert(format!("{:?}", origin), reason.clone());
        }
    }

    let health = ServerHealth {
        healthy: errors.is_empty(),
        errors,
    };
    let status = if health.healthy { 200 } else { 503 };
    let response = Response::builder(status)
        .content_type("application/json")
        .body(serde_json::to_string_pretty(&health)?)
        .build();
    Ok(response)
}
