use std::{
    fmt::{self, Display},
    ops::Deref,
};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TargetId(String);
impl TargetId {
    pub const fn new(id: String) -> Self {
        Self(id)
    }
}
impl Deref for TargetId {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl Display for TargetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone)]
pub struct OutgoingMessage<T> {
    pub to: Option<TargetId>,
    pub data: T,
}

pub struct IncomingMessage<T> {
    pub from: TargetId,
    pub data: T,
}
