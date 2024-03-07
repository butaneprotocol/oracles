use futures::channel::mpsc;
use rust_decimal::Decimal;

use crate::token::Token;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Origin {
    Binance,
    Maestro,
}

#[derive(Debug)]
pub struct PriceInfo {
    pub origin: Origin,
    pub token: Token,
    pub value: Decimal,
    pub relative_to: Token,
}

pub type PriceSink = mpsc::UnboundedSender<PriceInfo>;
