mod client;
mod logic;
#[cfg(test)]
mod tests;

use tokio::{
    select,
    sync::{mpsc, watch},
    time::{sleep_until, Instant},
};
use tracing::{debug, info, info_span};

use crate::{
    config::OracleConfig,
    network::{Network, NetworkChannel, NodeId},
    price_feed::PriceData,
};

pub use client::RaftClient;
use logic::RaftState;
pub use logic::{RaftLeader, RaftMessage};

#[derive(Debug)]
pub enum RaftCommand {
    /// If we are currently leader, step down.
    /// Stop sending heartbeats, so that another node triggers election and wins.
    Abdicate,
    /// Trigger a new election for the given term.
    ForceElection(usize),
}

pub struct Raft {
    pub id: NodeId,
    expected_payloads: usize,
    price_source: watch::Receiver<PriceData>,
    command_source: mpsc::Receiver<RaftCommand>,
    channel: NetworkChannel<RaftMessage>,
    state: RaftState,
}

impl Raft {
    pub fn new(
        config: &OracleConfig,
        network: &mut Network,
        leader_sink: watch::Sender<RaftLeader>,
        price_source: watch::Receiver<PriceData>,
        command_source: mpsc::Receiver<RaftCommand>,
    ) -> Self {
        // quorum is set to a majority of expected nodes (which includes ourself!)
        let quorum = ((config.network.peers.len() + 1) / 2) + 1;
        let heartbeat_freq = config.heartbeat;
        let timeout_freq = config.timeout;
        let expected_payloads = config.synthetics.len();

        info!(
            quorum = quorum,
            heartbeat = format!("{:?}", heartbeat_freq),
            timeout = format!("{:?}", timeout_freq),
            "New raft protocol"
        );
        let id = network.id.clone();
        let channel = network.raft_channel();
        let state = RaftState::new(
            id.clone(),
            Instant::now(),
            quorum,
            heartbeat_freq,
            timeout_freq,
            leader_sink,
        );
        Self {
            id,
            expected_payloads,
            price_source,
            command_source,
            channel,
            state,
        }
    }

    pub async fn handle_messages(self) {
        let (sender, mut receiver) = self.channel.split();
        let mut price_source = self.price_source;
        let mut command_source = self.command_source;
        let mut state = self.state;
        loop {
            let next_event = state.next_event(Instant::now());
            let responses = select! {
                Some(msg) = receiver.recv() => {
                    let span = msg.span.clone();
                    span.in_scope(|| {
                        debug!("Received message: {:?}", msg);
                        state.receive(Instant::now(), msg)
                    })
                },
                Some(command) = command_source.recv() => {
                    let span = info_span!("raft_command", ?command);
                    span.in_scope(|| {
                        match command {
                            RaftCommand::Abdicate => state.abdicate(),
                            RaftCommand::ForceElection(term) => state.run_election(term, Instant::now()),
                        }
                    })
                }
                Ok(()) = price_source.changed() => {
                    let present_payloads = price_source.borrow().synthetics.len();
                    let missing_payloads = self.expected_payloads - present_payloads;
                    state.set_missing_payloads(missing_payloads)
                }
                _ = sleep_until(next_event) => {
                    let span = info_span!("raft_tick");
                    span.in_scope(|| {
                        state.tick(next_event)
                    })
                },
            };
            // Send out any responses
            for (peer, response) in responses {
                sender.send(peer, response).await;
            }
        }
    }
}
