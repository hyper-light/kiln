//! Quiver's run: its node in this process, as kiln runs it. The publisher publishes on this thread;
//! the subscriptions are shared out over the worker threads, each taking from each of its
//! subscriptions in turn and, with nothing to take, waiting on the topic (or, `--busy`, looking
//! again).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use futures::FutureExt as _;
use quiver::{Node, NodeConfig};
use quiver_api::{Class, Next, Start, Subscription, Topic, Topics};

use crate::follow::{Follow, Schedule, Seen, Window};
use crate::report::{Run, tally};
use crate::{Args, System, thread_cpu_s};

struct Shared {
    base: Instant,
    window: Window,
    /// The least number every subscription of each worker takes next.
    caught_up: Vec<AtomicU64>,
    stop: AtomicBool,
}

fn worker(
    index: usize,
    mut subs: Vec<(Box<dyn Subscription>, Follow)>,
    shared: Arc<Shared>,
    busy: bool,
) -> (Seen, Vec<Follow>) {
    let mut seen = Seen::new();
    loop {
        let mut took = false;
        let mut lowest = u64::MAX;
        let mut ended = false;
        for (sub, follow) in &mut subs {
            while let Some(next) = sub.take() {
                took = true;
                match next {
                    Next::Message(m) => {
                        let at = shared.base.elapsed().as_nanos() as u64;
                        seen.take(follow, m.payload, at, &shared.window);
                    }
                    Next::Lag { .. } => follow.lag(),
                    Next::Ended => {
                        ended = true;
                        break;
                    }
                }
            }
            lowest = lowest.min(follow.next());
        }
        shared.caught_up[index].store(lowest, Ordering::Release);
        if ended || shared.stop.load(Ordering::Acquire) {
            break;
        }
        if took || busy {
            continue;
        }
        if let Some((first, _)) = subs.first_mut() {
            futures::executor::block_on(first.ready());
        }
    }
    (seen, subs.into_iter().map(|(_, f)| f).collect())
}

/// One run; `minute` is told each minute of the schedule and what was published by then.
pub fn run(a: &Args, seed: u64, mut minute: impl FnMut(u64, u64)) -> Run {
    let node = Node::new(NodeConfig {
        memory: 4 << 30,
        topics: 4,
        subscriptions: a.subscribers + 16,
    })
    .unwrap();
    node.create(
        "bench",
        Topic {
            class: Class::Ephemeral,
            retain_messages: a.retain,
            retain_bytes: 1 << 30,
        },
    )
    .now_or_never()
    .unwrap()
    .unwrap();
    let workers = a.workers.min(a.subscribers).max(1);
    let mut shares: Vec<Vec<(Box<dyn Subscription>, Follow)>> =
        (0..workers).map(|_| Vec::new()).collect();
    for i in 0..a.subscribers {
        let sub = node
            .subscribe("bench", Start::Latest)
            .now_or_never()
            .unwrap()
            .unwrap();
        shares[i % workers].push((sub, Follow::new()));
    }
    let shared = Arc::new(Shared {
        base: Instant::now(),
        window: Window::new(),
        caught_up: (0..workers).map(|_| AtomicU64::new(0)).collect(),
        stop: AtomicBool::new(false),
    });
    let load_before = hyper_measure::usage::load();
    let cpu_before = hyper_measure::usage::this().ok();
    let handles: Vec<_> = shares
        .into_iter()
        .enumerate()
        .map(|(i, subs)| {
            let shared = Arc::clone(&shared);
            let busy = a.busy;
            std::thread::Builder::new()
                .name(format!("sub-{i}"))
                .spawn(move || worker(i, subs, shared, busy))
                .unwrap()
        })
        .collect();

    // A millisecond in, so every worker is waiting.
    let mut schedule = Schedule::new(a, seed, 1_000_000, &shared.window);
    let mut payload = vec![0u8; a.payload];
    let mut refused = 0u64;
    let mut last_minute = 0u64;
    let began = Instant::now();
    let publisher_before = thread_cpu_s();
    loop {
        if schedule.over(shared.base.elapsed().as_nanos() as u64) {
            break;
        }
        let Some((at, number)) = schedule.next() else {
            break;
        };
        while (shared.base.elapsed().as_nanos() as u64) < at {
            std::hint::spin_loop();
        }
        crate::follow::stamp(&mut payload, at, number);
        if !matches!(
            node.publish("bench", None, &payload).now_or_never(),
            Some(Ok(_))
        ) {
            refused += 1;
        }
        if Schedule::minute(at) > last_minute {
            last_minute = Schedule::minute(at);
            minute(last_minute, schedule.offered());
        }
    }
    let seconds = began.elapsed().as_secs_f64();
    let publisher_cpu_s = match (publisher_before, thread_cpu_s()) {
        (Some(b), Some(a)) => a - b,
        _ => f64::NAN,
    };
    // Every worker takes what was published (the fact waited on), then the topic ends.
    let deadline = Instant::now() + Duration::from_secs(60);
    while shared
        .caught_up
        .iter()
        .any(|c| c.load(Ordering::Acquire) < schedule.offered())
        && Instant::now() < deadline
    {
        std::thread::yield_now();
    }
    shared.stop.store(true, Ordering::Release);
    node.delete("bench").now_or_never().unwrap().unwrap();
    let mut seen = Seen::new();
    let mut follows = Vec::new();
    for h in handles {
        let (s, f) = h.join().unwrap();
        seen.merge(&s);
        follows.extend(f);
    }
    let cpu_s = match (cpu_before, hyper_measure::usage::this().ok()) {
        (Some(b), Some(a)) => a.since(&b).cpu_ns() as f64 / 1e9,
        _ => f64::NAN,
    };
    let t = tally(&mut seen, &follows, &schedule);
    Run {
        system: System::Quiver,
        busy: a.busy,
        rate: a.rate,
        subscribers: a.subscribers,
        subscriptions: a.subscribers,
        workers,
        published: schedule.offered(),
        unoffered: schedule.unoffered,
        publish_errors: refused,
        seconds,
        window_s: a.seconds,
        seen,
        undelivered: t.undelivered,
        silent: t.silent,
        lags: t.lags,
        doubled: t.doubled,
        kept_up: t.kept_up,
        cpu_s,
        publisher_cpu_s,
        broker_cpu_s: None,
        load_before,
        load_after: hyper_measure::usage::load(),
    }
}
