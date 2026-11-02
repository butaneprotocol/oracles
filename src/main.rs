use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use oracles::{
    api::APIServer,
    config::{OracleConfig, load_config},
    dkg::Keygen,
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
};
use tracing::{error, info, info_span, warn};

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[clap(short, long)]
    config_file: Vec<String>,
}

struct OracleNode {
    api_server: APIServer,
    raft: Raft,
    price_aggregator: PriceAggregator,
    signature_aggregator: SignatureAggregator,
    publisher: Publisher,
}

struct Node {
    health_server: HealthServer,
    health_sink: HealthSink,
    network: Network,
    oracle: Option<OracleNode>,
    keygen: Option<Keygen>,
    config: Arc<OracleConfig>,
}

impl Node {
    pub fn new(config: Arc<OracleConfig>) -> Result<Self> {
        let (leader_sink, leader_source) = watch::channel(RaftLeader::Unknown);
        let (price_sink, price_source) = watch::channel(PriceData {
            synthetics: vec![],
            generics: vec![],
        });
        let (raft_client, raft_source) = RaftClient::new();

        let (health_server, health_sink) = HealthServer::new(
            config.frost_address.as_ref(),
            &config.network,
            leader_source.clone(),
            raft_client.clone(),
        );

        // Construct a peer-to-peer network that can connect to peers, and dispatch messages to the correct state machine
        let mut network = Network::new(&config.network, health_sink.clone());

        let oracle = if config.frost_address.is_some() {
            let (price_audit_sink, price_audit_source) = watch::channel(vec![]);

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

            Some(OracleNode {
                api_server,
                raft,
                price_aggregator,
                signature_aggregator,
                publisher,
            })
        } else {
            None
        };

        let keygen = if config.keygen.enabled {
            Some(Keygen::new(&config, &mut network)?)
        } else {
            None
        };

        Ok(Node {
            health_server,
            health_sink,
            network,
            oracle,
            keygen,
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

        if let Some(oracle) = self.oracle {
            let api_port = self.config.api_port;
            set.spawn(async move {
                oracle.api_server.run(api_port).await;
            });

            // Spawn the abstract raft state machine, which internally uses network to maintain a Raft consensus
            set.spawn(async move {
                oracle.raft.handle_messages().await;
            });

            let health = self.health_sink;
            set.spawn(async move {
                oracle.price_aggregator.run(&health).await;
            });

            let signature_aggregator = oracle.signature_aggregator;
            set.spawn(async move {
                signature_aggregator.run().await;
            });

            set.spawn(async move {
                oracle.publisher.run().await;
            });
        }

        if let Some(keygen) = self.keygen {
            set.spawn(async move {
                match keygen.run().await {
                    Ok(()) => info!("DKG complete"),
                    Err(err) => {
                        error!("DKG failed: {err:#?}")
                    }
                };
            });
        }

        // Then spawn the peer to peer network, which will maintain a connection to each peer, and dispatch messages to the correct state machine
        set.spawn(async move {
            self.network.listen().await.unwrap();
        });

        // Then wait for all of them to complete (they won't)
        while let Some(res) = set.join_next().await {
            res?
        }

        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    if rustls::crypto::CryptoProvider::install_default(rustls::crypto::ring::default_provider())
        .is_err()
    {
        warn!("Could not configure CryptoProvider");
    }
    let args = Args::parse();

    let config = Arc::new(load_config(&args.config_file)?);

    let _guard = instrumentation::init_tracing(&config.logs)?;
    info_span!("init").in_scope(|| {
        info!("Node starting...");
        if config.network.peers.is_empty() {
            warn!("This node has no peers configured.");
        }
    });

    // Start the node, which will spawn a bunch of threads and infinite loop
    let node = Node::new(config.clone())?;
    node.start().await?;
    Ok(())
}
