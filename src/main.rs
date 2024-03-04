use std::sync::Arc;

use networking::Network;
use raft::Raft;
use tokio::{sync::mpsc, task::{JoinError, JoinSet}};
use clap::Parser;
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
    peers: Vec<String>
}

struct Node {
    network: Network,
    raft: Raft,
}

impl Node {
    pub fn new(id: &String, quorum: usize, heartbeat: std::time::Duration, timeout: std::time::Duration) -> Self {
        
        let (raft_tx, raft_rx) = mpsc::channel(10);

        let network = Network::new(id.clone(), Arc::new(raft_tx));

        Node {
            network: network.clone(),
            raft: Raft::new(&id, quorum, heartbeat, timeout, raft_rx, network),
        }
    }

    pub async fn start(self, port: u16, peers: Vec<String>) -> Result<Self, JoinError> {
        let mut set = JoinSet::new();

        let raft = self.raft.clone();
        set.spawn(async move {
            raft.handle_messages().await;
        });

        let network = self.network.clone();
        set.spawn(async move {
            network.handle_network(port, peers).await.unwrap();
        });

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
        // all spans/events with a level higher than TRACE (e.g, debug, info, warn, etc.)
        // will be written to stdout.
        .with_max_level(if debug { Level::TRACE } else { Level::INFO })
        // completes the builder.
        .finish();
    tracing::subscriber::set_global_default(subscriber)
        .expect("setting default subscriber failed");

    let id = args.id.unwrap_or(uuid::Uuid::new_v4().to_string());
    info!(me = id, "Node starting...");

    let num_nodes = args.peers.len() + 1;
    let node = Node::new(
        &id,
        (num_nodes / 2) + 1,
        std::time::Duration::from_millis(100 * if debug { 10 } else { 1 }),
        std::time::Duration::from_millis((500 * if debug { 10 } else { 1 }) as u64),
    );

    // Set up the peer-to-peer connections
    node.start(args.port, args.peers).await.unwrap();
}
