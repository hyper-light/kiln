//! Warm microVM pools kiln asks a runner's node to keep (kiln plan 01a1dd2e313f72f69c6d93061143cc2e, P3.2): kiln names
//! the pools, from the starts of the agents people start there; the node sizes each by its own demand rule within its
//! room (Shards' D26 and D253, through Hex's P3.1). Pure: starts and the time come in, what to ask the node goes out.
//!
//! - **A pool begins** with a start of a key no kept pool has: kiln asks the node to keep it, once, and holds its
//!   answer ([`Held`]).
//! - **Each later start** of its key is its latest; one whose microVM started on a warm host is a hit.
//! - **It's let go** once no start of its key came for a keep window ([`Pools::due`]): kiln asks the node to let it go,
//!   unless the node refused it. A start after its window begins a new pool, its counts from nothing.
//! - **The keep window** is the caller's: the node's own keep-alive (Shards' `demand::KEEP`, D26's), so kiln names a
//!   pool for as long as its node keeps an idle pool's hosts, and no longer.
//!
//! The same rule serves the runner, which asks its node, and the server, which shows each runner's pools from the
//! starts it keeps: given the same starts and the same time, both hold the same pools. What it holds is bounded by the
//! starts it's told of: one pool for each key started within a keep window, each forgotten as its window passes.

use std::collections::BTreeMap;

/// The node's answer to kiln's ask to keep a pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Held {
    /// Asked; no answer yet.
    Asked,
    /// The node keeps it.
    Kept,
    /// The node refused it, in its words: not asked again until a new pool of its key begins.
    Refused(String),
}

/// A pool kiln names on a runner's node: when it began and its latest start (Unix ms), how many starts it had and how
/// many of them were hits, and the node's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pool {
    pub began_ms: i64,
    pub last_ms: i64,
    pub starts: u64,
    pub hits: u64,
    pub held: Held,
}

impl Pool {
    fn new(at_ms: i64, warm: bool, held: Held) -> Self {
        Pool {
            began_ms: at_ms,
            last_ms: at_ms,
            starts: 1,
            hits: u64::from(warm),
            held,
        }
    }

    /// When its keep window ends (Unix ms): `keep_ms` past its latest start. It's let go after it.
    pub fn until_ms(&self, keep_ms: i64) -> i64 {
        self.last_ms.saturating_add(keep_ms)
    }
}

/// The pools kiln names on one runner's node, by key, each kept `keep_ms` past its latest start.
#[derive(Debug, Clone)]
pub struct Pools<K> {
    keep_ms: i64,
    kept: BTreeMap<K, Pool>,
}

impl<K: Ord + Clone> Pools<K> {
    /// None named yet, each to be kept `keep_ms` past its latest start (none below 0).
    pub fn new(keep_ms: i64) -> Self {
        Pools {
            keep_ms: keep_ms.max(0),
            kept: BTreeMap::new(),
        }
    }

    /// Its keep window, in ms.
    pub fn keep_ms(&self) -> i64 {
        self.keep_ms
    }

    /// A start of `key` at `at_ms`, `warm` where its microVM started on a warm host. True where kiln asks the node to
    /// keep its pool: none was named, or its last was refused and its window had passed, so a new one begins. A start
    /// after its pool's window that its due hadn't let go of yet begins a new pool too, the node's answer kept.
    pub fn started(&mut self, key: K, at_ms: i64, warm: bool) -> bool {
        let keep_ms = self.keep_ms;
        let Some(pool) = self.kept.get_mut(&key) else {
            self.kept.insert(key, Pool::new(at_ms, warm, Held::Asked));
            return true;
        };
        if at_ms.saturating_sub(pool.last_ms) <= keep_ms {
            pool.last_ms = pool.last_ms.max(at_ms);
            pool.starts = pool.starts.saturating_add(1);
            pool.hits = pool.hits.saturating_add(u64::from(warm));
            return false;
        }
        let ask = matches!(pool.held, Held::Refused(_));
        let held = if ask { Held::Asked } else { pool.held.clone() };
        *pool = Pool::new(at_ms, warm, held);
        ask
    }

    /// The node's answer to kiln's ask to keep `key`'s pool; for a pool no longer named, nothing.
    pub fn answered(&mut self, key: &K, held: Held) {
        if let Some(pool) = self.kept.get_mut(key) {
            pool.held = held;
        }
    }

    /// The pools whose keep window had passed by `now_ms`, forgotten: those the node is asked to let go of, none it
    /// refused.
    pub fn due(&mut self, now_ms: i64) -> Vec<K> {
        let keep_ms = self.keep_ms;
        let mut gone = Vec::new();
        self.kept.retain(|key, pool| {
            let kept = now_ms.saturating_sub(pool.last_ms) <= keep_ms;
            if !kept && !matches!(pool.held, Held::Refused(_)) {
                gone.push(key.clone());
            }
            kept
        });
        gone
    }

    /// When the next pool's keep window has passed (Unix ms): when [`Pools::due`] lets one go; none while none is named.
    pub fn next_due(&self) -> Option<i64> {
        self.kept
            .values()
            .map(|p| p.until_ms(self.keep_ms).saturating_add(1))
            .min()
    }

    /// The pools named now, by key.
    pub fn kept(&self) -> impl Iterator<Item = (&K, &Pool)> {
        self.kept.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::{Held, Pool, Pools};

    /// Shards' D26 keep-alive, in ms (`shards_api::demand::KEEP`), as the runner passes it.
    const KEEP: i64 = 600_000;

    /// P3.2: a first start of a key asks its pool kept, once; later starts within its window are its own, each warm
    /// one a hit.
    #[test]
    fn p3_2_a_first_start_asks_its_pool_kept_and_later_ones_count_its_hits() {
        let mut pools = Pools::new(KEEP);
        assert!(
            pools.started("a", 1_000, false),
            "a first start asks its pool kept"
        );
        assert!(
            !pools.started("a", 2_000, true),
            "a later start asks nothing"
        );
        assert!(!pools.started("a", 3_000, true));
        let kept: Vec<_> = pools.kept().collect();
        assert_eq!(
            kept,
            vec![(
                &"a",
                &Pool {
                    began_ms: 1_000,
                    last_ms: 3_000,
                    starts: 3,
                    hits: 2,
                    held: Held::Asked
                }
            )]
        );
    }

    /// P3.2: no start of a key for its keep window lets its pool go, once: kept through the window's last ms, let go
    /// past it, and asked again by a start after.
    #[test]
    fn p3_2_a_pool_without_starts_for_its_keep_window_is_let_go_once() {
        let mut pools = Pools::new(KEEP);
        pools.started("a", 0, false);
        pools.answered(&"a", Held::Kept);
        assert_eq!(pools.next_due(), Some(KEEP + 1));
        assert!(pools.due(KEEP).is_empty(), "kept through its window");
        assert_eq!(pools.due(KEEP + 1), vec!["a"]);
        assert!(pools.due(KEEP + 2).is_empty(), "let go once");
        assert_eq!(pools.kept().count(), 0, "the pool is gone");
        assert_eq!(pools.next_due(), None);
        assert!(
            pools.started("a", KEEP + 5, false),
            "a start after it asks a new pool kept"
        );
    }

    /// P3.2: a pool the node refused is never asked let go; a start after its window asks it kept again, one within
    /// doesn't.
    #[test]
    fn p3_2_a_refused_pool_is_not_let_go_and_is_asked_again_only_after_its_window() {
        let mut pools = Pools::new(KEEP);
        pools.started("a", 0, false);
        pools.answered(
            &"a",
            Held::Refused("a warm pool kept for its launches".into()),
        );
        assert!(
            !pools.started("a", 10, false),
            "within its window: not asked again"
        );
        assert!(
            pools.due(KEEP + 11).is_empty(),
            "a refused pool is forgotten, not let go"
        );
        assert!(pools.started("a", KEEP + 20, false));
        pools.answered(&"a", Held::Refused("no".into()));
        assert!(
            pools.started("a", 2 * KEEP + 30, false),
            "a start after its window, not yet due, asks again"
        );
        assert_eq!(
            pools
                .kept()
                .map(|(_, p)| p.held.clone())
                .collect::<Vec<_>>(),
            vec![Held::Asked]
        );
    }

    /// P3.2: a start after its pool's window that no due let go of yet begins a new pool, counted from nothing, the
    /// node's answer kept and nothing asked.
    #[test]
    fn p3_2_a_start_past_its_window_before_its_due_begins_a_new_pool() {
        let mut pools = Pools::new(KEEP);
        pools.started("a", 0, false);
        pools.answered(&"a", Held::Kept);
        pools.started("a", 100, true);
        assert!(!pools.started("a", KEEP + 200, true));
        let kept: Vec<_> = pools.kept().collect();
        assert_eq!(
            kept,
            vec![(
                &"a",
                &Pool {
                    began_ms: KEEP + 200,
                    last_ms: KEEP + 200,
                    starts: 1,
                    hits: 1,
                    held: Held::Kept
                }
            )]
        );
    }

    /// P3.2, and P7.1's variants: each key is a pool of its own, its starts and hits its own, let go on its own window;
    /// the next due is the earliest window's.
    #[test]
    fn p3_2_each_key_is_its_own_pool_counted_and_let_go_alone() {
        let mut pools = Pools::new(KEEP);
        assert!(pools.started(("agent", "variant a"), 0, false));
        assert!(pools.started(("agent", "variant b"), 50, false));
        pools.started(("agent", "variant a"), 60, true);
        pools.started(("agent", "variant b"), 70, false);
        pools.started(("agent", "variant b"), 80, true);
        let counts: Vec<_> = pools.kept().map(|(k, p)| (k.1, p.starts, p.hits)).collect();
        assert_eq!(counts, vec![("variant a", 2, 1), ("variant b", 3, 1)]);
        assert_eq!(pools.next_due(), Some(60 + KEEP + 1));
        assert_eq!(pools.due(60 + KEEP + 1), vec![("agent", "variant a")]);
        assert_eq!(
            pools.kept().map(|(k, _)| k.1).collect::<Vec<_>>(),
            vec!["variant b"]
        );
    }

    /// P3.2: a start out of order (a server reading starts kept on another clock) is within its pool's window, and
    /// moves its latest start back by nothing.
    #[test]
    fn p3_2_an_earlier_start_counts_without_moving_its_pools_latest() {
        let mut pools = Pools::new(KEEP);
        pools.started("a", 5_000, false);
        assert!(!pools.started("a", 4_000, true));
        assert_eq!(
            pools
                .kept()
                .map(|(_, p)| (p.last_ms, p.starts, p.hits))
                .collect::<Vec<_>>(),
            vec![(5_000, 2, 1)]
        );
        assert_eq!(Pools::<&str>::new(-1).keep_ms(), 0, "no window below 0");
    }
}
