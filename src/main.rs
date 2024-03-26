use std::{sync::Arc, time::Duration};

use anyhow::Result;
use clap::Parser;
use config::{load_config, Config};
use health::{HealthServer, HealthSink};
use networking::Network;
use price_aggregator::PriceAggregator;
use publisher::Publisher;
use raft::{Raft, RaftLeader};
use signature_aggregator::SignatureAggregator;
use tokio::{
    sync::{mpsc, watch},
    task::{JoinError, JoinSet},
    time::sleep,
};
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;

pub mod apis;
pub mod config;
pub mod frost;
pub mod health;
pub mod networking;
pub mod price_aggregator;
pub mod price_feed;
pub mod publisher;
pub mod raft;
pub mod signature_aggregator;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[clap(short, long)]
    id: Option<String>,

    #[clap(short, long)]
    port: u16,
    /// A list of peers to connect to
    #[clap(long)]
    peers: Vec<String>,
    #[clap(short, long, default_value_t = false)]
    /// Use consensus algorithm to sign packages
    consensus: bool,
}

struct Node {
    id: String,
    health_server: HealthServer,
    health_sink: HealthSink,
    network: Network,
    raft: Raft,
    price_aggregator: PriceAggregator,
    signature_aggregator: SignatureAggregator,
    publisher: Publisher,
}

impl Node {
    pub fn new(
        id: &str,
        quorum: usize,
        heartbeat: std::time::Duration,
        timeout: std::time::Duration,
        consensus: bool,
        config: &Arc<Config>,
    ) -> Result<Self> {
        let (health_server, health_sink) = HealthServer::new();

        // Construct mpsc channels for each of our abstract state machines
        let (raft_tx, raft_rx) = mpsc::channel(10);
        let (signer_tx, signer_rx) = mpsc::channel(10);

        // Construct a peer-to-peer network that can connect to peers, and dispatch messages to the correct state machine
        let network = Network::new(id.to_string(), Arc::new(raft_tx), Arc::new(signer_tx));

        let (pa_tx, pa_rx) = watch::channel(vec![]);

        let price_aggregator = PriceAggregator::new(pa_tx, config.clone())?;

        let (leader_tx, leader_rx) = watch::channel(RaftLeader::Unknown);

        let (result_tx, result_rx) = mpsc::channel(10);

        let signature_aggregator = if consensus {
            SignatureAggregator::consensus(
                id.to_string(),
                network.clone(),
                signer_rx,
                pa_rx,
                leader_rx,
                result_tx,
            )?
        } else {
            SignatureAggregator::single(pa_rx, leader_rx, result_tx)?
        };

        let publisher = Publisher::new(result_rx)?;

        Ok(Node {
            id: id.to_string(),
            health_server,
            health_sink,
            network: network.clone(),
            raft: Raft::new(id, quorum, heartbeat, timeout, raft_rx, network, leader_tx),
            price_aggregator,
            signature_aggregator,
            publisher,
        })
    }

    pub async fn start(mut self, port: u16, peers: Vec<String>) -> Result<(), JoinError> {
        let mut set = JoinSet::new();

        // Start up the health server
        set.spawn(async move {
            self.health_server.run(port + 10000).await;
        });

        // Spawn the abstract raft state machine, which internally uses network to maintain a Raft consensus
        set.spawn(async move {
            self.raft.handle_messages().await;
        });

        // Then spawn the peer to peer network, which will maintain a connection to each peer, and dispatch messages to the correct state machine
        set.spawn(async move {
            self.network.handle_network(port, peers).await.unwrap();
        });

        let health = self.health_sink;
        set.spawn(async move {
            self.price_aggregator.run(&health).await;
        });

        let signature_aggregator = self.signature_aggregator;
        set.spawn(async move {
            signature_aggregator.run().await;
        });

        set.spawn(async move {
            self.publisher.run().await;
        });

        // Then wait for all of them to complete (they won't)
        while let Some(res) = set.join_next().await {
            res?
        }

        Ok(())
    }
}

async fn run<F>(node_factory: F, port: u16, peers: Vec<String>) -> Result<()>
where
    F: Fn() -> Result<Node>,
{
    let node = node_factory()?;
    node.start(port, peers).await?;
    Ok(())
}

async fn run_debug<F>(
    node_factory: F,
    port: u16,
    peers: Vec<String>,
    restart_after: Duration,
) -> Result<()>
where
    F: Fn() -> Result<Node>,
{
    loop {
        let mut set = JoinSet::new();

        let node = node_factory()?;
        let id = node.id.clone();
        let peers = peers.clone();
        set.spawn(async move {
            // Runs forever
            node.start(port, peers).await.unwrap();
        });

        set.spawn_blocking(|| {
            // runs until we get input
            let mut line = String::new();
            std::io::stdin().read_line(&mut line).unwrap();
        });

        set.join_next().await.unwrap()?;
        set.abort_all();
        info!(me = id, "Node shutting down for {:?}", restart_after);
        sleep(restart_after).await;
        info!(me = id, "Node restarting...");
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let config = Arc::new(load_config().await?);

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
    let node_factory = || {
        Node::new(
            &id,
            (num_nodes / 2) + 1,
            std::time::Duration::from_millis(100 * if debug { 10 } else { 1 }),
            timeout,
            args.consensus,
            &config,
        )
    };

    // Start the node, which will spawn a bunch of threads and infinite loop
    if debug {
        let restart_after = timeout + Duration::from_secs(1);
        run_debug(node_factory, args.port, args.peers, restart_after).await?;
    } else {
        run(node_factory, args.port, args.peers).await.unwrap();
    }
    Ok(())
}
