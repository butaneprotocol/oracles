mod logic;
#[cfg(test)]
mod tests;

use tokio::{sync::watch, time::Instant};
use tracing::{debug, info};

use crate::network::{Network, NetworkChannel, NodeId};

use logic::RaftState;
pub use logic::{RaftLeader, RaftMessage};

pub struct Raft {
    pub id: NodeId,

    channel: NetworkChannel<RaftMessage>,
    state: RaftState,
}

impl Raft {
    pub fn new(
        quorum: usize,
        heartbeat_freq: std::time::Duration,
        timeout_freq: std::time::Duration,
        network: &mut Network,
        leader_sink: watch::Sender<RaftLeader>,
    ) -> Self {
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
        Raft { id, channel, state }
    }

    pub async fn handle_messages(self) {
        let (sender, mut receiver) = self.channel.split();
        let mut state = self.state;
        loop {
            let next_message = receiver.try_recv().await;
            let timestamp = Instant::now();

            let responses = match next_message {
                Some(msg) => {
                    let span = msg.span.clone();
                    let _x = span.enter();
                    debug!("Received message: {:?}", msg);
                    state.receive(timestamp, msg)
                }
                None => state.tick(timestamp),
            };

            // Send out any responses
            for (peer, response) in responses {
                sender.send(peer, response).await;
            }
            // Yield back to the scheduler, so that other tasks can run
            tokio::task::yield_now().await;
        }
    }
}
