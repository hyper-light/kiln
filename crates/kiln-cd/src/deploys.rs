//! Deploys by what they share (docs/ci.md W28-R38, R39), the pure part: which of the deploys in
//! flight run at once, which wait and for what, and which a newer deploy of its own target
//! supersedes. The server says each deploy's target, what it holds and how far it is, and carries
//! out what this says (`crates/kiln/src/deploy`).
//!
//! - A deploy's target is an app at its destination. Deploys to one target coalesce to the newest
//!   revision, the way rollouts already supersede (docs/cd-targets.md §5): a newer deploy takes the
//!   place of every one before it once it is ready or under way, but never in the middle of a
//!   write, which it waits for so that the newer revision lands on top of the older (R38).
//! - Besides its target, a deploy holds resources: its place (the target's cluster and namespace),
//!   its app, and each config it deploys that another app deploys too. Deploys that share none run
//!   at once; deploys of different targets that share one take turns, oldest first, and the one
//!   that waits names what they share ([`Verdict::why`], R39).
//! - A deploy under way holds its resources whatever its place in the queue, and one ready to go
//!   holds them for those behind it. One its own gates hold (an approval, a soak, a window) holds
//!   nothing: it never holds back another deploy, which goes before it.

use std::collections::BTreeSet;
use std::fmt;

/// What a deploy holds that another deploy may share (W28-R38).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Resource {
    /// Its place: a cluster, and a namespace on it (none for a platform without one).
    Place { cluster: String, namespace: String },
    /// The app it deploys, in any environment.
    App { name: String },
    /// An object it deploys that another app deploys too (`ConfigMap shop/settings`), on its
    /// cluster.
    Config { cluster: String, object: String },
}

impl fmt::Display for Resource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Resource::Place { cluster, namespace } if namespace.is_empty() => write!(f, "both deploy to {cluster}"),
            Resource::Place { cluster, namespace } => write!(f, "both deploy to {cluster}'s {namespace} namespace"),
            Resource::App { name } => write!(f, "both deploy app {name}"),
            Resource::Config { cluster, object } => write!(f, "both deploy {object} on {cluster}"),
        }
    }
}

/// How far a deploy is (W28-R38).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Phase {
    /// Its own gates hold it (an approval, a soak, a window): it holds nothing.
    Held,
    /// Its gates let it go: it waits only for the deploys it shares a resource with.
    Ready,
    /// Its write is under way (a commit to its release repository): never superseded mid-write.
    Writing,
    /// Rolling out.
    Rolling,
}

impl Phase {
    /// Under way: writing or rolling out.
    pub fn going(self) -> bool {
        matches!(self, Phase::Writing | Phase::Rolling)
    }
}

/// A deploy in flight, as the queue decides it (W28-R38).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deploy {
    /// The queue's id for it (`release 7`), one per deploy.
    pub id: String,
    /// Its target: an app at its destination (`web in production`). Deploys of one target
    /// coalesce to the newest.
    pub target: String,
    /// What it holds that another deploy may share.
    pub resources: BTreeSet<Resource>,
    /// How far it is.
    pub phase: Phase,
}

/// A deploy's decision against another deploy in flight (W28-R39).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// They share nothing: it deploys beside the other, in either order.
    Beside { other: String },
    /// Ahead of it, the other shares `shares` but its own gates hold it: it goes first.
    Passes { other: String, shares: Vec<Resource> },
    /// It waits for the other, which holds `shares` too: ahead of it and ready to go, or under way.
    Waits { other: String, shares: Vec<Resource> },
    /// Ahead of it, of its target and not writing: it takes the other's place, the newest revision
    /// of the target rolling out.
    Supersedes { other: String, target: String },
}

impl fmt::Display for Decision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Decision::Beside { other } => write!(f, "deploys beside {other}: nothing shared"),
            Decision::Passes { other, shares } => write!(f, "goes before {other}, which its gates hold: {}", said(shares)),
            Decision::Waits { other, shares } => write!(f, "waits for {other}: {}", said(shares)),
            Decision::Supersedes { other, target } => write!(f, "supersedes {other}: the newest revision of {target} rolls out"),
        }
    }
}

/// What the queue does with a deploy (W28-R39): its decision against each deploy ahead of it, and
/// against each deploy behind it that is under way and shares a resource with it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Verdict {
    /// The deploy decided.
    pub deploy: String,
    /// Against each deploy ahead of it in queue order, then each under way behind it.
    pub decisions: Vec<Decision>,
}

impl Verdict {
    /// Whether it may go now: no deploy has it wait.
    pub fn at_once(&self) -> bool {
        !self.decisions.iter().any(|d| matches!(d, Decision::Waits { .. }))
    }

    /// The deploys it waits for.
    pub fn waits_for(&self) -> Vec<&str> {
        self.decisions.iter().filter_map(|d| if let Decision::Waits { other, .. } = d { Some(other.as_str()) } else { None }).collect()
    }

    /// The deploys of its target ahead of it that it supersedes as it goes.
    pub fn supersedes(&self) -> Vec<&str> {
        self.decisions.iter().filter_map(|d| if let Decision::Supersedes { other, .. } = d { Some(other.as_str()) } else { None }).collect()
    }

    /// Its waits in words, each naming the deploy it waits for and what they share:
    /// `waits for release 5: both deploy to prod's shop-web namespace`.
    pub fn waits(&self) -> Vec<String> {
        self.decisions.iter().filter(|d| matches!(d, Decision::Waits { .. })).map(ToString::to_string).collect()
    }

    /// Its decisions in words: `waits for release 5: both deploy to prod's shop-web namespace`,
    /// `deploys beside release 4: nothing shared`, `supersedes release 3: the newest revision of
    /// web in staging rolls out`; with nothing else in flight, `deploys at once: nothing else is in
    /// flight`.
    pub fn why(&self) -> Vec<String> {
        if self.decisions.is_empty() {
            return vec!["deploys at once: nothing else is in flight".to_owned()];
        }
        self.decisions.iter().map(ToString::to_string).collect()
    }
}

/// The deploys in flight, oldest first, each decided against the others (W28-R38, R39).
#[derive(Debug, Clone, Default)]
pub struct Deploys {
    deploys: Vec<Deploy>,
    /// Each deploy's verdict, in queue order.
    pub verdicts: Vec<Verdict>,
}

impl Deploys {
    /// Decides each of `deploys`, given oldest first.
    pub fn new(deploys: &[Deploy]) -> Deploys {
        let verdicts = deploys
            .iter()
            .enumerate()
            .map(|(j, b)| {
                let ahead = deploys.iter().take(j).map(|a| ahead_of(a, b));
                let behind = deploys.iter().skip(j + 1).filter(|a| a.phase.going() && a.target != b.target).filter_map(|a| {
                    let shares = shared(a, b);
                    (!shares.is_empty()).then(|| Decision::Waits { other: a.id.clone(), shares })
                });
                Verdict { deploy: b.id.clone(), decisions: ahead.chain(behind).collect() }
            })
            .collect();
        Deploys { deploys: deploys.to_vec(), verdicts }
    }

    /// A deploy, by its id.
    pub fn deploy(&self, id: &str) -> Option<&Deploy> {
        self.deploys.iter().find(|d| d.id == id)
    }

    /// A deploy's verdict, by its id.
    pub fn verdict(&self, id: &str) -> Option<&Verdict> {
        self.verdicts.iter().find(|v| v.deploy == id)
    }

    /// The newest deploy of `id`'s target behind it that takes its place (R38), if any: for one
    /// waiting, a newer one its gates let go or under way; for one rolling out, a newer one rolling
    /// out, its write done; for one writing, none — a write is never cut short. The server asks
    /// this first: a deploy superseded goes no further, whatever its verdict.
    pub fn superseded_by(&self, id: &str) -> Option<&Deploy> {
        let at = self.deploys.iter().position(|d| d.id == id)?;
        let me = self.deploys.get(at)?;
        let takes = |d: &Deploy| match me.phase {
            Phase::Writing => false,
            Phase::Rolling => d.phase == Phase::Rolling,
            Phase::Held | Phase::Ready => d.phase != Phase::Held,
        };
        self.deploys.iter().skip(at + 1).rfind(|d| d.target == me.target && takes(d))
    }
}

/// `b`'s decision against `a`, ahead of it: one target writes first and is superseded otherwise;
/// other targets share or don't, and what's shared is held unless `a`'s gates hold it.
fn ahead_of(a: &Deploy, b: &Deploy) -> Decision {
    let shares = shared(a, b);
    if a.target == b.target {
        return if a.phase == Phase::Writing { Decision::Waits { other: a.id.clone(), shares } } else { Decision::Supersedes { other: a.id.clone(), target: b.target.clone() } };
    }
    match (shares.is_empty(), a.phase) {
        (true, _) => Decision::Beside { other: a.id.clone() },
        (false, Phase::Held) => Decision::Passes { other: a.id.clone(), shares },
        (false, _) => Decision::Waits { other: a.id.clone(), shares },
    }
}

/// What two deploys share, in [`Resource`] order.
fn shared(a: &Deploy, b: &Deploy) -> Vec<Resource> {
    a.resources.intersection(&b.resources).cloned().collect()
}

/// Shared resources in words: the first three, and how many more.
fn said(shares: &[Resource]) -> String {
    let mut words: Vec<String> = shares.iter().take(3).map(ToString::to_string).collect();
    if shares.len() > 3 {
        words.push(format!("and {} more", shares.len() - 3));
    }
    words.join("; ")
}

#[cfg(test)]
mod tests;
