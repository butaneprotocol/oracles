use std::time::Duration;

use futures::{Future, StreamExt, stream::FuturesUnordered};
use kupon::{AssetId, Client, HealthStatus, Match};
use tokio::time::sleep;
use tracing::{debug, warn};

pub async fn wait_for_sync(client: &Client) {
    let mut last_status = None;
    loop {
        let health = client.health().await;
        let status_changed = !last_status.is_some_and(|s| s == health.status);
        last_status = Some(health.status.clone());
        match health.status {
            HealthStatus::Healthy => return,
            HealthStatus::Syncing => {
                let status = health
                    .info
                    .map(|i| format!("syncing ({})", i.sync_progress()))
                    .unwrap_or("syncing".into());
                if status_changed {
                    warn!(
                        "Kupo server is {}. This source will be ignored until it is fully synced.",
                        status
                    );
                } else {
                    debug!("Kupo server is still {}.", status);
                }
                sleep(Duration::from_secs(1)).await;
            }
            HealthStatus::Disconnected => {
                if status_changed {
                    warn!(
                        "Kupo server is disconnected from the underlying node. Please check that it is configured correctly. This source will be ignored until kupo is connected and fully synced."
                    )
                } else {
                    debug!("Kupo server is still disconnected from the underlying node.");
                }
                sleep(Duration::from_secs(5)).await;
            }
            HealthStatus::Error(reason) => {
                if status_changed {
                    warn!(
                        "Kupo server is unreachable or otherwise unhealthy: {}. Please ensure the server is running and configured correctly. This source will be ignored until it is.",
                        reason
                    );
                } else {
                    debug!("Kupo server is still unhealthy: {}", reason);
                }
                sleep(Duration::from_secs(10)).await;
            }
        }
    }
}

pub fn get_asset_value(matc: &Match, asset_id: &Option<AssetId>) -> Option<u64> {
    get_asset_value_minus_tx_fee(matc, asset_id, 0)
}

pub fn get_asset_value_minus_tx_fee(
    matc: &Match,
    asset_id: &Option<AssetId>,
    tx_fee: u64,
) -> Option<u64> {
    match asset_id {
        Some(token) => matc.value.assets.get(token).copied(),
        None => matc.value.coins.checked_sub(tx_fee),
    }
}

/**
 * Runs a set of futures, only allowing a certain number to execute at a time.
 */
pub struct MaxConcurrencyFutureSet<F: Future> {
    running: FuturesUnordered<F>,
    queued: Vec<F>,
    concurrency: usize,
}
impl<F: Future> MaxConcurrencyFutureSet<F> {
    pub fn new(concurrency: usize) -> Self {
        assert!(concurrency > 0);
        Self {
            running: FuturesUnordered::new(),
            queued: vec![],
            concurrency,
        }
    }

    pub fn push(&mut self, future: F) {
        self.queued.push(future);
    }

    pub async fn next(&mut self) -> Option<F::Output> {
        while self.running.len() < self.concurrency {
            if let Some(another_task) = self.queued.pop() {
                self.running.push(another_task);
            } else {
                break;
            }
        }
        self.running.next().await
    }
}
