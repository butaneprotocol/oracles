use std::time::Duration;

use anyhow::Result;
use reqwest::Client;
use tokio::sync::watch;
use tracing::{info, trace, warn};

use crate::{config::OracleConfig, network::NodeId, signature_aggregator::Payload};

pub struct Publisher {
    id: NodeId,
    url: Option<String>,
    source: watch::Receiver<Payload>,
    client: Client,
}

impl Publisher {
    pub fn new(config: &OracleConfig, source: watch::Receiver<Payload>) -> Result<Self> {
        Ok(Self {
            id: config.id.clone(),
            url: config.publish_url.clone(),
            source,
            client: Client::builder().build()?,
        })
    }

    pub async fn run(self) {
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
            if let Some(url) = &self.url {
                info!(payload, "publishing payload");

                match make_request(url, &client, payload).await {
                    Ok(res) => trace!("Payload published! {}", res),
                    Err(err) => warn!("Could not publish payload: {}", err),
                }
            } else {
                info!(payload, "final payload (not publishing)");
            }
        }
    }
}

async fn make_request(url: &str, client: &Client, payload: String) -> Result<String> {
    let response = client
        .post(url)
        .header("Content-Type", "application/json")
        .body(payload)
        .timeout(Duration::from_secs(5))
        .send()
        .await?;
    Ok(response.text().await?)
}
