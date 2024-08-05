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

pub type PriceSink = mpsc::UnboundedSender<PriceInfo>;

pub trait Source {
    fn name(&self) -> String;
    fn tokens(&self) -> Vec<String>;
    fn query<'a>(&'a self, sink: &'a PriceSink) -> BoxFuture<Result<()>>;
}
