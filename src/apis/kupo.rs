use std::time::Duration;

use futures::{stream::FuturesUnordered, Future, StreamExt};
use kupon::{Client, HealthStatus};
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
                    warn!("Kupo server is disconnected from the underlying node. Please check that it is configured correctly. This source will be ignored until kupo is connected and fully synced.")
                } else {
                    debug!("Kupo server is still disconnected from the underlying node.");
                }
                sleep(Duration::from_secs(5)).await;
            }
            HealthStatus::Error(reason) => {
                if status_changed {
                    warn!("Kupo server is unreachable or otherwise unhealthy: {}. Please ensure the server is running and configured correctly. This source will be ignored until it is.", reason);
                } else {
                    debug!("Kupo server is still unhealthy: {}", reason);
                }
                sleep(Duration::from_secs(10)).await;
            }
        }
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
        if self.running.len() < self.concurrency {
            self.running.push(future);
        } else {
            self.queued.push(future);
        }
    }

    pub async fn next(&mut self) -> Option<F::Output> {
        let next = self.running.next().await;
        if next.is_none() {
            if let Some(another_task) = self.queued.pop() {
                self.running.push(another_task);
                return self.running.next().await;
            }
        }
        next
    }
}
