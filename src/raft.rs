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
};

pub use client::RaftClient;
use logic::RaftState;
pub use logic::{RaftLeader, RaftMessage};

pub enum RaftCommand {
    /// If we are currently leader, step down.
    /// Stop sending heartbeats, so that another node triggers election and wins.
    Abdicate,
}

pub struct Raft {
    pub id: NodeId,
    command_source: mpsc::Receiver<RaftCommand>,
    channel: NetworkChannel<RaftMessage>,
    state: RaftState,
}

impl Raft {
    pub fn new(
        config: &OracleConfig,
        network: &mut Network,
        leader_sink: watch::Sender<RaftLeader>,
    ) -> (Self, RaftClient) {
        // quorum is set to a majority of expected nodes (which includes ourself!)
        let quorum = ((config.network.peers.len() + 1) / 2) + 1;
        let heartbeat_freq = config.heartbeat;
        let timeout_freq = config.timeout;

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
        let (command_sink, command_source) = mpsc::channel(10);
        let client = RaftClient::new(command_sink);
        (
            Self {
                id,
                command_source,
                channel,
                state,
            },
            client,
        )
    }

    pub async fn handle_messages(self) {
        let (sender, mut receiver) = self.channel.split();
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
                    let span = info_span!("raft_command");
                    span.in_scope(|| {
                        match command {
                            RaftCommand::Abdicate => state.abdicate()
                        }
                    })
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
