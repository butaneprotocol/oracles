use std::str::FromStr;

use anyhow::{anyhow, Result};
use futures::{FutureExt, SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use tokio::{
    select,
    time::{sleep, Duration},
};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use super::source::{PriceInfo, PriceSink, Source};

const URL: &str = "wss://stream.bybit.com/v5/public/linear";

#[derive(Default)]
pub struct ByBitSource;

impl Source for ByBitSource {
    fn name(&self) -> String {
        "ByBit".into()
    }

    fn tokens(&self) -> Vec<String> {
        vec!["ADA".into(), "BTCb".into(), "SOLp".into(), "MATICb".into()]
    }

    fn query<'a>(
        &'a self,
        sink: &'a PriceSink,
    ) -> futures::prelude::future::BoxFuture<anyhow::Result<()>> {
        self.query_impl(sink).boxed()
    }
}

impl ByBitSource {
    pub fn new() -> Self {
        Self
    }

    async fn query_impl(&self, sink: &PriceSink) -> Result<()> {
        let (mut stream, _) = connect_async(URL).await?;

        stream
            .send(
                ByBitRequest {
                    op: "subscribe".into(),
                    args: vec![
                        "tickers.ADAUSDT".into(),
                        "tickers.BTCUSDT".into(),
                        "tickers.MATICUSDT".into(),
                        "tickers.SOLUSDT".into(),
                    ],
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
        };

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
                        let Some(mark_price) = data.mark_price else {
                            continue;
                        };

                        let mut value = Decimal::from_str(&mark_price)?;
                        let (token, unit) = match data.symbol.as_str() {
                            "ADAUSDT" => ("ADA", "USD"),
                            "BTCUSDT" => ("BTCb", "USD"),
                            "MATICUSDT" => ("MATICb", "USD"),
                            "SOLUSDT" => {
                                value = Decimal::ONE / value;
                                ("SOLp", "USD")
                            }
                            _ => {
                                return Err(anyhow!(
                                    "Unrecognized price to match: {}",
                                    data.symbol
                                ));
                            }
                        };
                        let price_info = PriceInfo {
                            token: token.into(),
                            unit: unit.into(),
                            value,
                        };
                        sink.send(price_info)?;
                    }
                    other => {
                        return Err(anyhow!("Unexpected response {:?}", other));
                    }
                };
            }
            Ok(())
        };

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
}
