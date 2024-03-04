use serde::{Deserialize, Serialize};

use crate::raft::RaftMessage;


#[derive(Serialize, Deserialize, Debug)]
pub enum Message {
    Hello(String),
    Raft(RaftMessage),
}
