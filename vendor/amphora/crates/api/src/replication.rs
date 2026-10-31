//! The host transports authenticated messages; Amphora owns replication and membership.
use serde::{Deserialize, Serialize};

/// An explicit original database identity, shared by every initial group member.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bootstrap {
    pub source_node: u64,
    pub source_id: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicaConfig {
    pub node_id: u64,
    pub voters: Vec<u64>,
    pub learners: Vec<u64>,
    #[serde(default)]
    pub bootstrap: Option<Bootstrap>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outbound {
    pub to: u64,
    pub bytes: Vec<u8>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicaStatus {
    pub node_id: u64,
    pub leader: u64,
    pub term: u64,
    pub applied: u64,
    pub durable_commit: u64,
    pub voters: Vec<u64>,
    pub learners: Vec<u64>,
    pub fenced: Option<String>,
    /// Initial data is not exposed until its declared image is committed and applied.
    #[serde(default)]
    pub bootstrapping: bool,
}
