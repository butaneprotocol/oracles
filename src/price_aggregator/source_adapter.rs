use std::{sync::Arc, time::Instant};

use dashmap::DashMap;
use tokio::{
    select,
    sync::mpsc::unbounded_channel,
    time::{sleep, Duration},
};
use tracing::{info_span, instrument, warn, Instrument};

use crate::{
    health::{HealthSink, HealthStatus, Origin},
    sources::source::{PriceInfoAsOf, PriceSink, Source},
};

pub struct SourceAdapter {
    pub name: String,
    max_time_without_updates: Duration,
    source: Box<dyn Source + Send + Sync>,
    prices: Arc<DashMap<String, PriceInfoAsOf>>,
}

impl SourceAdapter {
    pub fn new<T: Source + Send + Sync + 'static>(source: T) -> Self {
        Self {
            name: source.name(),
            max_time_without_updates: source.max_time_without_updates(),
            source: Box::new(source),
            prices: Arc::new(DashMap::new()),
        }
    }

    pub fn get_prices(&self) -> Arc<DashMap<String, PriceInfoAsOf>> {
        self.prices.clone()
    }

    #[instrument(skip_all, fields(source = self.name))]
    pub async fn run(self, health: HealthSink) {
        let (tx, mut rx) = unbounded_channel();

        // track when each token was updated
        let last_updated = Arc::new(DashMap::new());
        for token in self.source.tokens() {
            last_updated.insert(token, None);
        }

        let source = self.source;
        let name = self.name.clone();
        let run_task = async move {
            let sink = PriceSink::new(tx);
            loop {
                let span = info_span!("source_query", source = name);
                if let Err(error) = source.query(&sink).instrument(span.clone()).await {
                    span.in_scope(|| {
                        warn!(
                            "Error occurred while querying {:?}, retrying: {}",
                            name, error
                        );
                    });
                    sleep(Duration::from_secs(1)).await;
                }
            }
        };

        let updated = last_updated.clone();
        let prices = self.prices.clone();
        let update_prices_task = async move {
            // write emitted values into our map
            while let Some(info_as_of) = rx.recv().await {
                let info = &info_as_of.info;
                let as_of = info_as_of.as_of;
                let span = info_span!("update_price", token = info.token, unit = info.unit);
                if info.value.is_zero() {
                    span.in_scope(|| warn!("ignoring reported value of 0"));
                    continue;
                }
                if info.reliability.is_zero() {
                    span.in_scope(|| warn!("ignoring reported value with reliability 0"));
                    continue;
                }
                updated.insert(info.token.clone(), Some(as_of));
                prices.insert(info.token.clone(), info_as_of);
            }
            "Price receiver has closed"
        };

        // Check how long it's been since we updated prices
        // Mark ourself as unhealthy if any prices are too old.
        let name = self.name.clone();
        let max_time_without_updates = self.max_time_without_updates;
        let track_health_task = async move {
            loop {
                sleep(Duration::from_secs(30)).await;
                let now = Instant::now();
                let mut missing_updates = vec![];
                for update_times in last_updated.iter() {
                    let too_long_without_update = update_times
                        .value()
                        .map_or(true, |v| now - v >= max_time_without_updates);
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
                health.update(Origin::Source(name.clone()), status);
            }
        };

        let reason = select! {
            _ = run_task => "Running failed",
            msg = update_prices_task => msg,
            _ = track_health_task => "Health tracking failed",
        };
        warn!(reason, "Source has unexpectedly stopped running");
    }
}
