use std::{sync::Arc, time::Duration};

use anyhow::Result;
use reqwest::Client;
use tokio::sync::{mpsc::Receiver, Mutex};
use tracing::{trace, warn};

const URL: &str = "https://infra-integration.silver-train-1la.pages.dev/api/updatePrices";

#[derive(Clone)]
pub struct Publisher {
    source: Arc<Mutex<Receiver<String>>>,
    client: Arc<Client>,
}

impl Publisher {
    pub fn new(source: Receiver<String>) -> Result<Self> {
        Ok(Self {
            source: Arc::new(Mutex::new(source)),
            client: Arc::new(Client::builder().build()?),
        })
    }

    pub async fn run(&mut self) {
        const DEBUG: bool = false;

        let mut source = self.source.lock().await;
        while let Some(payload) = source.recv().await {
            println!("{}", payload);

            if !DEBUG {
                match self.make_request(payload).await {
                    Ok(res) => trace!("Payload published! {}", res),
                    Err(err) => warn!("Could not publish payload: {}", err),
                }
            }
        }
    }

    async fn make_request(&self, payload: String) -> Result<String> {
        let response = self
            .client
            .post(URL)
            .header("Content-Type", "application/json")
            .body(payload)
            .timeout(Duration::from_secs(5))
            .send()
            .await?;
        Ok(response.text().await?)
    }
}
