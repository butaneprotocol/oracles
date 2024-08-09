use tokio::sync::mpsc;
use tracing::warn;

use super::RaftCommand;

#[derive(Clone)]
pub struct RaftClient(mpsc::Sender<RaftCommand>);

impl RaftClient {
    pub fn new(sender: mpsc::Sender<RaftCommand>) -> Self {
        Self(sender)
    }

    pub fn abdicate(&self) {
        if let Err(error) = self.0.try_send(RaftCommand::Abdicate) {
            warn!("Could not abdicate raft leadership: {}", error);
        };
    }
}
