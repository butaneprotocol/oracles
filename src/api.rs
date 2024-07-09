use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use tokio::{net::TcpListener, sync::watch, task::JoinSet};
use tracing::{info, warn};

use crate::signature_aggregator::{Payload, PayloadEntry};

#[derive(Clone)]
pub struct APIState {
    payload_source: Arc<watch::Receiver<Payload>>,
}

pub struct APIServer {
    state: APIState,
}
impl APIServer {
    pub fn new(payload_source: watch::Receiver<Payload>) -> Self {
        Self {
            state: APIState {
                payload_source: Arc::new(payload_source),
            },
        }
    }

    pub async fn run(self, port: u16) {
        let mut set = JoinSet::new();

        let app = Router::new()
            .route("/payload", get(report_all_payloads))
            .route("/payload/:synthetic", get(report_payload))
            .with_state(self.state);
        set.spawn(async move {
            info!("API server starting on port {}", port);
            let listener = match TcpListener::bind(("0.0.0.0", port)).await {
                Ok(l) => l,
                Err(error) => {
                    warn!("Could not start API server: {}", error);
                    return;
                }
            };
            if let Err(error) = axum::serve(listener, app).await {
                warn!("API server stopped: {}", error);
            }
        });

        while let Some(res) = set.join_next().await {
            if let Err(error) = res {
                warn!("{:?}", error);
            }
        }
    }
}

async fn report_all_payloads(State(state): State<APIState>) -> impl IntoResponse {
    let payload = state.payload_source.borrow().clone();
    (StatusCode::OK, Json(payload))
}

pub enum Response {
    Ok(PayloadEntry),
    NotFound,
}
impl IntoResponse for Response {
    fn into_response(self) -> axum::response::Response {
        match self {
            Response::Ok(entry) => (StatusCode::OK, Json(entry)).into_response(),
            Response::NotFound => (StatusCode::NOT_FOUND, "Not Found").into_response(),
        }
    }
}

async fn report_payload(
    Path(synthetic): Path<String>,
    State(state): State<APIState>,
) -> impl IntoResponse {
    let payload = state.payload_source.borrow().clone();
    match payload.entries.iter().find(|&p| p.synthetic == synthetic) {
        Some(entry) => Response::Ok(entry.clone()),
        None => Response::NotFound,
    }
}
