use std::sync::Arc;

use dashmap::DashMap;
use tokio::{
    sync::mpsc::unbounded_channel,
    task::JoinSet,
    time::{sleep, Duration, Instant},
};
use tracing::warn;

use crate::{
    apis::source::{Origin, PriceInfo, Source},
    health::{HealthSink, HealthStatus},
};

pub struct SourceAdapter {
    origin: Origin,
    source: Box<dyn Source + Send + Sync>,
    prices: Arc<DashMap<String, PriceInfo>>,
}

impl SourceAdapter {
    pub fn new<T: Source + Send + Sync + 'static>(source: T) -> Self {
        Self {
            origin: source.origin(),
            source: Box::new(source),
            prices: Arc::new(DashMap::new()),
        }
    }

    pub fn get_prices(&self) -> Arc<DashMap<String, PriceInfo>> {
        self.prices.clone()
    }

    pub async fn run(self, health: HealthSink) {
        let mut set = JoinSet::new();

        let (tx, mut rx) = unbounded_channel();

        // track when each token was updated
        let last_updated = Arc::new(DashMap::new());
        for token in self.source.tokens() {
            last_updated.insert(token, None);
        }

        let source = self.source;
        set.spawn(async move {
            // read values from the source
            loop {
                if let Err(error) = source.query(&tx).await {
                    warn!(
                        "Error occurred while querying {:?}, retrying: {}",
                        self.origin, error
                    );
                    sleep(Duration::from_secs(1)).await;
                }
            }
        });

        let updated = last_updated.clone();
        let prices = self.prices.clone();
        set.spawn(async move {
            // write emitted values into our map
            while let Some(info) = rx.recv().await {
                updated.insert(info.token.clone(), Some(Instant::now()));
                prices.insert(info.token.clone(), info);
            }
        });

        // Check how long it's been since we updated prices
        // Mark ourself as unhealthy if any prices are too old.
        set.spawn(async move {
            loop {
                sleep(Duration::from_secs(30)).await;
                let now = Instant::now();
                let mut missing_updates = vec![];
                for update_times in last_updated.iter() {
                    let too_long_without_update = update_times
                        .value()
                        .map_or(true, |v| now - v >= Duration::from_secs(30));
                    if too_long_without_update {
                        missing_updates.push(update_times.key().clone());
                    }
                }

                let status = if missing_updates.is_empty() {
                    HealthStatus::Healthy
                } else {
                    HealthStatus::Unhealthy(format!(
                        "Went more than 30 seconds without updates for {:?}",
                        missing_updates
                    ))
                };
                health.update(crate::health::Origin::Source(self.origin), status);
            }
        });

        while let Some(res) = set.join_next().await {
            if let Err(error) = res {
                warn!("{}", error);
            }
        }
    }
}
