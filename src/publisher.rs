use std::time::Duration;

use anyhow::Result;
use reqwest::Client;
use tokio::sync::watch;
use tracing::{info, trace, warn};

use crate::signature_aggregator::SignedPayload;

const URL: &str = "https://infra-integration.silver-train-1la.pages.dev/api/updatePrices";

pub struct Publisher {
    source: watch::Receiver<SignedPayload>,
    client: Client,
}

impl Publisher {
    pub fn new(source: watch::Receiver<SignedPayload>) -> Result<Self> {
        Ok(Self {
            source,
            client: Client::builder().build()?,
        })
    }

    pub async fn run(self) {
        const DEBUG: bool = false;

        let mut source = self.source;
        let client = self.client;
        while source.changed().await.is_ok() {
            let payload =
                serde_json::to_string(&source.borrow_and_update().entries).expect("infallible");
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
