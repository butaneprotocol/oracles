use std::{collections::HashMap, time::Duration};

use anyhow::{Context, Result, bail};
use futures::{FutureExt as _, SinkExt, StreamExt, future::BoxFuture};
use rand::{RngCore, thread_rng};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use tokio::{
    net::TcpStream,
    select,
    time::{sleep, timeout},
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

use crate::config::{KucoinTokenConfig, OracleConfig};

use super::source::{PriceInfo, PriceSink, Source};

const REQUEST_TOKEN_URL: &str = "https://api.kucoin.com/api/v1/bullet-public";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);

pub struct KucoinSource {
    client: reqwest::Client,
    symbols: HashMap<String, KucoinTokenConfig>,
}

impl Source for KucoinSource {
    fn name(&self) -> String {
        "Kucoin".into()
    }

    fn tokens(&self) -> Vec<String> {
        self.symbols.values().map(|s| s.token.clone()).collect()
    }

    fn query<'a>(&'a self, sink: &'a PriceSink) -> BoxFuture<'a, Result<()>> {
        self.query_impl(sink).boxed()
    }
}

impl KucoinSource {
    pub fn new(config: &OracleConfig) -> Self {
        let symbols = config
            .kucoin
            .tokens
            .iter()
            .map(|t| (t.symbol.clone(), t.clone()))
            .collect();
        Self {
            client: reqwest::Client::new(),
            symbols,
        }
    }

    async fn query_impl(&self, sink: &PriceSink) -> Result<()> {
        let (mut stream, ping_interval) = self.connect().await?;
        let sub_request = KucoinRequest {
            id: thread_rng().next_u64(),
            type_: "subscribe".into(),
            topic: Some(format!(
                "/market/snapshot:{}",
                self.symbols.keys().cloned().collect::<Vec<_>>().join(",")
            )),
        };
        timeout(CONNECT_TIMEOUT, stream.send(sub_request.try_into()?)).await??;

        let (mut ws_sink, mut stream) = stream.split();
        let heartbeat = async move {
            loop {
                sleep(ping_interval).await;
                let ping_req = KucoinRequest {
                    id: thread_rng().next_u64(),
                    type_: "ping".into(),
                    topic: None,
                };
                ws_sink
                    .send(ping_req.try_into()?)
                    .await
                    .context("Error sending heartbeat")?;
            }
        };

        let consumer = async move {
            while let Ok(Some(result)) = timeout(CONNECT_TIMEOUT, stream.next()).await {
                let message = result.context("Websocket error querying kucoin")?;
                if !message.is_text() {
                    continue;
                }
                let response: KucoinResponse = serde_json::from_str(&message.into_text()?)?;
                let KucoinResponse::Message { data } = response else {
                    continue;
                };
                let Some(symbol) = self.symbols.get(&data.data.symbol) else {
                    bail!("Unrecognized symbol \"{}\"", data.data.symbol);
                };
                sink.send(PriceInfo {
                    token: symbol.token.clone(),
                    unit: symbol.unit.clone(),
                    value: data.data.buy.try_into()?,
                    reliability: Decimal::ONE,
                })?;
            }
            bail!("Kucoin stream has closed")
        };

        select! {
            res = heartbeat => res,
            res = consumer => res,
        }
    }

    async fn connect(&self) -> Result<(WebSocketStream<MaybeTlsStream<TcpStream>>, Duration)> {
        let res = self.client.post(REQUEST_TOKEN_URL).send().await?;
        let parsed: RequestTokenResponse = res.json().await?;
        let Some(server) = parsed
            .data
            .instance_servers
            .iter()
            .find(|s| s.protocol == "websocket")
        else {
            bail!("no servers found");
        };
        let ping_interval = Duration::from_millis(server.ping_interval);
        let (ws, _) = timeout(
            CONNECT_TIMEOUT,
            connect_async(format!("{}?token={}", server.endpoint, parsed.data.token)),
        )
        .await??;
        Ok((ws, ping_interval))
    }
}

#[derive(Serialize)]
struct KucoinRequest {
    id: u64,
    #[serde(rename = "type")]
    type_: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    topic: Option<String>,
}
impl TryFrom<KucoinRequest> for Message {
    type Error = anyhow::Error;

    fn try_from(value: KucoinRequest) -> std::result::Result<Self, Self::Error> {
        Ok(Self::text(serde_json::to_string(&value)?))
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum KucoinResponse {
    Welcome,
    Pong,
    Message { data: KucoinMessage },
}

#[derive(Deserialize)]
struct KucoinMessage {
    data: KucoinResponseData,
}

#[derive(Deserialize)]
struct KucoinResponseData {
    symbol: String,
    buy: f64,
}

#[derive(Deserialize)]
struct RequestTokenResponse {
    data: RequestTokenResponseData,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RequestTokenResponseData {
    token: String,
    instance_servers: Vec<InstanceServer>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InstanceServer {
    endpoint: String,
    protocol: String,
    ping_interval: u64,
}
