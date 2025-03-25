use std::{sync::Arc, time::Duration};

use anyhow::Result;
use clap::Parser;
use oracles::{
    api::APIServer,
    config::{load_config, OracleConfig},
    dkg,
    health::{HealthServer, HealthSink},
    instrumentation,
    network::Network,
    price_aggregator::PriceAggregator,
    price_feed::PriceData,
    publisher::Publisher,
    raft::{Raft, RaftClient, RaftLeader},
    signature_aggregator::SignatureAggregator,
};
use tokio::{
    sync::watch,
    task::{JoinError, JoinSet},
    time::sleep,
};
use tracing::{info, info_span, warn};

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[clap(long, default_value_t = false)]
    debug: bool,
    #[clap(short, long)]
    config_file: Vec<String>,
}

struct Node {
    api_server: APIServer,
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
    pub fn new(config: Arc<OracleConfig>) -> Result<Self> {
        let (leader_sink, leader_source) = watch::channel(RaftLeader::Unknown);
        let (price_sink, price_source) = watch::channel(PriceData {
            synthetics: vec![],
            generics: vec![],
        });
        let (price_audit_sink, price_audit_source) = watch::channel(vec![]);
        let (raft_client, raft_source) = RaftClient::new();

        let (health_server, health_sink) = HealthServer::new(
            config.frost_address.as_ref(),
            &config.network,
            leader_source.clone(),
            raft_client.clone(),
        );

        // Construct a peer-to-peer network that can connect to peers, and dispatch messages to the correct state machine
        let mut network = Network::new(&config.network, health_sink.clone());

        let raft = Raft::new(
            &config,
            &mut network,
            leader_sink,
            price_source.clone(),
            raft_source,
        );

        let (signature_aggregator, payload_source) = if config.consensus {
            SignatureAggregator::consensus(
                &config,
                &mut network,
                raft_client,
                price_source,
                leader_source,
            )?
        } else {
            SignatureAggregator::single(&config, raft_client, price_source, leader_source)?
        };

        let price_aggregator = PriceAggregator::new(
            price_sink,
            price_audit_sink,
            payload_source.clone(),
            config.clone(),
        )?;

        let api_server = APIServer::new(&config, payload_source.clone(), price_audit_source);

        let publisher = Publisher::new(&config, payload_source)?;

        Ok(Node {
            api_server,
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

    pub async fn start(self) -> Result<(), JoinError> {
        let mut set = JoinSet::new();

        // Start up the health server
        let health_port = self.config.health_port;
        set.spawn(async move {
            self.health_server.run(health_port).await;
        });

        let api_port = self.config.api_port;
        set.spawn(async move {
            self.api_server.run(api_port).await;
        });

        // Spawn the abstract raft state machine, which internally uses network to maintain a Raft consensus
        set.spawn(async move {
            self.raft.handle_messages().await;
        });

        // Then spawn the peer to peer network, which will maintain a connection to each peer, and dispatch messages to the correct state machine
        set.spawn(async move {
            self.network.listen().await.unwrap();
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

async fn run<F>(node_factory: F) -> Result<()>
where
    F: Fn() -> Result<Node>,
{
    let node = node_factory()?;
    node.start().await?;
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
            node.start().await.unwrap();
        });

        set.spawn_blocking(|| {
            // runs until we get input
            let mut line = String::new();
            std::io::stdin().read_line(&mut line).unwrap();
        });

        set.join_next().await.unwrap()?;
        set.abort_all();
        info!("Node shutting down for {:?}", restart_after);
        sleep(restart_after).await;
        info!("Node restarting...");
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let debug = args.debug;

    let config = Arc::new(load_config(&args.config_file)?);

    let _guard = instrumentation::init_tracing(&config.logs)?;
    info_span!("init").in_scope(|| {
        info!("Node starting...");
        if config.network.peers.is_empty() {
            warn!("This node has no peers configured.");
        }
    });

    if config.keygen.enabled {
        dkg::run(&config).await?;
        return Ok(());
    }

    let node_factory = || Node::new(config.clone());

    // Start the node, which will spawn a bunch of threads and infinite loop
    if debug {
        let restart_after = config.timeout + Duration::from_secs(1);
        run_debug(node_factory, restart_after).await?;
    } else {
        run(node_factory).await?;
    }
    Ok(())
}
