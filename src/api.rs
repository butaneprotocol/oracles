use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use serde::Serialize;
use serde_json::json;
use tokio::{net::TcpListener, sync::watch, task::JoinSet};
use tracing::{info, warn};

use crate::{
    config::OracleConfig,
    network::NodeId,
    price_aggregator::TokenPrice,
    signature_aggregator::{GenericPayloadEntry, Payload, SyntheticPayloadEntry},
};

#[derive(Clone, Serialize)]
struct OracleIdentifiers {
    id: NodeId,
    name: String,
}

#[derive(Clone)]
pub struct APIState {
    payload_source: Arc<watch::Receiver<Payload>>,
    prices_source: Arc<watch::Receiver<Vec<TokenPrice>>>,
    oracle: OracleIdentifiers,
}

pub struct APIServer {
    state: APIState,
}
impl APIServer {
    pub fn new(
        config: &OracleConfig,
        payload_source: watch::Receiver<Payload>,
        audit_source: watch::Receiver<Vec<TokenPrice>>,
    ) -> Self {
        Self {
            state: APIState {
                payload_source: Arc::new(payload_source),
                oracle: OracleIdentifiers {
                    id: config.id.clone(),
                    name: config.label.clone(),
                },
                prices_source: Arc::new(audit_source),
            },
        }
    }

    pub async fn run(self, port: u16) {
        let mut set = JoinSet::new();

        let app = Router::new()
            .route("/payload", get(report_all_payloads))
            .route("/payload/{feed}", get(report_payload))
            .route("/prices", get(report_all_prices))
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

#[derive(Serialize)]
struct AllPricesResponse {
    oracle: OracleIdentifiers,
    prices: Vec<TokenPrice>,
}

async fn report_all_prices(State(state): State<APIState>) -> impl IntoResponse {
    let prices = state.prices_source.borrow().clone();
    let response = AllPricesResponse {
        oracle: state.oracle.clone(),
        prices,
    };
    (StatusCode::OK, Json(response))
}

#[allow(clippy::large_enum_variant)]
pub enum Response {
    Synthetic(SyntheticPayloadEntry),
    Generic(GenericPayloadEntry),
    NotFound,
}
impl IntoResponse for Response {
    fn into_response(self) -> axum::response::Response {
        match self {
            Response::Synthetic(entry) => (StatusCode::OK, Json(entry)).into_response(),
            Response::Generic(entry) => (StatusCode::OK, Json(entry)).into_response(),
            Response::NotFound => {
                (StatusCode::NOT_FOUND, Json(json!({ "error": "Not Found" }))).into_response()
            }
        }
    }
}

async fn report_payload(
    Path(feed): Path<String>,
    State(state): State<APIState>,
) -> impl IntoResponse {
    let payload = state.payload_source.borrow().clone();
    if let Some(entry) = payload.synthetics.into_iter().find(|p| p.synthetic == feed) {
        return Response::Synthetic(entry);
    }
    if let Some(entry) = payload.generics.into_iter().find(|p| p.name == feed) {
        return Response::Generic(entry);
    }
    Response::NotFound
}
