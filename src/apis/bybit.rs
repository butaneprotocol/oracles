use std::str::FromStr;

use anyhow::{anyhow, Result};
use dashmap::DashMap;
use futures::{FutureExt, SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use tokio::{
    select,
    time::{sleep, Duration},
};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::Instrument;

use crate::config::OracleConfig;

use super::source::{PriceInfo, PriceSink, Source};

const URL: &str = "wss://stream.bybit.com/v5/public/linear";

struct ByBitPriceInfo {
    token: String,
    unit: String,
    last_value: Option<Decimal>,
    last_volume: Option<Decimal>,
}
impl ByBitPriceInfo {
    fn new(token: &str, unit: &str) -> Self {
        Self {
            token: token.to_string(),
            unit: unit.to_string(),
            last_value: None,
            last_volume: None,
        }
    }
}

#[derive(Default)]
pub struct ByBitSource {
    stream_info: DashMap<String, ByBitPriceInfo>,
}

impl Source for ByBitSource {
    fn name(&self) -> String {
        "ByBit".into()
    }

    fn tokens(&self) -> Vec<String> {
        self.stream_info
            .iter()
            .map(|entry| entry.token.clone())
            .collect()
    }

    fn query<'a>(
        &'a self,
        sink: &'a PriceSink,
    ) -> futures::prelude::future::BoxFuture<anyhow::Result<()>> {
        self.query_impl(sink).boxed()
    }
}

impl ByBitSource {
    pub fn new(config: &OracleConfig) -> Self {
        let stream_info = config
            .bybit
            .tokens
            .iter()
            .map(|x| (x.stream.clone(), ByBitPriceInfo::new(&x.token, &x.unit)))
            .collect();
        Self { stream_info }
    }

    async fn query_impl(&self, sink: &PriceSink) -> Result<()> {
        let (mut stream, _) = connect_async(URL).await?;

        stream
            .send(
                ByBitRequest {
                    op: "subscribe".into(),
                    args: self
                        .stream_info
                        .iter()
                        .map(|e| format!("tickers.{}", e.key()))
                        .collect(),
                }
                .try_into()?,
            )
            .await?;

        let (mut ws_sink, mut stream) = stream.split();

        let ping_msg: Message = ByBitRequest {
            op: "ping".into(),
            args: vec![],
        }
        .try_into()?;
        let heartbeat = async move {
            loop {
                sleep(Duration::from_secs(20)).await;
                if let Err(err) = ws_sink.send(ping_msg.clone()).await {
                    return Err(anyhow!("Error sending heartbeat: {}", err));
                }
            }
        }
        .in_current_span();

        let consumer = async move {
            while let Some(result) = stream.next().await {
                match result? {
                    Message::Text(content) => {
                        let response: ByBitResponse = serde_json::from_str(&content)?;
                        let data = match response {
                            ByBitResponse::StatusResponse { success, ret_msg } => {
                                if !success {
                                    return Err(anyhow!("Error subscribing: {}", ret_msg));
                                }
                                continue;
                            }
                            ByBitResponse::TickerResponse { data } => data,
                        };
                        let Some(mut info) = self.stream_info.get_mut(&data.symbol) else {
                            continue;
                        };
                        let Some(value) = data
                            .mark_price
                            .and_then(|x| Decimal::from_str(&x).ok())
                            .map(|p| {
                                // We track inverse solana prices
                                if info.token == "SOLp" {
                                    Decimal::ONE / p
                                } else {
                                    p
                                }
                            })
                            .or(info.last_value)
                        else {
                            continue;
                        };
                        info.last_value = Some(value);

                        let Some(volume) = data
                            .volume_24h
                            .and_then(|x| Decimal::from_str(&x).ok())
                            .or(info.last_volume)
                        else {
                            continue;
                        };
                        info.last_volume = Some(volume);

                        let price_info = PriceInfo {
                            token: info.token.clone(),
                            unit: info.unit.clone(),
                            value,
                            reliability: volume,
                        };
                        sink.send(price_info)?;
                    }
                    other => {
                        return Err(anyhow!("Unexpected response {:?}", other));
                    }
                };
            }
            Ok(())
        }
        .in_current_span();

        select! {
            res = heartbeat => res,
            res = consumer => res,
        }
    }
}

#[derive(Serialize)]
struct ByBitRequest {
    op: String,
    args: Vec<String>,
}

impl TryFrom<ByBitRequest> for Message {
    type Error = anyhow::Error;

    fn try_from(value: ByBitRequest) -> Result<Self> {
        let contents = serde_json::to_string(&value)?;
        Ok(Message::text(contents))
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ByBitResponse {
    StatusResponse { success: bool, ret_msg: String },
    TickerResponse { data: TickerSnapshotData },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TickerSnapshotData {
    symbol: String,
    mark_price: Option<String>,
    volume_24h: Option<String>,
}
