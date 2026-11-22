//! G9 step 3's measure (goal a2a44298, "goal edit to driver action, p99 is at most 50 ms"): how
//! long after a goal edit the goal driver acts on it, on a real store on disk, the real clock and
//! real agent sessions, the driver running as the server runs it (`goals::run`: `Driver::follow`
//! on the store's change feed, the hub's agents' news, its riders).
//!
//! Each edit goes through kiln's operation table as an MCP call does (`crate::ops::call`, the
//! route table and the audit trail), one of four kinds, in a cycle on each agent's stack:
//! - `add`: three goals added to a stack with none to go; the first goes to the agent;
//! - `update`: the goal under way reworded; it goes to the agent again as it now says;
//! - `done`: the goal under way said met by a person (`goals.control`, `done`); the next goes;
//! - `verdict`: the agent says the goal under way is met, with what shows it (`goals.done`); the next goes.
//!
//! The action is the driver's send: a committed row of the agent's durable mail (`agent_mail`),
//! what every goal sent to an agent is first. It's timed from just before the edit is asked for
//! (before its commit) to when the harness sees that row committed (woken by the store's change
//! count of the mail's tables): an upper bound on commit-to-action, never under it. Between
//! edits the agent takes what it was sent and goes idle, so each edit finds it free; the skip
//! that closes a cycle (its stack done again, for the next `add`), the clear that takes the
//! cycle's goals out of it (so one agent's edits have no bound) and those waits aren't timed.
//!
//! The load is how many agents' stacks are edited at once, each its own session and cycle. Nothing
//! here asserts on time: the measurement is printed as one JSON line, for the coordinator's run.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::hub::Hub;

/// What the measurement is asked for: `edits` timed in all, spread over `agents` edited at once.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Load {
    pub edits: usize,
    pub agents: usize,
}

/// The edits a cycle times, in order, on one agent's stack.
const CYCLE: [&str; 4] = ["add", "update", "done", "verdict"];

/// One edit's time to its action, or none: no action came within the driver's longest reminder gap
/// (`REMIND_MAX_MS`, past which the driver itself would send the goal again).
struct Timed {
    kind: &'static str,
    took: Option<Duration>,
}

/// What a run measured: microseconds, nearest rank.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct Measured {
    pub n: usize,
    pub agents: usize,
    pub p50_us: u64,
    pub p99_us: u64,
    pub p999_us: u64,
    pub max_us: u64,
    /// Edits whose action never came (left out of the percentiles).
    pub lost: usize,
    /// Each kind's own p50, p99, p99.9 and max, and how many.
    pub kinds: serde_json::Map<String, Value>,
    /// Where the time went, while the edits ran (`turn_times`): the waits for the stacks' turns,
    /// every stack's turn held, and the driver's rounds, each within a factor of two.
    pub turns: serde_json::Map<String, Value>,
    /// Which stacks the driver's rounds stepped (`KILN_GOAL_FEED`, `goals::Steps`): `stacks`, those said changed;
    /// `every`, the control.
    pub feed: &'static str,
}

/// The `q`th quantile of `sorted` by nearest rank (`q` in 0..=1).
fn rank(sorted: &[u64], q: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    // Nearest rank: the smallest value with at least q of them at or under it.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
    let k = ((q * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    sorted.get(k - 1).copied().unwrap_or(0)
}

fn us(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

/// p50, p99, p99.9 and max of `of`.
fn summary(of: &[u64]) -> (u64, u64, u64, u64) {
    let mut s = of.to_vec();
    s.sort_unstable();
    (rank(&s, 0.50), rank(&s, 0.99), rank(&s, 0.999), s.last().copied().unwrap_or(0))
}

async fn op(hub: &Arc<Hub>, name: &str, input: Value) -> anyhow::Result<Value> {
    crate::ops::call(crate::ops::Cx { hub: Arc::clone(hub), caller: crate::authz::Caller::owner() }, name, input).await.map_err(|(s, m)| anyhow::anyhow!("{name}: {s} {m}"))
}

/// The last revision of `agent`'s mail: it moves with each send to it.
async fn mail_at(hub: &Hub, agent: &str) -> anyhow::Result<i64> {
    let a = agent.to_owned();
    hub.store.read(move |c| Ok(c.query_row("SELECT COALESCE(MAX(revision), 0) FROM agent_mail WHERE recipient = ?1", [a], |r| r.get::<_, i64>(0))?)).await
}

/// Until a send to `agent` past mail revision `before` is committed: when that was seen, or none
/// past `give_up`.
async fn sent_after(hub: &Hub, agent: &str, before: i64, give_up: Instant) -> anyhow::Result<Option<Instant>> {
    use futures::FutureExt as _;
    let changes = hub.store.changes();
    let told = changes.watch(crate::harness::mail::TABLES);
    loop {
        // Where the mail's tables stand before the look: a write after it wakes the wait.
        let at = changes.of(crate::harness::mail::TABLES);
        if mail_at(hub, agent).await? > before {
            return Ok(Some(Instant::now()));
        }
        let left = give_up.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(None);
        }
        let moved = async {
            while changes.of(crate::harness::mail::TABLES) == at {
                told.notified().await;
            }
        };
        futures::select_biased! {
            () = moved.fuse() => {}
            () = crate::rt::sleep(left).fuse() => {}
        }
    }
}

/// Until `agent` has taken all it was sent and is idle (not timed).
async fn settled(hub: &Hub, agent: &str) -> anyhow::Result<()> {
    crate::harness::ops::end_to_end::source_until(hub, agent, "the agent takes what it was sent", || {
        let recipient = agent.to_owned();
        async move {
            let pending = hub
                .store
                .read(move |c| Ok(c.query_row("SELECT COALESCE(SUM(disposition IN ('waiting','dispatched','admitted')),0) FROM agent_mail WHERE recipient=?1", [recipient], |r| r.get::<_, i64>(0))?))
                .await?;
            Ok(pending == 0 && hub.harness.info(agent).is_some_and(|i| i.status == crate::harness::model::Status::Idle))
        }
    })
    .await
}

/// The goal under way on `agent`'s stack, as `stack` (a `goals.*` answer) has it.
fn under_way(stack: &Value) -> Option<String> {
    stack.get("items")?.as_array()?.iter().find(|i| i.get("status").and_then(Value::as_str) == Some("active")).and_then(|i| i.get("id")?.as_str().map(str::to_owned))
}

/// One agent's share of the edits (`edits`), cycle after cycle, each edit timed to its send.
async fn agent_edits(hub: &Arc<Hub>, agent: &str, edits: usize) -> anyhow::Result<Vec<Timed>> {
    let give_up = Duration::from_millis(u64::try_from(super::REMIND_MAX_MS).unwrap_or(u64::MAX));
    let mut out = Vec::with_capacity(edits);
    let mut cycle = 0usize;
    let mut current: Option<String> = None;
    while out.len() < edits {
        for kind in CYCLE {
            if out.len() >= edits {
                break;
            }
            settled(hub, agent).await?;
            let input = match kind {
                "add" => json!({"agent": agent, "goals": [format!("cycle {cycle}: the first goal"), format!("cycle {cycle}: the second goal"), format!("cycle {cycle}: the third goal")]}),
                "update" => {
                    let id = current.clone().ok_or_else(|| anyhow::anyhow!("no goal under way to reword"))?;
                    json!({"agent": agent, "id": id, "text": format!("cycle {cycle}: the first goal, reworded")})
                }
                "done" => json!({"agent": agent, "action": "done"}),
                _ => json!({"agent": agent, "evidence": format!("cycle {cycle}: its checks pass")}),
            };
            let name = match kind {
                "add" => "goals.add",
                "update" => "goals.update",
                "done" => "goals.control",
                _ => "goals.done",
            };
            let before = mail_at(hub, agent).await?;
            let began = Instant::now();
            let stack = op(hub, name, input).await?;
            let seen = sent_after(hub, agent, before, began + give_up).await?;
            out.push(Timed { kind, took: seen.map(|t| t.saturating_duration_since(began)) });
            current = under_way(&stack).or(current);
        }
        // The cycle's last goal skipped: its stack done again, for the next cycle's `add` (not timed).
        // Then the cycle's goals cleared from it: a stack holds at most `ITEMS` goals, and those a
        // cycle left there stopped one agent's edits at about 660 (not timed).
        settled(hub, agent).await?;
        op(hub, "goals.control", json!({"agent": agent, "action": "skip"})).await?;
        settled(hub, agent).await?;
        op(hub, "goals.control", json!({"agent": agent, "action": "clear"})).await?;
        current = None;
        cycle += 1;
    }
    Ok(out)
}

/// Measures goal edit to driver action under `load` on a scratch server of its own: its store a
/// file on disk, its clock the real one, its agents `load.agents` sessions of the recording
/// harness (`harness::ops::end_to_end::recording_program`), the goal driver running as the
/// server runs it (`goals::run`).
pub(crate) async fn measure(load: Load) -> anyhow::Result<Measured> {
    anyhow::ensure!(load.edits > 0 && load.agents > 0, "edits and agents are at least 1");
    let dir = tempfile::tempdir()?;
    let hub = crate::hub::tests::scratch_hub_persistent(dir.path().join("kiln.db"), |c| c.conversations.off = true).await?;
    let h = Arc::clone(&hub);
    crate::rt::spawn(async move {
        let _ = crate::harness::registry::Registry::run_local(h).await;
    });
    let program = super::native_goal_program(&hub, dir.path()).await?;
    let mut agents = Vec::with_capacity(load.agents);
    for k in 0..load.agents {
        let cwd = dir.path().join(format!("agent-{k}"));
        std::fs::create_dir_all(&cwd)?;
        let spec = crate::harness::session::Spec { kind: "claude".into(), cwd: Some(cwd.to_string_lossy().into_owned()), program: Some(program.to_string_lossy().into_owned()), ..Default::default() };
        let opened = hub.harness.open(&hub, "local", spec, 1).await.map_err(|e| anyhow::anyhow!("{e:#}"))?;
        let id = opened.into_iter().next().map(|i| i.id).ok_or_else(|| anyhow::anyhow!("no agent opened"))?;
        crate::harness::ops::end_to_end::source_ready(&hub, &id).await?;
        agents.push(id);
    }
    // The goal driver, as the server runs it.
    let driver = {
        let h = Arc::clone(&hub);
        crate::rt::spawn(async move { super::run(h).await })
    };
    // Where the time goes, from here: the sessions opened and the driver started (G9 step 3's trace).
    super::turn_times::reset();
    // The edits shared out: the first `edits % agents` agents take one more.
    let (each, more) = (load.edits / load.agents, load.edits % load.agents);
    let runs = agents.iter().enumerate().map(|(k, a)| agent_edits(&hub, a, each + usize::from(k < more)));
    let all = futures::future::join_all(runs).await;
    let turns = super::turn_times::summary();
    hub.shutdown.cancel();
    let _ = driver.await;
    let mut timed = Vec::with_capacity(load.edits);
    for a in all {
        timed.extend(a?);
    }
    let took: Vec<u64> = timed.iter().filter_map(|t| t.took.map(us)).collect();
    let (p50_us, p99_us, p999_us, max_us) = summary(&took);
    let mut kinds = serde_json::Map::new();
    for kind in CYCLE {
        let of: Vec<u64> = timed.iter().filter(|t| t.kind == kind).filter_map(|t| t.took.map(us)).collect();
        let (p50, p99, p999, max) = summary(&of);
        kinds.insert(kind.into(), json!({"n": of.len(), "p50_us": p50, "p99_us": p99, "p999_us": p999, "max_us": max}));
    }
    let feed = if super::Steps::from_env() == super::Steps::Stacks { "stacks" } else { "every" };
    Ok(Measured { n: took.len(), agents: load.agents, p50_us, p99_us, p999_us, max_us, lost: timed.len() - took.len(), kinds, turns, feed })
}

/// The measurement as one JSON line, its times in milliseconds.
pub(crate) fn line(m: &Measured) -> String {
    #[allow(clippy::cast_precision_loss)]
    let ms = |us: u64| us as f64 / 1000.0;
    json!({"measure": "goal_edit_to_driver_action", "n": m.n, "agents": m.agents, "p50_ms": ms(m.p50_us), "p99_ms": ms(m.p99_us), "p999_ms": ms(m.p999_us), "max_ms": ms(m.max_us), "lost": m.lost, "kinds": m.kinds, "turns": m.turns, "feed": m.feed}).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A count the measurement is asked for, from the environment.
    fn asked(var: &str) -> anyhow::Result<usize> {
        std::env::var(var).map_err(|_| anyhow::anyhow!("set {var}"))?.trim().parse().map_err(|e| anyhow::anyhow!("{var}: {e}"))
    }

    /// G9 step 3 (goal a2a44298): goal edit to driver action, measured. `KILN_GOAL_EDITS`: the
    /// edits timed; `KILN_GOAL_AGENTS`: the agents edited at once (the load). Prints one JSON line;
    /// asserts nothing on time.
    #[test]
    #[ignore = "a measurement: KILN_GOAL_EDITS=… KILN_GOAL_AGENTS=… cargo test -p kiln --lib goals::latency -- --ignored --nocapture"]
    fn g9_s3_goal_edit_to_driver_action_measured() -> anyhow::Result<()> {
        let load = Load { edits: asked("KILN_GOAL_EDITS")?, agents: asked("KILN_GOAL_AGENTS")? };
        let m = crate::rt::block_on(measure(load))?;
        println!("{}", line(&m));
        Ok(())
    }

    /// G9 step 3: the measure gives a measurement: one cycle on one agent (each kind of edit once),
    /// every edit's action seen, its JSON line whole.
    #[test]
    fn g9_s3_the_edit_to_action_measure_measures() -> anyhow::Result<()> {
        let m = crate::rt::block_on(measure(Load { edits: CYCLE.len(), agents: 1 }))?;
        anyhow::ensure!(m.n == CYCLE.len() && m.lost == 0, "{m:?}");
        anyhow::ensure!(m.kinds.values().all(|k| k.get("n").and_then(Value::as_u64) == Some(1) && k.get("p999_us").and_then(Value::as_u64).is_some()), "{m:?}");
        anyhow::ensure!(m.p50_us <= m.p99_us && m.p99_us <= m.p999_us && m.p999_us <= m.max_us, "{m:?}");
        let v: Value = serde_json::from_str(&line(&m))?;
        anyhow::ensure!(["n", "p50_ms", "p99_ms", "p999_ms", "max_ms", "turns"].iter().all(|k| v.get(k).is_some()), "{v}");
        // The edits took their stack's turn, and the driver went round.
        let counted = |k: &str| m.turns.get(k).and_then(|t| t.get("n")).and_then(Value::as_u64).unwrap_or(0);
        anyhow::ensure!(counted("own_wait") >= 1 && counted("rounds") >= 1, "{m:?}");
        Ok(())
    }

    #[test]
    fn nearest_rank_quantiles() {
        let s: Vec<u64> = (1..=100).collect();
        assert_eq!((rank(&s, 0.5), rank(&s, 0.99), rank(&s, 0.999), rank(&s, 1.0)), (50, 99, 100, 100));
        let k: Vec<u64> = (1..=1000).collect();
        assert_eq!((rank(&k, 0.99), rank(&k, 0.999)), (990, 999));
        assert_eq!(summary(&[3, 1, 2]), (2, 3, 3, 3));
        assert_eq!(rank(&[7], 0.99), 7);
        assert_eq!(rank(&[], 0.5), 0);
    }
}
