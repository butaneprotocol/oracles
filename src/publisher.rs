use std::time::Duration;

use anyhow::Result;
use reqwest::Client;
use tokio::sync::watch;
use tracing::{info, trace, warn};

use crate::{network::NodeId, signature_aggregator::Payload};

const URL: &str = "https://infra-integration.silver-train-1la.pages.dev/api/updatePrices";

pub struct Publisher {
    id: NodeId,
    source: watch::Receiver<Payload>,
    client: Client,
}

impl Publisher {
    pub fn new(id: &NodeId, source: watch::Receiver<Payload>) -> Result<Self> {
        Ok(Self {
            id: id.clone(),
            source,
            client: Client::builder().build()?,
        })
    }

    pub async fn run(self) {
        const DEBUG: bool = false;

        let mut source = self.source;
        let client = self.client;
        while source.changed().await.is_ok() {
            let payload = {
                let latest = source.borrow_and_update();
                let new_entries: Vec<_> = latest
                    .entries
                    .iter()
                    .filter(|e| e.timestamp == latest.timestamp)
                    .map(|e| e.entry.clone())
                    .collect();
                let payload = serde_json::to_string(&new_entries).expect("infallible");
                if latest.publisher != self.id {
                    info!(%latest.publisher, payload, "someone else is publishing a payload");
                    continue;
                }
                payload
            };
            info!(payload, "publishing payload");

            if !DEBUG {
                match make_request(&client, payload).await {
                    Ok(res) => trace!("Payload published! {}", res),
                    Err(err) => warn!("Could not publish payload: {}", err),
                }
            }
        }
    }
}

async fn make_request(client: &Client, payload: String) -> Result<String> {
    let response = client
        .post(URL)
        .header("Content-Type", "application/json")
        .body(payload)
        .timeout(Duration::from_secs(5))
        .send()
        .await?;
    Ok(response.text().await?)
}
