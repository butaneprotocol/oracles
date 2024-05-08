use std::{str::FromStr, sync::Arc, time::Duration};

use anyhow::Result;
use clap::Parser;
use config::{load_config, LogConfig, OracleConfig};
use health::{HealthServer, HealthSink};
use network::Network;
use price_aggregator::PriceAggregator;
use publisher::Publisher;
use raft::{Raft, RaftLeader};
use signature_aggregator::SignatureAggregator;
use tokio::{
    sync::{mpsc, watch},
    task::{JoinError, JoinSet},
    time::sleep,
};
use tracing::{info, info_span, Instrument, Level};
use tracing_subscriber::FmtSubscriber;

pub mod apis;
pub mod config;
pub mod health;
pub mod network;
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
    #[clap(long, default_value_t = false)]
    debug: bool,
    #[clap(short, long)]
    config_file: Vec<String>,
}

struct Node {
    health_server: HealthServer,
    health_sink: HealthSink,
    network: Network,
    raft: Raft,
    price_aggregator: PriceAggregator,
    signature_aggregator: SignatureAggregator,
    publisher: Publisher,
    config: Arc<OracleConfig>,
}

impl Node {
    pub fn new(id: &str, config: Arc<OracleConfig>) -> Result<Self> {
        // quorum is set to a majority of expected nodes (which includes ourself!)
        let quorum = ((config.peers.len() + 1) / 2) + 1;
        let heartbeat = config.heartbeat();
        let timeout = config.timeout();

        let (health_server, health_sink) = HealthServer::new();

        // Construct an mpsc channel for incoming messages
        let (message_tx, message_rx) = mpsc::channel(10);

        // Construct a peer-to-peer network that can connect to peers, and dispatch messages to the correct state machine
        let old_network = crate::networking::Network::new(id.to_string(), Arc::new(message_tx));
        let mut network = Network::new(&config, old_network, message_rx);

        let (pa_tx, pa_rx) = watch::channel(vec![]);

        let price_aggregator = PriceAggregator::new(pa_tx, config.clone())?;

        let (leader_tx, leader_rx) = watch::channel(RaftLeader::Unknown);

        let (result_tx, result_rx) = mpsc::channel(10);

        let signature_aggregator = if config.consensus {
            SignatureAggregator::consensus(
                id.to_string(),
                &mut network,
                pa_rx,
                leader_rx,
                result_tx,
            )?
        } else {
            SignatureAggregator::single(pa_rx, leader_rx, result_tx)?
        };

        let publisher = Publisher::new(result_rx)?;

        let raft = Raft::new(
            id,
            quorum,
            heartbeat,
            timeout,
            network.raft_channel(),
            leader_tx,
        );

        Ok(Node {
            health_server,
            health_sink,
            network,
            raft,
            price_aggregator,
            signature_aggregator,
            publisher,
            config,
        })
    }

    pub async fn start(mut self) -> Result<(), JoinError> {
        let mut set = JoinSet::new();

        // Start up the health server
        let health_port = self.config.health_port;
        set.spawn(
            async move {
                self.health_server.run(health_port).await;
            }
            .in_current_span(),
        );

        // Spawn the abstract raft state machine, which internally uses network to maintain a Raft consensus
        let raft = self.raft;
        set.spawn(
            async move {
                raft.handle_messages().await;
            }
            .in_current_span(),
        );

        // Then spawn the peer to peer network, which will maintain a connection to each peer, and dispatch messages to the correct state machine
        set.spawn(
            async move {
                self.network.listen().await.unwrap();
            }
            .in_current_span(),
        );

        let health = self.health_sink;
        set.spawn(
            async move {
                self.price_aggregator.run(&health).await;
            }
            .in_current_span(),
        );

        let signature_aggregator = self.signature_aggregator;
        set.spawn(
            async move {
                signature_aggregator.run().await;
            }
            .in_current_span(),
        );

        set.spawn(
            async move {
                self.publisher.run().await;
            }
            .in_current_span(),
        );

        // Then wait for all of them to complete (they won't)
        while let Some(res) = set.join_next().await {
            res?
        }

        Ok(())
    }
}

async fn run<F>(node_factory: F) -> Result<()>
where
    F: Fn() -> Result<Node>,
{
    let node = node_factory()?;
    node.start().in_current_span().await?;
    Ok(())
}

async fn run_debug<F>(node_factory: F, restart_after: Duration) -> Result<()>
where
    F: Fn() -> Result<Node>,
{
    loop {
        let mut set = JoinSet::new();

        let node = node_factory()?;
        set.spawn(async move {
            // Runs forever
            node.start().in_current_span().await.unwrap();
        });

        set.spawn_blocking(|| {
            // runs until we get input
            let mut line = String::new();
            std::io::stdin().read_line(&mut line).unwrap();
        });

        set.join_next().in_current_span().await.unwrap()?;
        set.abort_all();
        info!("Node shutting down for {:?}", restart_after);
        sleep(restart_after).await;
        info!("Node restarting...");
    }
}

fn init_tracing(config: &LogConfig) -> Result<()> {
    let level = Level::from_str(&config.level)?;
    if config.json {
        let subscriber = FmtSubscriber::builder()
            .json()
            .with_max_level(level)
            .finish();
        tracing::subscriber::set_global_default(subscriber)?;
    } else {
        let subscriber = FmtSubscriber::builder()
            .compact()
            .with_max_level(level)
            .finish();
        tracing::subscriber::set_global_default(subscriber)?;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Let us specify an ID on the command line, for easier debugging, otherwise generate a UUID
    let id = args.id.unwrap_or(uuid::Uuid::new_v4().to_string());
    let debug = args.debug;

    let config = Arc::new(load_config(&args.config_file)?);

    init_tracing(&config.logs)?;

    let span = info_span!("oracle", me = id);
    span.in_scope(|| info!("Node starting..."));

    let node_factory = || Node::new(&id, config.clone());

    // Start the node, which will spawn a bunch of threads and infinite loop
    if debug {
        let restart_after = config.timeout() + Duration::from_secs(1);
        run_debug(node_factory, restart_after)
            .instrument(span)
            .await?;
    } else {
        run(node_factory).instrument(span).await?;
    }
    Ok(())
}
