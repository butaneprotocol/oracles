use std::sync::Arc;

use clap::Parser;
use networking::Network;
use raft::Raft;
use tokio::{
    sync::mpsc,
    task::{JoinError, JoinSet},
};
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;

pub mod networking;
pub mod raft;

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

struct Node {
    network: Network,
    raft: Raft,
}

impl Node {
    pub fn new(
        id: &String,
        quorum: usize,
        heartbeat: std::time::Duration,
        timeout: std::time::Duration,
    ) -> Self {
        // Construct mpsc channels for each of our abstract state machines
        let (raft_tx, raft_rx) = mpsc::channel(10);

        // Construct a peer-to-peer network that can connect to peers, and dispatch messages to the correct state machine
        let network = Network::new(id.clone(), Arc::new(raft_tx));

        Node {
            network: network.clone(),
            raft: Raft::new(&id, quorum, heartbeat, timeout, raft_rx, network),
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

        // Then wait for all of them to complete (they won't)
        while let Some(res) = set.join_next().await {
            if let Err(e) = res {
                return Err(e);
            }
        }

        Ok(self)
    }
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    let debug = false;

    let subscriber = FmtSubscriber::builder()
        .with_max_level(if debug { Level::TRACE } else { Level::INFO })
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");

    // Let us specify an ID on the command line, for easier debugging, otherwise generate a UUID
    let id = args.id.unwrap_or(uuid::Uuid::new_v4().to_string());

    info!(me = id, "Node starting...");

    // Construct a new node; quorum is set to a majority of expected nodes (which includes ourself!)
    let num_nodes = args.peers.len() + 1;
    let node = Node::new(
        &id,
        (num_nodes / 2) + 1,
        std::time::Duration::from_millis(100 * if debug { 10 } else { 1 }),
        std::time::Duration::from_millis((500 * if debug { 10 } else { 1 }) as u64),
    );

    // Start the node, which will spawn a bunch of threads and infinite loop
    node.start(args.port, args.peers).await.unwrap();
}
