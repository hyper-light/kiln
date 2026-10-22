//! A runtime for the tests: two shards, started once; [`block_on`] runs a future on one.

use std::future::Future;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use hyper_rt::{Runtime, RuntimeConfig, SlotHolder};

struct Rt {
    _runtime: Runtime,
    holders: Vec<SlotHolder>,
    next: AtomicUsize,
}

static RT: OnceLock<Result<Rt, String>> = OnceLock::new();

pub(crate) fn config(shards: u16, tasks: usize) -> RuntimeConfig {
    RuntimeConfig {
        shards,
        tasks_per_shard: tasks,
        timers_per_shard: tasks.saturating_mul(2),
        interests_per_shard: hyper_rt::runtime::interests_for(tasks),
        ring_entries: 1024,
        step_budget_ns: 250_000,
        timer_tick_ns: 100_000,
        batch: 64,
        pin: false,
        cores: Vec::new(),
        page_bytes: 4096,
        spin_ns: 0,
        wake_tracking: None,
        latency_objective_ns: None,
    }
}

fn rt() -> Result<&'static Rt, String> {
    RT.get_or_init(|| {
        let runtime = Runtime::start(&config(2, 4096)).map_err(|e| e.to_string())?;
        let holders = runtime.shard_ids().iter().filter_map(|id| runtime.holder_of(*id)).collect();
        Ok(Rt { _runtime: runtime, holders, next: AtomicUsize::new(0) })
    })
    .as_ref()
    .map_err(Clone::clone)
}

/// Runs `fut` on a shard to its end, for at most a minute.
pub(crate) fn block_on<T: Send + 'static>(fut: impl Future<Output = T> + Send + 'static) -> Result<T, String> {
    let rt = rt()?;
    let i = rt.next.fetch_add(1, Ordering::Relaxed);
    let holder = rt.holders.get(i % rt.holders.len().max(1)).copied().ok_or("no shard")?;
    let (tx, rx) = std::sync::mpsc::channel();
    hyper_rt::runtime::submit_to_holder(holder, async move {
        let _ = tx.send(fut.await);
    })
    .map_err(|e| e.to_string())?;
    rx.recv_timeout(Duration::from_secs(60)).map_err(|e| format!("the test's future didn't finish: {e}"))
}

/// Spawns `fut` on the current shard.
pub(crate) fn spawn(fut: impl Future<Output = ()> + 'static) {
    let _ = hyper_rt::futures::spawn_detached(fut);
}
