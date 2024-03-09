use std::sync::Arc;

use tokio::sync::{mpsc::Receiver, Mutex};

// const URL: &str = "https://infra-integration.silver-train-1la.pages.dev/api/updatePrices";

#[derive(Clone)]
pub struct Publisher {
    source: Arc<Mutex<Receiver<String>>>,
}

impl Publisher {
    pub fn new(source: Receiver<String>) -> Self {
        Self {
            source: Arc::new(Mutex::new(source)),
        }
    }

    pub async fn run(&mut self) {
        let mut source = self.source.lock().await;
        while let Some(payload) = source.recv().await {
            println!("{}", payload);
            // TODO: actually publish this to an endpoint
        }
    }
}
