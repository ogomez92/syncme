//! Types shared between the app, the peer protocol and the web UI.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub type NodeId = String;
pub type ReplicaId = String;

pub const APP: &str = "syncme";
pub const PROTOCOL: u32 = 1;
pub const DEFAULT_PORT: u16 = 47474;

/// One copy of a synced folder: a path on one device.
/// A device may hold several replicas of the same folder (e.g. two drives).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Replica {
    pub id: ReplicaId,
    pub node: NodeId,
    pub path: String,
}

/// A synced folder and where it lives on every participating device.
/// Replicated to every involved device; the highest (rev, updated_by) wins.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Share {
    pub id: String,
    pub name: String,
    pub replicas: Vec<Replica>,
    /// Devices that used to be part of this folder and must hear about its removal.
    #[serde(default)]
    pub former_nodes: BTreeSet<NodeId>,
    pub rev: u64,
    pub updated_by: NodeId,
}

impl Share {
    pub fn nodes(&self) -> BTreeSet<NodeId> {
        self.replicas.iter().map(|r| r.node.clone()).collect()
    }

    pub fn involves(&self, node: &str) -> bool {
        self.replicas.iter().any(|r| r.node == node) || self.former_nodes.contains(node)
    }

    pub fn is_newer_than(&self, other: &Share) -> bool {
        (self.rev, &self.updated_by) > (other.rev, &other.updated_by)
    }

    pub fn is_deleted(&self) -> bool {
        self.replicas.is_empty()
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Hello {
    pub app: String,
    pub protocol: u32,
    pub id: NodeId,
    pub name: String,
    pub os: String,
    pub port: u16,
    pub version: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PairRequest {
    pub hello: Hello,
    pub token: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct NotifyMsg {
    pub share_id: String,
    pub replica_id: ReplicaId,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct BrowseResult {
    pub path: String,
    pub parent: Option<String>,
    pub dirs: Vec<BrowseDir>,
    pub home: String,
    pub roots: Vec<BrowseDir>,
    pub sep: String,
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct BrowseDir {
    pub name: String,
    pub path: String,
}

/// Version vector: replica id -> counter.
pub type Vv = BTreeMap<ReplicaId, u64>;
