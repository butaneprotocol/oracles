use std::{sync::Arc, time::Duration};

use clap::Parser;
use networking::Network;
use price_aggregator::PriceAggregator;
use raft::Raft;
use tokio::{
    sync::mpsc,
    task::{JoinError, JoinSet},
    time::sleep,
};
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;

pub mod apis;
pub mod frost;
pub mod networking;
pub mod price_aggregator;
pub mod raft;
pub mod token;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[clap(short, long)]
    id: Option<String>,

    #[clap(short, long)]
    port: u16,
    // A list of peers to connect to
    #[clap(long)]
    peers: Vec<String>,
}

#[derive(Clone)]
struct Node {
    id: String,
    network: Network,
    raft: Raft,
}

impl Node {
    pub fn new(
        id: &str,
        quorum: usize,
        heartbeat: std::time::Duration,
        timeout: std::time::Duration,
    ) -> Self {
        // Construct mpsc channels for each of our abstract state machines
        let (raft_tx, raft_rx) = mpsc::channel(10);

        // Construct a peer-to-peer network that can connect to peers, and dispatch messages to the correct state machine
        let network = Network::new(id.to_string(), Arc::new(raft_tx));

        Node {
            id: id.to_string(),
            network: network.clone(),
            raft: Raft::new(id, quorum, heartbeat, timeout, raft_rx, network),
        }
    }

    pub async fn start(self, port: u16, peers: Vec<String>) -> Result<Self, JoinError> {
        let mut set = JoinSet::new();

        // Spawn the abstract raft state machine, which internally uses network to maintain a Raft consensus
        let raft = self.raft.clone();
        set.spawn(async move {
            raft.handle_messages().await;
        });

        // Then spawn the peer to peer network, which will maintain a connection to each peer, and dispatch messages to the correct state machine
        let network = self.network.clone();
        set.spawn(async move {
            network.handle_network(port, peers).await.unwrap();
        });

        let aggregator = PriceAggregator::new();
        set.spawn(async move {
            aggregator.run().await;
        });

        // Then wait for all of them to complete (they won't)
        while let Some(res) = set.join_next().await {
            res?
        }

        Ok(self)
    }

    pub async fn start_debug(
        self,
        port: u16,
        peers: Vec<String>,
        restart_after: Duration,
    ) -> Result<Self, JoinError> {
        loop {
            let mut set = JoinSet::new();

            let me = self.clone();
            let peers = peers.clone();
            set.spawn(async move {
                // runs forever
                me.start(port, peers).await.unwrap();
            });

            set.spawn_blocking(|| {
                // runs until we get input
                let mut line = String::new();
                std::io::stdin().read_line(&mut line).unwrap();
            });

            set.join_next().await.unwrap()?;
            set.abort_all();
            info!(me = self.id, "Node shutting down for {:?}", restart_after);
            sleep(restart_after).await;
            info!(me = self.id, "Node restarting...");
        }
    }
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    let debug = false;

    let subscriber = FmtSubscriber::builder()
        .with_max_level(if debug { Level::DEBUG } else { Level::INFO })
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");

    frost::frost_poc().await;

    // Let us specify an ID on the command line, for easier debugging, otherwise generate a UUID
    let id = args.id.unwrap_or(uuid::Uuid::new_v4().to_string());

    info!(me = id, "Node starting...");

    let timeout = std::time::Duration::from_millis(if debug { 5000 } else { 500 });

    // Construct a new node; quorum is set to a majority of expected nodes (which includes ourself!)
    let num_nodes = args.peers.len() + 1;
    let node = Node::new(
        &id,
        (num_nodes / 2) + 1,
        std::time::Duration::from_millis(100 * if debug { 10 } else { 1 }),
        timeout,
    );

    // Start the node, which will spawn a bunch of threads and infinite loop
    if debug {
        let restart_after = timeout + Duration::from_secs(1);
        node.start_debug(args.port, args.peers, restart_after)
            .await
            .unwrap();
    } else {
        node.start(args.port, args.peers).await.unwrap();
    }
}
