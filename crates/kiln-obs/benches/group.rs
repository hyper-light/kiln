//! The group on recorded data (`docs/observe.md` §12.9): the corpus's batches written through a
//! replicated group on real hyper-log files, as kiln's nodes write: `S` senders, each with one
//! write out at a time, proposed at the leader and done when the leader applied it; learners in
//! the same process take every message at once. Per write: commit latency (median and 99th
//! percentile with their 95% distribution-free intervals, hyper-measure); per sample: time,
//! allocations and page faults across every thread of the process (the log's included); and the
//! log writes the leader made, to show how many writes one flush carries.
//!
//! `KOBS_CORPUS=<corpus> KOBS_DIR=<a folder on a real disk> cargo bench -p kiln-obs --bench group`
//! (`KOBS_SENDERS=1,4,16,64`, `KOBS_LEARNERS=0`, `KOBS_WINDOW_S=10`, `KOBS_WRITES=2000`). The folder
//! must be on the disk the numbers are about: on tmpfs a flush is free.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::cast_precision_loss)]

#[path = "common/corpus.rs"]
mod corpus;
#[path = "common/load.rs"]
mod load;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::task::{Wake, Waker};
use std::time::{Duration, Instant};

use hyper_block::file::DeviceFile;
use hyper_durable::{GroupStore, LogStore as _, Output, Replica, Unbounded};
use hyper_log::Log;
use hyper_measure::quantile::{Level, cell};
use hyper_measure::{alloc, faults};
use hyper_raft::proto::{ConfChangeType, Message};
use kiln_obs::group;
use kiln_obs::hash::Keyed;
use kiln_obs::machine::Machine;
use kiln_obs::store::Answer;

#[global_allocator]
static COUNTING: alloc::Counting = alloc::Counting;

/// The members' waker: woken by the log as a write is durable, waited on when nothing moved.
struct Flag {
    woken: std::sync::Mutex<bool>,
    cv: std::sync::Condvar,
}

impl Wake for Flag {
    fn wake(self: Arc<Self>) {
        *self.woken.lock().unwrap() = true;
        self.cv.notify_one();
    }
}

impl Flag {
    /// Waits until woken, or `most` passes (a member's deadline); whether it was woken.
    fn wait(&self, most: Duration) -> bool {
        let g = self.woken.lock().unwrap();
        let (mut g, t) = self.cv.wait_timeout_while(g, most, |w| !*w).unwrap();
        *g = false;
        !t.timed_out()
    }
}

struct Member {
    r: Replica<GroupStore<DeviceFile>, Machine>,
    _log: Log<DeviceFile>,
    out: Output<(u64, Answer)>,
}

fn member(dir: &std::path::Path, id: u64, frame: usize) -> Member {
    let (log, store) = group::open_log_framed(&dir.join(format!("m{id}.log")), frame).unwrap();
    let machine = Machine::open(&dir.join(format!("m{id}")), Keyed::new(), &group::founding(&[1])).unwrap();
    let r = Replica::open(&group::settings(id, store.depth()).unwrap(), store, machine, Unbounded).unwrap();
    Member { r, _log: log, out: Output::default() }
}

/// One round: every member driven and its messages carried; the leader's answers returned, and
/// the earliest time a member asks to be driven again.
fn round(members: &mut [Member], start: Instant, waker: &Waker, answers: &mut Vec<(u64, Answer)>, wake: &mut Option<u64>) -> bool {
    let now = u64::try_from(start.elapsed().as_nanos()).unwrap();
    let mut messages: Vec<Message> = Vec::new();
    *wake = None;
    for (i, m) in members.iter_mut().enumerate() {
        let mut got = Vec::new();
        if let Some(w) = group::pump(&mut m.r, now, waker, &mut m.out, &mut messages, &mut got).unwrap() {
            *wake = Some(wake.map_or(w, |x: u64| x.min(w)));
        }
        if i == 0 {
            answers.append(&mut got);
        }
    }
    let moved = !messages.is_empty();
    for msg in messages {
        let to = msg.to as usize - 1;
        if let Some(m) = members.get_mut(to) {
            m.r.step(msg).unwrap();
        }
    }
    moved
}

fn main() {
    let series = match corpus::load() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return;
        }
    };
    let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_owned());
    let senders: Vec<usize> = env("KOBS_SENDERS", "1,4,16,64").split(',').filter_map(|s| s.parse().ok()).collect();
    let learners: u64 = env("KOBS_LEARNERS", "0").parse().unwrap();
    let window_s: i64 = env("KOBS_WINDOW_S", "10").parse().unwrap();
    let writes_max: usize = env("KOBS_WRITES", "2000").parse().unwrap();
    let base = PathBuf::from(env("KOBS_DIR", "target/kobs-group"));
    let frame: usize = env("KOBS_FRAME_KIB", "1024").parse::<usize>().unwrap() << 10;
    std::fs::create_dir_all(&base).unwrap();
    // The corpus's batches, a window a batch across every series (each series defined in its first).
    let batches = load::batches_of(&series, window_s * 1000);
    let samples_of: Vec<u64> = batches.iter().map(|b| load::samples_in(b)).collect();
    let batches: Vec<Vec<u8>> = batches.into_iter().take(writes_max).collect();
    let samples: u64 = samples_of.iter().take(batches.len()).sum();
    let bytes: usize = batches.iter().map(Vec::len).sum();
    println!(
        "{} writes of a {window_s} s window each: {samples} samples, {:.1} a write, {:.2} bytes a sample; {learners} learner(s); frames of {} KiB; {}",
        batches.len(),
        samples as f64 / batches.len() as f64,
        bytes as f64 / samples as f64,
        frame >> 10,
        base.display()
    );
    for &s in &senders {
        let dir = tempfile::tempdir_in(&base).unwrap();
        let flag = Arc::new(Flag { woken: std::sync::Mutex::new(false), cv: std::sync::Condvar::new() });
        let waker = Waker::from(flag.clone());
        let start = Instant::now();
        let mut members = vec![member(dir.path(), 1, frame)];
        members[0].r.campaign().unwrap();
        let mut answers = Vec::new();
        let mut wake = None;
        while !members[0].r.is_leader() {
            round(&mut members, start, &waker, &mut answers, &mut wake);
        }
        for id in 2..2 + learners {
            members.push(member(dir.path(), id, frame));
            members[0].r.change(Vec::new(), &group::change(id, ConfChangeType::AddLearnerNode)).unwrap();
            while !members[0].r.configuration().learners.contains(&id) {
                round(&mut members, start, &waker, &mut answers, &mut wake);
                std::thread::sleep(Duration::from_micros(50));
            }
        }
        answers.clear();
        // Each sender's write out: (its batch, when it was proposed).
        let mut next = 0usize;
        let mut out: Vec<Option<(usize, Instant)>> = vec![None; s];
        let mut queue: VecDeque<usize> = (0..s).collect();
        let mut latencies: Vec<u64> = Vec::with_capacity(batches.len());
        let (writes0, faults0, usage0) = (members[0].r.writes(), faults::read().unwrap(), hyper_measure::usage::this().unwrap());
        let t0 = Instant::now();
        alloc::begin();
        let mut done = 0usize;
        let (mut woken, mut timed_out) = (0u64, 0u64);
        let mut deadlines = 0u64;
        while done < batches.len() {
            // Every sender with nothing out sends its next write.
            while let Some(i) = queue.pop_front() {
                if next >= batches.len() {
                    break;
                }
                let entry = kiln_obs::command::write(0x5e00 + i as u64, next as u64 + 1, 0, &batches[next]);
                members[0].r.propose(Vec::new(), entry).unwrap();
                out[i] = Some((next, Instant::now()));
                next += 1;
            }
            let moved = round(&mut members, start, &waker, &mut answers, &mut wake);
            let answered = !answers.is_empty();
            for (_, a) in answers.drain(..) {
                let Answer::Write { sender, .. } = a else { continue };
                let i = (sender - 0x5e00) as usize;
                if let Some((_, at)) = out[i].take() {
                    latencies.push(u64::try_from(at.elapsed().as_nanos()).unwrap());
                    done += 1;
                    queue.push_back(i);
                }
            }
            if !moved && !answered {
                let now = u64::try_from(start.elapsed().as_nanos()).unwrap();
                let d = members.iter().filter_map(|m| m.r.deadline()).min().into_iter().chain(wake).min();
                deadlines += u64::from(d.is_some());
                let deadline = d.map_or(Duration::from_millis(5), |d| Duration::from_nanos(d.saturating_sub(now)));
                if flag.wait(deadline.min(Duration::from_micros(std::env::var("KOBS_WAIT_US").ok().and_then(|v| v.parse().ok()).unwrap_or(5000)))) {
                    woken += 1;
                } else {
                    timed_out += 1;
                }
            }
        }
        let took = t0.elapsed();
        let counts = alloc::end();
        let f = faults::read().unwrap().since(&faults0);
        let cpu = hyper_measure::usage::this().unwrap().since(&usage0);
        let w = members[0].r.writes();
        let readies = w.readies - writes0.readies;
        println!(
            "      log writes: {readies} readies, {} empty, {} commit alone after quiet, {} fenced, {} starts; CPU {:.2} s user + {:.2} s system over {:.2} s; waits {woken} woken, {timed_out} timed out, {deadlines} with a deadline",
            w.empty - writes0.empty,
            w.quiet - writes0.quiet,
            w.fenced - writes0.fenced,
            w.starts - writes0.starts,
            cpu.user_ns as f64 / 1e9,
            cpu.system_ns as f64 / 1e9,
            took.as_secs_f64()
        );
        // Learners take everything: how long after the leader's last answer.
        let lag_start = Instant::now();
        let target = members[0].r.applied().index;
        while members.iter().skip(1).any(|m| m.r.applied().index < target) {
            round(&mut members, start, &waker, &mut answers, &mut wake);
            std::thread::yield_now();
        }
        let lag = lag_start.elapsed();
        latencies.sort_unstable();
        let us = |n: u64| format!("{:.0} µs", n as f64 / 1e3);
        let coverage = Level::new(95, 100).unwrap();
        println!(
            "{s:>3} senders: {:.0} writes/s, {:.2} M samples/s; commit p50 {}, p99 {}; {:.1} ns, {:.4} allocations, {:.5} page faults a sample; {:.1} writes a log write ({readies} log writes); learners {:.1} ms behind",
            batches.len() as f64 / took.as_secs_f64(),
            samples as f64 / took.as_secs_f64() / 1e6,
            cell(&latencies, Level::P50, coverage, us),
            cell(&latencies, Level::P99, coverage, us),
            took.as_nanos() as f64 / samples as f64,
            counts.calls() as f64 / samples as f64,
            f.total() as f64 / samples as f64,
            batches.len() as f64 / readies.max(1) as f64,
            lag.as_secs_f64() * 1e3
        );
        // Every learner holds what the leader holds.
        let want = members[0].r.machine().read().digest(true).unwrap();
        for m in members.iter().skip(1) {
            assert_eq!(m.r.machine().read().digest(true).unwrap(), want, "a learner holds another state");
        }
        drop(members);
    }
}
