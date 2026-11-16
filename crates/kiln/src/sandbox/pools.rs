//! Warm pools of a machine's microVMs, kept for the agents people start there (kiln plan
//! 01a1dd2e313f72f69c6d93061143cc2e, P3.2): kiln names them, the machine's node sizes them (Shards' D26 and D253,
//! through Hex's P3.1).
//!
//! - **On the runner** ([`Pools`]): each start of an isolated session's harness in a microVM of its own
//!   (`super::isolated::launch`) is told to [`Pools::started`]. Its pool, its image pinned by its digest and its size
//!   ([`size_of`]), is asked kept of the machine's microVMs (`Sandbox::keep`) as its first start comes; each start whose
//!   microVM started on a warm host is a hit; and it's let go of (`Sandbox::let_go`) once no start of it came for its
//!   keep window ([`KEEP`]), by a waiter armed for the earliest window's end while any pool is named, and none
//!   otherwise. A refusal is held and said once a pool, and asked again only as a new pool of its key begins. Hex's
//!   microVMs keep none until its image class takes Shards' pools (`super::hexvm::NO_POOLS`, P3.1).
//! - **On the server** ([`of_runner`], `GET /api/runners/:name/pools`): the same rule (`kiln_ci::pools`) over the starts
//!   its sessions keep shows each runner's pools, each agent's apart and each of a deployment experiment's variants
//!   apart (P7.1): a variant that boots another image or size is a pool of its own on the node, sized by its own starts.
//!
//! Both read the same starts the same way, so they name the same pools; what the node holds of each (its warm hosts)
//! is the node's, which no runner reports to the server yet.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use http::StatusCode;
use kiln_ci::pools::{Held, Pools as Rule};
use kiln_seams::sandbox::Pool;
use kiln_seams::{Clock, Sandbox};
use parking_lot::Mutex;
use rusqlite::OptionalExtension as _;
use serde_json::{Value, json};

use crate::harness::session::{Info, Isolation, Runs};
use crate::http::server::{self as srv, Response};
use crate::hub::Hub;

/// How long kiln names a pool past its latest start: its node's own keep-alive (Shards' D26 `demand::KEEP`: as AWS
/// keeps an idle function, Shahrad et al., "Serverless in the Wild", USENIX ATC 2020, §1), so kiln names a pool for
/// as long as its node keeps an idle pool's hosts warm, and no longer.
pub const KEEP: Duration = shards_api::demand::KEEP;

/// [`KEEP`] in ms, as `kiln_ci::pools` takes it.
fn keep_ms() -> i64 {
    i64::try_from(KEEP.as_millis()).unwrap_or(i64::MAX)
}

/// A microVM's size, whole vCPUs and MiB each rounded up, as its session's isolation gives it (plan step 32; a
/// deployment experiment's variant, P7.1); none where it gives neither.
pub fn size_of(isolation: Option<&Isolation>) -> Option<(u32, u64)> {
    isolation
        .filter(|i| i.cpus.is_some() || i.memory.is_some())
        .map(|i| {
            let cpus = i.cpus.map_or(0, kiln_ci::sched::state::Cpus::ceil);
            (
                u32::try_from(cpus).unwrap_or(u32::MAX),
                i.memory.map_or(0, |b| b.div_ceil(1 << 20)),
            )
        })
}

/// The pools kiln names on a machine's microVMs, and what it asks of them.
pub struct Pools {
    sandbox: Arc<dyn Sandbox>,
    clock: Arc<dyn Clock>,
    state: Mutex<State>,
}

struct State {
    named: Rule<Pool>,
    /// A waiter is armed, for the earliest window's end.
    waiting: bool,
}

impl Pools {
    /// None named yet on `sandbox`'s microVMs, their windows read on `clock`.
    pub fn new(sandbox: Arc<dyn Sandbox>, clock: Arc<dyn Clock>) -> Arc<Self> {
        Arc::new(Pools {
            sandbox,
            clock,
            state: Mutex::new(State {
                named: Rule::new(keep_ms()),
                waiting: false,
            }),
        })
    }

    /// A start of `pool` here, `warm` where its microVM started on a warm host of it: its pool asked kept as it begins
    /// (a refusal held, and said), and a waiter armed to let it go as its window passes, where none is armed.
    pub async fn started(self: &Arc<Self>, pool: Pool, warm: bool) {
        let now = self.clock.unix_ms();
        let (ask, arm) = {
            let mut st = self.state.lock();
            let ask = st.named.started(pool.clone(), now, warm);
            let arm = !st.waiting;
            st.waiting = true;
            (ask, arm)
        };
        if ask {
            let held = match self.sandbox.keep(pool.clone()).await {
                Ok(()) => Held::Kept,
                Err(why) => {
                    tracing::warn!(image = %pool.image, size = ?pool.size, %why, "a warm pool wasn't kept: its starts start as before");
                    Held::Refused(why.to_string())
                }
            };
            self.state.lock().named.answered(&pool, held);
        }
        if arm {
            let me = Arc::clone(self);
            crate::rt::spawn(async move { me.wait().await });
        }
    }

    /// Lets each pool go as its window passes, asleep until the earliest window's end; ends once none is named.
    async fn wait(self: Arc<Self>) {
        loop {
            let due = {
                let mut st = self.state.lock();
                match st.named.next_due() {
                    Some(due) => due,
                    None => {
                        st.waiting = false;
                        return;
                    }
                }
            };
            let left = u64::try_from(due.saturating_sub(self.clock.unix_ms())).unwrap_or(0);
            self.clock
                .sleep_until(kiln_seams::clock::later(
                    self.clock.now(),
                    Duration::from_millis(left),
                ))
                .await;
            self.expire().await;
        }
    }

    /// Lets go of each pool whose window has passed now (`Sandbox::let_go`), once.
    pub async fn expire(&self) {
        let gone = self.state.lock().named.due(self.clock.unix_ms());
        for pool in gone {
            if let Err(why) = self.sandbox.let_go(pool.clone()).await {
                tracing::warn!(image = %pool.image, size = ?pool.size, %why, "a warm pool wasn't let go");
            }
        }
    }

    /// The pools named here now, each as the rule holds it: its starts, its hits and its node's answer.
    pub fn named(&self) -> Vec<(Pool, kiln_ci::pools::Pool)> {
        self.state
            .lock()
            .named
            .kept()
            .map(|(k, p)| (k.clone(), p.clone()))
            .collect()
    }
}

/// A pool as the server shows it: whose, the agent definition its sessions started from (else their harness), the
/// deployment experiment and variant they ran as (P7.1), and its microVM.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Shown {
    agent: String,
    variant: Option<(String, String)>,
    image: String,
    size: Option<(u32, u64)>,
}

/// Whether a session placed on `target` runs on runner `runner`: by its name, or the server's own runner's sessions,
/// which say `local`.
fn on_runner(hub: &Hub, runner: &str, target: &str) -> bool {
    let machine = crate::harness::wait::machine(target);
    machine == runner || (machine == "local" && crate::harness::limit::own(hub, runner))
}

/// The starts of `info` that began a microVM of their own, each with its microVM's image, when (Unix ms), and whether
/// it started on a warm host: a swap into the microVM its session ran in started none.
fn microvm_starts(info: &Info) -> impl Iterator<Item = (&str, i64, bool)> {
    info.starts.iter().filter_map(|s| {
        let Runs::Microvm { image, .. } = &s.runs else {
            return None;
        };
        let from = s.measured.as_ref().and_then(|m| m.started_from.as_deref());
        (from != Some(super::isolated::SWAPPED)).then_some((
            image.as_str(),
            s.at,
            from == Some(super::hexvm::said(hex_api::StartedFrom::Warm)),
        ))
    })
}

/// The warm pools kiln names on runner `runner` at `now_ms` (kiln plan 01a1dd2e…, P3.2), by the rule its runner asks
/// its node by, over the starts its sessions keep: each pool's agent (its sessions' definition, else their harness), its
/// deployment experiment's variant where they ran as one (P7.1), its microVM (image, vCPUs, MiB), its starts and hits
/// (starts whose microVM started on a warm host of it), when it began, its latest start and when it's let go
/// (`until_ms`); and `warm_hosts`, its node's warm hosts, which no runner reports yet (P3.1). One without starts for its
/// keep window is gone.
pub async fn of_runner(hub: &Hub, runner: &str, now_ms: i64) -> anyhow::Result<Value> {
    let infos: Vec<Arc<Info>> = hub
        .harness
        .list_shared()
        .into_iter()
        .filter(|i| {
            i.isolation.is_some()
                && on_runner(hub, runner, &i.target)
                && microvm_starts(i).next().is_some()
        })
        .collect();
    let ids: Vec<String> = infos.iter().map(|i| i.id.clone()).collect();
    let defs: HashMap<String, String> = hub
        .store
        .read(move |c| {
            let mut st =
                c.prepare("SELECT agent_def_id FROM agent_def_sessions WHERE session_id = ?1")?;
            let mut defs = HashMap::new();
            for id in ids {
                let def = st
                    .query_row(rusqlite::params![id], |r| r.get::<_, String>(0))
                    .optional()?;
                if let Some(def) = def {
                    defs.insert(id, def);
                }
            }
            Ok(defs)
        })
        .await?;
    let mut starts: Vec<(Shown, i64, bool)> = Vec::new();
    for info in &infos {
        let agent = defs
            .get(&info.id)
            .cloned()
            .unwrap_or_else(|| info.kind.clone());
        let variant = info
            .experiment
            .as_ref()
            .map(|e| (e.experiment.clone(), e.arm.clone()));
        let size = size_of(info.isolation.as_ref());
        for (image, at, warm) in microvm_starts(info) {
            starts.push((
                Shown {
                    agent: agent.clone(),
                    variant: variant.clone(),
                    image: image.to_owned(),
                    size,
                },
                at,
                warm,
            ));
        }
    }
    Ok(view(runner, starts, now_ms))
}

/// The pools `starts` name at `now_ms`, in time order, as [`of_runner`] shows them.
fn view(runner: &str, mut starts: Vec<(Shown, i64, bool)>, now_ms: i64) -> Value {
    starts.sort_by_key(|(_, at, _)| *at);
    let mut named = Rule::new(keep_ms());
    for (shown, at, warm) in starts {
        named.started(shown, at, warm);
    }
    named.due(now_ms);
    let keep = named.keep_ms();
    let pools: Vec<Value> = named
        .kept()
        .map(|(k, p)| {
            json!({
                "agent": k.agent,
                "variant": k.variant.as_ref().map(|(experiment, arm)| json!({"experiment": experiment, "arm": arm})),
                "image": k.image,
                "vcpus": k.size.map(|s| s.0),
                "memory_mib": k.size.map(|s| s.1),
                "starts": p.starts,
                "hits": p.hits,
                "began_ms": p.began_ms,
                "last_ms": p.last_ms,
                "until_ms": p.until_ms(keep),
                "warm_hosts": Value::Null,
            })
        })
        .collect();
    json!({"runner": runner, "keep_ms": keep, "pools": pools})
}

/// `GET /api/runners/:name/pools` ([`of_runner`], now): a runner the server doesn't know is `404`.
pub async fn get(hub: &Hub, name: &str) -> Result<Response, (StatusCode, String)> {
    if !crate::harness::limit::known(hub, name).await {
        return Err((StatusCode::NOT_FOUND, format!("no runner named {name}")));
    }
    let view = of_runner(hub, name, crate::util::now_ms())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    Ok(srv::json(StatusCode::OK, &view))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::time::Duration;

    use kiln_ci::pools::Held;
    use kiln_seams::sandbox::{Pool, Sandbox};
    use kiln_seams::{Clock, clock::SimClock};

    use super::super::fake::FakeVms;
    use super::{KEEP, Pools, Shown, keep_ms, view};

    /// A clock whose time moves only as a test sets it, and whose sleeps never end: a test lets pools go itself
    /// ([`Pools::expire`]), its waiter never waking.
    struct Set(AtomicI64);

    impl Clock for Set {
        fn now(&self) -> std::time::Instant {
            std::time::Instant::now()
        }
        fn unix_ms(&self) -> i64 {
            self.0.load(Ordering::SeqCst)
        }
        fn sleep_until(&self, _: std::time::Instant) -> kiln_seams::clock::Sleep {
            Box::pin(futures::future::pending::<()>())
        }
    }

    fn pooling() -> Arc<FakeVms> {
        Arc::new(
            FakeVms::with(
                "harness-claude@sha256:00",
                "claude",
                std::path::Path::new("claude"),
            )
            .pooling(),
        )
    }

    fn pool(image: &str) -> Pool {
        Pool {
            image: image.to_owned(),
            size: Some((2, 2048)),
        }
    }

    /// P3.2: repeated starts of an agent's microVM ask its node to keep its pool once, from those starts; a later
    /// start that reports Warm is a hit; no start for a keep window lets it go, and the pool is gone.
    #[test]
    fn p3_2_repeated_starts_keep_a_pool_once_count_hits_and_let_it_go_after_its_window()
    -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let vms = pooling();
            let clock = Arc::new(Set(AtomicI64::new(1_000_000)));
            let pools = Pools::new(
                Arc::<FakeVms>::clone(&vms) as Arc<dyn Sandbox>,
                Arc::<Set>::clone(&clock) as Arc<dyn Clock>,
            );
            let a = pool("harness-claude@sha256:00");
            pools.started(a.clone(), false).await;
            clock.0.store(1_060_000, Ordering::SeqCst);
            pools.started(a.clone(), true).await;
            assert_eq!(
                vms.pool_asks(),
                vec![("keep", a.clone())],
                "its node is asked to keep it once"
            );
            assert_eq!(vms.pools().len(), 1, "its node keeps it");
            let named = pools.named();
            assert_eq!(named.len(), 1);
            assert_eq!(
                named
                    .first()
                    .map(|(_, p)| (p.starts, p.hits, p.held.clone())),
                Some((2, 1, Held::Kept))
            );
            clock.0.store(1_060_000 + keep_ms(), Ordering::SeqCst);
            pools.expire().await;
            assert_eq!(vms.pool_asks().len(), 1, "kept through its window");
            clock.0.store(1_060_001 + keep_ms(), Ordering::SeqCst);
            pools.expire().await;
            assert_eq!(
                vms.pool_asks(),
                vec![("keep", a.clone()), ("let go", a.clone())],
                "let go once its window passed"
            );
            assert!(
                pools.named().is_empty() && vms.pools().is_empty(),
                "the pool is gone"
            );
            pools.expire().await;
            assert_eq!(vms.pool_asks().len(), 2, "let go once");
            anyhow::Ok(())
        })
    }

    /// P3.2: a node that keeps no pools (Hex's until P3.1) refuses, typed; kiln holds the refusal, asks nothing more of
    /// it while its pool is named, never lets go of what it never kept, and asks again as a new pool begins.
    #[test]
    fn p3_2_a_refused_pool_is_held_and_asked_again_only_by_a_new_pool() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let vms = Arc::new(FakeVms::with(
                "harness-claude@sha256:00",
                "claude",
                std::path::Path::new("claude"),
            ));
            let clock = Arc::new(Set(AtomicI64::new(0)));
            let pools = Pools::new(
                Arc::<FakeVms>::clone(&vms) as Arc<dyn Sandbox>,
                Arc::<Set>::clone(&clock) as Arc<dyn Clock>,
            );
            let a = pool("harness-claude@sha256:00");
            pools.started(a.clone(), false).await;
            let held = pools.named().first().map(|(_, p)| p.held.clone());
            assert_eq!(
                held,
                Some(Held::Refused(format!(
                    "this sandbox can't hold {}",
                    kiln_seams::sandbox::NO_POOLS
                )))
            );
            clock.0.store(10, Ordering::SeqCst);
            pools.started(a.clone(), false).await;
            clock.0.store(11 + keep_ms(), Ordering::SeqCst);
            pools.expire().await;
            assert!(pools.named().is_empty(), "its window passed");
            assert!(vms.pool_asks().is_empty(), "nothing kept, nothing let go");
            anyhow::Ok(())
        })
    }

    /// P3.2: the waiter, armed for the earliest window's end, lets a pool go as its window passes on the clock alone,
    /// one waiter for every pool named, and sleeps on for the next.
    #[test]
    fn p3_2_the_waiter_lets_a_pool_go_as_its_window_passes() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let vms = pooling();
            let clock = SimClock::new();
            let pools = Pools::new(
                Arc::<FakeVms>::clone(&vms) as Arc<dyn Sandbox>,
                Arc::new(clock.clone()) as Arc<dyn Clock>,
            );
            let (a, b) = (pool("a@sha256:00"), pool("b@sha256:00"));
            let since = clock.registrations();
            pools.started(a.clone(), false).await;
            clock.sleeping_since(since, 1).await;
            clock.advance(Duration::from_secs(1));
            pools.started(b.clone(), false).await;
            clock.advance_and_settle(KEEP).await;
            assert_eq!(
                vms.pool_asks(),
                vec![("keep", a.clone()), ("keep", b.clone()), ("let go", a)],
                "a let go as its window passed, b kept"
            );
            assert_eq!(
                clock.sleepers(),
                1,
                "one waiter, asleep until b's window ends"
            );
            anyhow::Ok(())
        })
    }

    fn shown(agent: &str, arm: Option<&str>) -> Shown {
        Shown {
            agent: agent.to_owned(),
            variant: arm.map(|a| ("exp-1".to_owned(), a.to_owned())),
            image: "harness-claude@sha256:00".to_owned(),
            size: Some((2, 2048)),
        }
    }

    /// P3.2 (and P7.1's variants): the server shows each runner's pools by the runner's rule over the starts it keeps:
    /// each agent's, and each variant's, apart, with its starts and hits; one without starts for its keep window is gone.
    #[test]
    fn p3_2_the_runners_pools_show_each_agents_and_variants_pool_with_its_hits() {
        let k = keep_ms();
        let starts = vec![
            (shown("agent-a", None), 1_000, false),
            (shown("agent-a", None), 2_000, true),
            (shown("agent-b", Some("v1")), 1_500, false),
            (shown("agent-b", Some("v2")), 1_600, false),
            (shown("agent-b", Some("v2")), 1_700, true),
            (shown("agent-c", None), 0, false),
        ];
        // At the end of agent-b v1's window (its one start at 1,500): agent-c's, whose last start was at 0, has passed.
        let v = view("runner-1", starts, 1_500 + k);
        let pools = v["pools"].as_array().cloned().unwrap_or_default();
        let rows: Vec<(String, Option<String>, u64, u64)> = pools
            .iter()
            .map(|p| {
                (
                    p["agent"].as_str().unwrap_or_default().to_owned(),
                    p["variant"]["arm"].as_str().map(str::to_owned),
                    p["starts"].as_u64().unwrap_or(0),
                    p["hits"].as_u64().unwrap_or(0),
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                ("agent-a".to_owned(), None, 2, 1),
                ("agent-b".to_owned(), Some("v1".to_owned()), 1, 0),
                ("agent-b".to_owned(), Some("v2".to_owned()), 2, 1)
            ],
            "agent-c's pool, without starts for its window, is gone: {v}"
        );
        let first = pools.first().map(|p| {
            (
                p["vcpus"].clone(),
                p["memory_mib"].clone(),
                p["until_ms"].clone(),
                p["warm_hosts"].clone(),
            )
        });
        assert_eq!(
            first,
            Some((
                serde_json::json!(2),
                serde_json::json!(2048),
                serde_json::json!(2_000 + k),
                serde_json::Value::Null
            ))
        );
        assert_eq!(
            view("runner-1", vec![(shown("agent-a", None), 0, true)], k + 1)["pools"],
            serde_json::json!([]),
            "no start of it for its keep window: the pool is gone"
        );
    }
}
