//! `Placement`: where work runs, by the resources it needs and the labels it asks for. A placement
//! never over-commits a node; one that can't be made is refused with why, in words (what's short,
//! and where there's the most of it); releasing frees exactly what was placed. Hex is the real one
//! (CP-T7.1, with priorities, quotas and fair share); [`SimPlacement`] places on the node with the
//! most memory free, then by name, so a run is the same every time.

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::future::BoxFuture;
use parking_lot::Mutex;

use crate::conformance::{Case, Failure, ensure, fine, same};

/// Resources, as asked for or as a node has them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Resources {
    /// Thousandths of a CPU.
    pub cpu_milli: u64,
    pub memory_bytes: u64,
}

impl Resources {
    fn fits_in(self, free: Self) -> bool {
        self.cpu_milli <= free.cpu_milli && self.memory_bytes <= free.memory_bytes
    }
    fn plus(self, o: Self) -> Self {
        Self { cpu_milli: self.cpu_milli.saturating_add(o.cpu_milli), memory_bytes: self.memory_bytes.saturating_add(o.memory_bytes) }
    }
    fn minus(self, o: Self) -> Self {
        Self { cpu_milli: self.cpu_milli.saturating_sub(o.cpu_milli), memory_bytes: self.memory_bytes.saturating_sub(o.memory_bytes) }
    }
}

/// A node work can go on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub name: String,
    pub capacity: Resources,
    pub labels: BTreeMap<String, String>,
}

/// Work to place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    pub id: String,
    pub needs: Resources,
    /// Labels the node must have.
    pub selector: BTreeMap<String, String>,
}

/// Where work was placed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placed {
    pub id: String,
    pub node: String,
}

/// A node and what's placed on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeUse {
    pub name: String,
    pub capacity: Resources,
    pub used: Resources,
}

/// Why a request was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    /// No node can take it now: `why` says what's short.
    #[error("{id} can't be placed: {why}")]
    Unplaceable { id: String, why: String },
    #[error("{what} isn't valid: {why}")]
    Invalid { what: &'static str, why: String },
    #[error("placement holds its most ({limit})")]
    Full { limit: usize },
    #[error("placement is unavailable: {0}")]
    Unavailable(String),
}

/// Where work runs.
pub trait Placement: Send + Sync + 'static {
    /// Places `ask` on a node with its resources free and its labels; the same id again is where
    /// it already is.
    fn place(&self, ask: Ask) -> BoxFuture<'_, Result<Placed, Refusal>>;
    /// Frees what `id` was placed with; an id not placed is released already.
    fn release<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), Refusal>>;
    /// Every node and its use.
    fn nodes(&self) -> BoxFuture<'_, Result<Vec<NodeUse>, Refusal>>;
}

fn gib(bytes: u64) -> String {
    let tenths = bytes.saturating_mul(10).checked_div(1 << 30).unwrap_or(0);
    format!("{}.{} GiB", tenths.checked_div(10).unwrap_or(0), tenths.checked_rem(10).unwrap_or(0))
}

fn cpus(milli: u64) -> String {
    format!("{}.{:03} CPUs", milli.checked_div(1000).unwrap_or(0), milli.checked_rem(1000).unwrap_or(0))
}

// ---------------------------------------------------------------------------------- simulator

/// Most placements a [`SimPlacement`] holds.
pub const SIM_PLACED: usize = 1_000_000;

/// Nodes in memory. Clones share them.
#[derive(Clone)]
pub struct SimPlacement {
    inner: Arc<Mutex<SimState>>,
}

struct SimState {
    nodes: Vec<Node>,
    placed: BTreeMap<String, (String, Resources)>,
}

impl SimPlacement {
    pub fn new(nodes: Vec<Node>) -> Self {
        Self { inner: Arc::new(Mutex::new(SimState { nodes, placed: BTreeMap::new() })) }
    }
}

impl SimState {
    fn used(&self, node: &str) -> Resources {
        self.placed.values().filter(|(n, _)| n == node).fold(Resources::default(), |acc, (_, r)| acc.plus(*r))
    }
}

impl Placement for SimPlacement {
    fn place(&self, ask: Ask) -> BoxFuture<'_, Result<Placed, Refusal>> {
        let out = (|| {
            if ask.id.is_empty() || ask.id.len() > 256 {
                return Err(Refusal::Invalid { what: "an ask's id", why: format!("1 to 256 bytes ({})", ask.id.len()) });
            }
            let mut st = self.inner.lock();
            if let Some((node, _)) = st.placed.get(&ask.id) {
                return Ok(Placed { id: ask.id.clone(), node: node.clone() });
            }
            if st.placed.len() >= SIM_PLACED {
                return Err(Refusal::Full { limit: SIM_PLACED });
            }
            let labelled: Vec<&Node> = st.nodes.iter().filter(|n| ask.selector.iter().all(|(k, v)| n.labels.get(k) == Some(v))).collect();
            if labelled.is_empty() {
                let wanted: Vec<String> = ask.selector.iter().map(|(k, v)| format!("{k}={v}")).collect();
                return Err(Refusal::Unplaceable { id: ask.id.clone(), why: format!("no node has the labels {}", wanted.join(", ")) });
            }
            let mut best: Option<(&Node, Resources)> = None;
            let mut most: Option<(&Node, Resources)> = None;
            for n in labelled {
                let free = n.capacity.minus(st.used(&n.name));
                if most.is_none_or(|(_, m)| free.memory_bytes > m.memory_bytes) {
                    most = Some((n, free));
                }
                if ask.needs.fits_in(free) && best.is_none_or(|(_, b)| free.memory_bytes > b.memory_bytes) {
                    best = Some((n, free));
                }
            }
            let Some((node, _)) = best else {
                let why = match most {
                    Some((n, free)) if ask.needs.memory_bytes > free.memory_bytes => format!("no node has {} of memory free (the most is {} on {})", gib(ask.needs.memory_bytes), gib(free.memory_bytes), n.name),
                    Some((n, free)) => format!("no node has {} free with {} of memory (the most memory is on {}, with {})", cpus(ask.needs.cpu_milli), gib(ask.needs.memory_bytes), n.name, cpus(free.cpu_milli)),
                    None => "there are no nodes".into(),
                };
                return Err(Refusal::Unplaceable { id: ask.id.clone(), why });
            };
            let name = node.name.clone();
            st.placed.insert(ask.id.clone(), (name.clone(), ask.needs));
            Ok(Placed { id: ask.id, node: name })
        })();
        Box::pin(async move { out })
    }

    fn release<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), Refusal>> {
        self.inner.lock().placed.remove(id);
        Box::pin(async { Ok(()) })
    }

    fn nodes(&self) -> BoxFuture<'_, Result<Vec<NodeUse>, Refusal>> {
        let st = self.inner.lock();
        let out = st.nodes.iter().map(|n| NodeUse { name: n.name.clone(), capacity: n.capacity, used: st.used(&n.name) }).collect();
        Box::pin(async move { Ok(out) })
    }
}

// -------------------------------------------------------------------------------- conformance

/// What a `Placement` conformance suite runs against: a fresh cluster per case with two nodes,
/// `conformance-a` (4 CPUs, 8 GiB, `disk=ssd`) and `conformance-b` (8 CPUs, 16 GiB, `gpu=yes`),
/// with nothing placed.
pub trait PlacementFixture: Send + Sync {
    fn placement(&self) -> &dyn Placement;
}

/// The two nodes every case runs on.
pub fn conformance_nodes() -> Vec<Node> {
    let node = |name: &str, cpu: u64, gib: u64, label: (&str, &str)| Node {
        name: name.to_owned(),
        capacity: Resources { cpu_milli: cpu.saturating_mul(1000), memory_bytes: gib.saturating_mul(1 << 30) },
        labels: BTreeMap::from([(label.0.to_owned(), label.1.to_owned())]),
    };
    vec![node("conformance-a", 4, 8, ("disk", "ssd")), node("conformance-b", 8, 16, ("gpu", "yes"))]
}

/// The fixture for a [`SimPlacement`].
pub struct SimPlacementFixture(pub SimPlacement);

impl Default for SimPlacementFixture {
    fn default() -> Self {
        Self(SimPlacement::new(conformance_nodes()))
    }
}

impl PlacementFixture for SimPlacementFixture {
    fn placement(&self) -> &dyn Placement {
        &self.0
    }
}

const GIB: u64 = 1 << 30;

fn ask(id: &str, cpu_milli: u64, memory_bytes: u64) -> Ask {
    Ask { id: id.to_owned(), needs: Resources { cpu_milli, memory_bytes }, selector: BTreeMap::new() }
}

async fn use_of(p: &dyn Placement, node: &str) -> Result<Resources, Failure> {
    let nodes = fine("nodes", p.nodes().await)?;
    nodes.into_iter().find(|n| n.name == node).map(|n| n.used).ok_or_else(|| format!("no node {node}"))
}

fn c1<F: PlacementFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let p = f.placement();
        let placed = fine("place", p.place(ask("w1", 1000, 2 * GIB)).await)?;
        let used = use_of(p, &placed.node).await?;
        same("its node's use", used, Resources { cpu_milli: 1000, memory_bytes: 2 * GIB })
    })
}

fn c2<F: PlacementFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let p = f.placement();
        fine("fill b", p.place(ask("big", 8000, 16 * GIB)).await)?;
        fine("fill a", p.place(ask("other", 4000, 8 * GIB)).await)?;
        match p.place(ask("one-more", 1, 1)).await {
            Err(Refusal::Unplaceable { why, .. }) => ensure(!why.is_empty(), || "refused without saying why".into())?,
            other => return Err(format!("a full cluster took more: {other:?}")),
        }
        for n in fine("nodes", p.nodes().await)? {
            ensure(n.used.fits_in(n.capacity), || format!("{} is over-committed: {:?} of {:?}", n.name, n.used, n.capacity))?;
        }
        Ok(())
    })
}

fn c3<F: PlacementFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let p = f.placement();
        let placed = fine("place", p.place(ask("w1", 3000, 6 * GIB)).await)?;
        fine("place", p.place(ask("w2", 1000, GIB)).await)?;
        let before = use_of(p, &placed.node).await?;
        fine("release", p.release("w1").await)?;
        same("its node's use once released", use_of(p, &placed.node).await?, before.minus(Resources { cpu_milli: 3000, memory_bytes: 6 * GIB }))?;
        fine("releasing again", p.release("w1").await)?;
        fine("releasing what was never placed", p.release("never-placed").await)
    })
}

fn c4<F: PlacementFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let p = f.placement();
        let first = fine("place", p.place(ask("same", 1000, GIB)).await)?;
        let again = fine("place again", p.place(ask("same", 1000, GIB)).await)?;
        same("the same id placed again", again, first.clone())?;
        same("its node's use", use_of(p, &first.node).await?, Resources { cpu_milli: 1000, memory_bytes: GIB })
    })
}

fn c5<F: PlacementFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let p = f.placement();
        let mut gpu = ask("needs-gpu", 1000, GIB);
        gpu.selector.insert("gpu".into(), "yes".into());
        same("where work that needs a gpu went", fine("place", p.place(gpu).await)?.node, "conformance-b".to_owned())?;
        let mut ssd = ask("needs-ssd", 1000, GIB);
        ssd.selector.insert("disk".into(), "ssd".into());
        same("where work that needs an ssd went", fine("place", p.place(ssd).await)?.node, "conformance-a".to_owned())?;
        let mut none = ask("needs-tpu", 1000, GIB);
        none.selector.insert("tpu".into(), "yes".into());
        ensure(matches!(p.place(none).await, Err(Refusal::Unplaceable { .. })), || "work that needs a label no node has was placed".into())
    })
}

fn c6<F: PlacementFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let p = f.placement();
        match p.place(ask("huge", 1000, 64 * GIB)).await {
            Err(Refusal::Unplaceable { why, .. }) => ensure(why.contains("memory"), || format!("why doesn't say memory is short: {why}")),
            other => Err(format!("64 GiB on 8 and 16 GiB nodes: {other:?}")),
        }
    })
}

fn c7<F: PlacementFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let p = f.placement();
        let mut placed = 0u32;
        for i in 0..1000 {
            if p.place(ask(&format!("small-{i}"), 100, GIB / 8)).await.is_ok() {
                placed = placed.saturating_add(1);
            }
        }
        // 12 CPUs and 24 GiB: 120 at 0.1 CPU each, the CPUs run out first.
        same("small asks placed", placed, 120)?;
        fine("release one", p.release("small-0").await)?;
        fine("place once there's room", p.place(ask("after", 100, GIB / 8)).await).map(|_| ())
    })
}

/// The `Placement` contract, clause by clause (docs/seams.md, "Placement").
pub fn cases<F: PlacementFixture>() -> Vec<Case<F>> {
    vec![
        Case { id: "PLACEMENT-1", says: "work placed is on a node whose use grows by what it needs", check: c1::<F> },
        Case { id: "PLACEMENT-2", says: "no node is ever over-committed: work that doesn't fit is refused Unplaceable, saying why", check: c2::<F> },
        Case { id: "PLACEMENT-3", says: "releasing frees exactly what was placed; releasing what isn't placed is fine", check: c3::<F> },
        Case { id: "PLACEMENT-4", says: "the same id placed again is where it is, and uses nothing more", check: c4::<F> },
        Case { id: "PLACEMENT-5", says: "work goes only on nodes with the labels it asks for; none with them, it's refused", check: c5::<F> },
        Case { id: "PLACEMENT-6", says: "a refusal's why names what's short", check: c6::<F> },
        Case { id: "PLACEMENT-7", says: "small work fills the cluster exactly to its capacity, and is placed again once some is released", check: c7::<F> },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::run;

    #[test]
    fn the_simulated_placement_passes_the_placement_suite() -> Result<(), String> {
        futures::executor::block_on(run("Placement", &cases::<SimPlacementFixture>(), || async { SimPlacementFixture::default() })).into_result().map(|_| ())
    }

    #[test]
    fn it_places_on_the_node_with_the_most_memory_free_and_says_why_it_cant() {
        let p = SimPlacement::new(conformance_nodes());
        let placed = futures::executor::block_on(p.place(ask("w", 1000, GIB)));
        assert_eq!(placed.map(|x| x.node), Ok("conformance-b".to_owned()));
        let why = futures::executor::block_on(p.place(ask("huge", 1000, 64 * GIB)));
        assert_eq!(why, Err(Refusal::Unplaceable { id: "huge".into(), why: "no node has 64.0 GiB of memory free (the most is 15.0 GiB on conformance-b)".into() }));
    }
}
