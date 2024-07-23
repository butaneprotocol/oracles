mod logic;
#[cfg(test)]
mod tests;

use tokio::{
    select,
    sync::watch,
    time::{sleep_until, Instant},
};
use tracing::{debug, info, info_span};

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
            let next_event = state.next_event(Instant::now());
            let responses = select! {
                Some(msg) = receiver.recv() => {
                    let span = msg.span.clone();
                    span.in_scope(|| {
                        debug!("Received message: {:?}", msg);
                        state.receive(Instant::now(), msg)
                    })
                },
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
