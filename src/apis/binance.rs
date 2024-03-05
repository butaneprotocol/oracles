use futures::StreamExt;
use serde::Deserialize;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{trace, warn};

// TODO: currencies shouldn't be hard-coded
const URL: &str = "wss://fstream.binance.com/stream?streams=btcusdt@markPrice/adausdt@markPrice";

pub async fn query_binance() {
    loop {
        let (mut stream, _) = connect_async(URL).await.unwrap();
        trace!("Connected to binance!");
        while let Some(res) = stream.next().await {
            match res {
                Ok(Message::Text(contents)) => {
                    process_binance_message(contents);
                    // TODO: emit these to some kind of stream?
                }
                Ok(message) => {
                    warn!("Unexpected response from binance: {:?}", message);
                }
                Err(error) => {
                    warn!("Error from binance: {:?}", error);
                }
            }
        }
        trace!("Connection to binance closed. Reconnecting...")
    }
}

#[derive(Deserialize)]
struct BinanceMarkPriceMessage {
    stream: String,
    data: BinanceMarkPriceMessageData,
}
#[derive(Deserialize)]
struct BinanceMarkPriceMessageData {
    #[serde(rename(deserialize = "p"))]
    price: String,
}
fn process_binance_message(contents: String) {
    let message: BinanceMarkPriceMessage =
        serde_json::from_str(&contents).expect("Malformed response");
    let currency = &message.stream[0..3];
    let price = &message.data.price;
    println!("{} is at {}", currency, price);
}
