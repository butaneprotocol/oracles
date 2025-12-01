use std::time::Duration;

use anyhow::Result;
use futures::{StreamExt, stream::FuturesUnordered};
use reqwest::Client;
use serde::Serialize;
use tokio::{join, sync::watch};
use tracing::{info, trace, warn};

use crate::{
    config::OracleConfig,
    network::NodeId,
    price_feed::{GenericPriceFeed, Signed, SyntheticPriceFeed, cbor_encode_in_list},
    signature_aggregator::Payload,
};

pub struct Publisher {
    id: NodeId,
    publish_synthetic_url: Option<String>,
    publish_feed_base_url: Option<String>,
    source: Option<watch::Receiver<Payload>>,
    client: Client,
}

impl Publisher {
    pub fn new(config: &OracleConfig, source: watch::Receiver<Payload>) -> Result<Self> {
        Ok(Self {
            id: config.id.clone(),
            publish_synthetic_url: config.publish_url.clone(),
            publish_feed_base_url: config.publish_feed_base_url.clone(),
            source: Some(source),
            client: Client::builder().build()?,
        })
    }

    pub async fn run(mut self) {
        let mut source = self.source.take().unwrap();
        while source.changed().await.is_ok() {
            let (publisher, butane_payload, feeds) = {
                let latest = source.borrow_and_update();

                let butane_payload: Vec<ButaneEntry> = latest
                    .synthetics
                    .iter()
                    .filter(|e| e.timestamp == latest.timestamp)
                    .map(|e| ButaneEntry {
                        synthetic: e.synthetic.clone(),
                        price: e.price,
                        payload: e.payload.clone(),
                    })
                    .collect();

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
                let butane_payload = serde_json::to_string(&butane_payload).expect("infallible");
                let feed_payloads = serde_json::to_string(&feeds).expect("infallible");
                info!(%publisher, butane_payload, feed_payloads, "someone else is publishing a payload");
                continue;
            }

            join!(
                self.publish_synthetic_payload(butane_payload),
                self.publish_feeds(feeds)
            );
        }
    }

    async fn publish_synthetic_payload(&self, entries: Vec<ButaneEntry>) {
        let butane_payload = serde_json::to_string(&entries).expect("infallible");
        if let Some(url) = &self.publish_synthetic_url {
            info!(butane_payload, "publishing payload");

            match make_request(url, &self.client, &self.id, butane_payload).await {
                Ok(res) => trace!("Payload published! {}", res),
                Err(err) => warn!("Could not publish payload: {}", err),
            }
        } else {
            info!(butane_payload, "final payload (not publishing)");
        }
    }

    async fn publish_feeds(&self, feeds: Vec<FeedEntry>) {
        let mut publish_tasks = FuturesUnordered::new();
        for feed in feeds {
            let name = feed.feed.clone();
            let feed_payload = serde_json::to_string(&feed).expect("infallible");
            if let Some(url) = &self.publish_feed_base_url {
                let full_url = format!("{url}/{}", urlencoding::encode(&name));
                let client = self.client.clone();
                publish_tasks.push(async move {
                    info!(feed_payload, name, "publishing feed payload");
                    match make_request(&full_url, &client, &self.id, feed_payload).await {
                        Ok(res) => trace!(name, "Payload published! {}", res),
                        Err(err) => warn!(name, "Could not publish payload: {}", err),
                    }
                });
            } else {
                info!(feed_payload, name, "feed payload (not publishing)")
            }
        }
        while let Some(()) = publish_tasks.next().await {}
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

async fn make_request(url: &str, client: &Client, id: &NodeId, payload: String) -> Result<String> {
    let response = client
        .post(url)
        .header("Content-Type", "application/json")
        .header("x-leader-node-id", id.to_string())
        .body(payload)
        .timeout(Duration::from_secs(5))
        .send()
        .await?;
    Ok(response.text().await?)
}
