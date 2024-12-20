use std::time::{Duration, Instant};

use anyhow::Result;
use futures::future::BoxFuture;
use rust_decimal::Decimal;
use tokio::sync::mpsc;

#[derive(Clone, Debug)]
pub struct PriceInfo {
    pub token: String,
    pub unit: String,
    pub value: Decimal,
    pub reliability: Decimal,
}

#[derive(Clone, Debug)]
pub struct PriceInfoAsOf {
    pub info: PriceInfo,
    pub as_of: Instant,
}

#[derive(Clone)]
pub struct PriceSink(mpsc::UnboundedSender<PriceInfoAsOf>);
impl PriceSink {
    pub fn new(inner: mpsc::UnboundedSender<PriceInfoAsOf>) -> Self {
        Self(inner)
    }

    pub fn send(&self, info: PriceInfo) -> Result<()> {
        let message = PriceInfoAsOf {
            info,
            as_of: Instant::now(),
        };
        Ok(self.0.send(message)?)
    }
}

pub trait Source {
    fn name(&self) -> String;
    fn max_time_without_updates(&self) -> Duration {
        Duration::from_secs(30)
    }
    fn tokens(&self) -> Vec<String>;
    fn query<'a>(&'a self, sink: &'a PriceSink) -> BoxFuture<'a, Result<()>>;
}
