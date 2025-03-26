use std::time::Duration;

use anyhow::Result;
use reqwest::Client;
use serde::Serialize;
use tokio::sync::watch;
use tracing::{info, trace, warn};

use crate::{
    config::OracleConfig,
    network::NodeId,
    price_feed::{cbor_encode_in_list, GenericPriceFeed, Signed, SyntheticPriceFeed},
    signature_aggregator::Payload,
};

pub struct Publisher {
    id: NodeId,
    publish_synthetic_url: Option<String>,
    publish_feed_base_url: Option<String>,
    source: watch::Receiver<Payload>,
    client: Client,
}

impl Publisher {
    pub fn new(config: &OracleConfig, source: watch::Receiver<Payload>) -> Result<Self> {
        Ok(Self {
            id: config.id.clone(),
            publish_synthetic_url: config.publish_url.clone(),
            publish_feed_base_url: config.publish_feed_base_url.clone(),
            source,
            client: Client::builder().build()?,
        })
    }

    pub async fn run(self) {
        let mut source = self.source;
        let client = self.client;
        while source.changed().await.is_ok() {
            let (publisher, butane_payload, feeds) = {
                let latest = source.borrow_and_update();

                let new_entries: Vec<ButaneEntry> = latest
                    .synthetics
                    .iter()
                    .filter(|e| e.timestamp == latest.timestamp)
                    .map(|e| ButaneEntry {
                        synthetic: e.synthetic.clone(),
                        price: e.price,
                        payload: e.payload.clone(),
                    })
                    .collect();
                let butane_payload = serde_json::to_string(&new_entries).expect("infallible");

                let feeds: Vec<FeedEntry> = latest
                    .generics
                    .iter()
                    .filter(|e| e.timestamp == latest.timestamp)
                    .map(|e| FeedEntry {
                        feed: e.name.clone(),
                        payload: e.payload.clone(),
                    })
                    .collect();

                // todo
                (latest.publisher.clone(), butane_payload, feeds)
            };

            if publisher != self.id {
                let feed_payloads = serde_json::to_string(&feeds).expect("infallible");
                info!(%publisher, butane_payload, feed_payloads, "someone else is publishing a payload");
                continue;
            }

            if let Some(url) = &self.publish_synthetic_url {
                info!(butane_payload, "publishing payload");

                match make_request(url, &client, butane_payload).await {
                    Ok(res) => trace!("Payload published! {}", res),
                    Err(err) => warn!("Could not publish payload: {}", err),
                }
            } else {
                info!(butane_payload, "final payload (not publishing)");
            }

            for feed in feeds {
                let name = feed.feed.clone();
                let feed_payload = serde_json::to_string(&feed).expect("infallible");
                if let Some(url) = &self.publish_feed_base_url {
                    let full_url = format!("{url}/{}", urlencoding::encode(&name));
                    info!(feed_payload, name, "publishing feed payload");
                    match make_request(&full_url, &client, feed_payload).await {
                        Ok(res) => trace!(name, "Payload published! {}", res),
                        Err(err) => warn!(name, "Could not publish payload: {}", err),
                    }
                } else {
                    info!(feed_payload, name, "feed payload (not publishing)")
                }
            }
        }
    }
}

#[derive(Serialize)]
struct ButaneEntry {
    pub synthetic: String,
    pub price: f64,
    #[serde(serialize_with = "cbor_encode_in_list")]
    pub payload: Signed<SyntheticPriceFeed>,
}

#[derive(Serialize)]
struct FeedEntry {
    pub feed: String,
    #[serde(serialize_with = "cbor_encode_in_list")]
    pub payload: Signed<GenericPriceFeed>,
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
