//! Clusters of persisted nodes in this process, each node's data in a directory of the cluster's
//! own under the test's temporary directory, gone with the cluster.

// Not every test uses every helper; the deadline's thread sleeps (it holds no worker).
#![allow(dead_code, clippy::disallowed_methods)]

use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use quiver::{Node, NodeConfig, PersistedConfig};

pub mod drill;

static CLUSTERS: AtomicU64 = AtomicU64::new(0);

/// A number no other cluster of this process takes.
pub fn next() -> u64 {
    CLUSTERS.fetch_add(1, Ordering::Relaxed)
}

/// A port no socket holds now, on loopback.
pub fn free_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Completes after `wait`, on a thread of its own.
pub fn after(wait: Duration) -> impl Future<Output = ()> {
    let (tell, told) = futures::channel::oneshot::channel::<()>();
    std::thread::spawn(move || {
        std::thread::sleep(wait);
        let _ = tell.send(());
    });
    async move {
        let _ = told.await;
    }
}

pub struct Cluster {
    pub dir: PathBuf,
    pub addresses: Vec<(u64, SocketAddr)>,
    nodes: Vec<Option<Node>>,
}

pub const NODE: NodeConfig = NodeConfig {
    memory: 64 << 20,
    topics: 100,
    subscriptions: 100,
};

impl Cluster {
    /// `n` nodes, each started.
    pub fn start(n: u64) -> Self {
        let id = next();
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("persisted")
            .join(format!("{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let addresses: Vec<(u64, SocketAddr)> = (1..=n)
            .map(|i| (i, SocketAddr::from(([127, 0, 0, 1], free_port()))))
            .collect();
        let mut cluster = Self {
            dir,
            addresses,
            nodes: (0..n).map(|_| None).collect(),
        };
        // Each node answers once its catalog is open; they elect among themselves as they come.
        for i in 1..=n {
            cluster.start_node(i);
        }
        for i in 1..=n {
            cluster.settled(i);
        }
        cluster
    }

    /// Waits until node `id` has caught up with its catalog's group.
    fn settled(&self, id: u64) {
        let began = std::time::Instant::now();
        while !self.node(id).persisted_stats().is_some_and(|s| s.settled) {
            assert!(
                began.elapsed() < Duration::from_secs(60),
                "node {id} never caught up with its catalog"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    pub fn config(&self, id: u64) -> PersistedConfig {
        PersistedConfig {
            id,
            nodes: self.addresses.clone(),
            data: self.dir.join(format!("node-{id}")),
            identities: self.dir.join("identities"),
            part_memory: PersistedConfig::PART_MEMORY_MIN,
            topics: 16,
            publishes: 4096,
            quorum_wait: Duration::from_secs(10),
        }
    }

    pub fn node(&self, id: u64) -> &Node {
        self.nodes[usize::try_from(id - 1).unwrap()]
            .as_ref()
            .expect("the node runs")
    }

    pub fn running(&self, id: u64) -> bool {
        self.nodes[usize::try_from(id - 1).unwrap()].is_some()
    }

    /// Stops node `id` at once, as a crash would.
    pub fn kill(&mut self, id: u64) {
        if let Some(node) = self.nodes[usize::try_from(id - 1).unwrap()].take() {
            node.halt_persisted();
        }
    }

    /// Starts node `id` again from what its directory holds, once it has caught up.
    pub fn restart(&mut self, id: u64) {
        self.start_node(id);
        self.settled(id);
    }

    fn start_node(&mut self, id: u64) {
        let node = Node::with_persisted(NODE, self.config(id)).unwrap();
        self.nodes[usize::try_from(id - 1).unwrap()] = Some(node);
    }
}

impl Drop for Cluster {
    fn drop(&mut self) {
        for slot in &mut self.nodes {
            if let Some(node) = slot.take() {
                node.halt_persisted();
            }
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
