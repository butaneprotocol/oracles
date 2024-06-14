use std::fmt::{self, Display};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, PartialOrd, Eq, Ord, Hash, Serialize, Deserialize)]
pub struct NodeId(String);
impl NodeId {
    pub const fn new(id: String) -> Self {
        Self(id)
    }
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}
impl Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug)]
pub struct OutgoingMessage<T> {
    pub to: Option<NodeId>,
    pub data: T,
}

#[derive(Debug)]
pub struct IncomingMessage<T> {
    pub from: NodeId,
    pub data: T,
}
