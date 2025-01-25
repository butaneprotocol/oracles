use tokio::sync::mpsc;
use tracing::warn;

use super::RaftCommand;

#[derive(Clone)]
pub struct RaftClient(mpsc::Sender<RaftCommand>);

impl RaftClient {
    pub fn new() -> (Self, mpsc::Receiver<RaftCommand>) {
        let (sender, receiver) = mpsc::channel(1024);
        (Self(sender), receiver)
    }

    pub fn abdicate(&self) {
        if let Err(error) = self.0.try_send(RaftCommand::Abdicate) {
            warn!("Could not abdicate raft leadership: {}", error);
        };
    }

    pub fn assume_leadership(&self, term: usize) {
        if let Err(error) = self.0.try_send(RaftCommand::ForceElection(term)) {
            warn!("Could not force election: {}", error);
        };
    }
}
