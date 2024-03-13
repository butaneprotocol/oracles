use rust_decimal::Decimal;
use tokio::sync::mpsc;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Origin {
    Binance,
    Coinbase,
    Maestro,
}

#[derive(Debug)]
pub struct PriceInfo {
    pub origin: Origin,
    pub token: String,
    pub value: Decimal,
    pub relative_to: String,
}

pub type PriceSink = mpsc::UnboundedSender<PriceInfo>;
