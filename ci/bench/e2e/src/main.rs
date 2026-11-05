//! kiln's open-loop end-to-end generator (ci/bench/README.md, "End to end"; research-bench.md:
//! "open loop, latency from intended send time, HdrHistogram; never at max load").
//!
//! Asks go to the asking runner's reach socket (`kiln run`'s, `crates/kiln/src/reach/local.rs`) at
//! a fixed rate, each at its intended send time `t0 + i / rate` whether or not earlier asks have
//! been answered (wrk2's and Nighthawk's open loop: a slow answer never delays the next ask, so
//! queueing shows in the latencies instead of hiding as a lower rate). Each ask is a no-op command
//! (`true` by default) for one other runner (`--target`), detached, with no tree to copy, so what
//! is measured is placement and its messages over the reach link (QUIC between the two runners),
//! not work. Two latencies per ask, both from its intended send time:
//!
//! - ask → answer: the first event the socket sends back (placement's answer: picked, queued,
//!   never, or an error);
//! - ask → started: the runner's word that the process started (`Ev::Started`).
//!
//! Recorded in a log-linear histogram of HdrHistogram's layout (2048 linear sub-buckets per power
//! of two: under 0.1% error at every value), after a warm-up that is discarded.
//!
//! Usage: kiln-bench-e2e --socket PATH --target RUNNER --rate ASKS_PER_S --seconds S
//!        [--warmup S] [--workers W] [--command CMD] [--json OUT]

mod hist;
mod proto;

use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::process::ExitCode;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hist::Hist;
use proto::{Deframer, Ev, Req, RunReq};

struct Args {
    socket: String,
    target: String,
    rate: f64,
    seconds: f64,
    warmup: f64,
    workers: usize,
    command: String,
    json: Option<String>,
}

fn parse() -> Result<Args, String> {
    let mut a = Args { socket: String::new(), target: String::new(), rate: 0.0, seconds: 0.0, warmup: 5.0, workers: 256, command: "true".into(), json: None };
    let argv: Vec<String> = std::env::args().skip(1).collect();
    for pair in argv.chunks(2) {
        let (Some(k), Some(v)) = (pair.first(), pair.get(1)) else { return Err(format!("{pair:?}: a flag without a value")) };
        let num = |v: &str| v.parse::<f64>().map_err(|e| format!("{k} {v}: {e}"));
        match k.as_str() {
            "--socket" => a.socket = v.clone(),
            "--target" => a.target = v.clone(),
            "--rate" => a.rate = num(v)?,
            "--seconds" => a.seconds = num(v)?,
            "--warmup" => a.warmup = num(v)?,
            "--workers" => a.workers = v.parse().map_err(|e| format!("{k} {v}: {e}"))?,
            "--command" => a.command = v.clone(),
            "--json" => a.json = Some(v.clone()),
            _ => return Err(format!("unknown flag {k}")),
        }
    }
    if a.socket.is_empty() || a.target.is_empty() || a.rate <= 0.0 || a.seconds <= 0.0 || a.workers == 0 {
        return Err("usage: --socket PATH --target RUNNER --rate R --seconds S [--warmup S] [--workers W] [--command CMD] [--json OUT]".into());
    }
    Ok(a)
}

/// What one worker saw.
#[derive(Default)]
struct Seen {
    answer: Hist,
    started: Hist,
    /// Asks answered with an error or an exit before a start, and their first words.
    refused: u64,
    why: Vec<String>,
    /// Asks the socket couldn't take at all (connect, send or read failed).
    failed: u64,
    measured: u64,
}

/// One ask, carried from its intended send time to its start (or its refusal).
fn one(a: &Args, intended: Instant, record: bool, seen: &mut Seen) {
    let req = Req::Run(Box::new(RunReq { target: a.target.clone(), command: a.command.clone(), detach: true, ..RunReq::default() }));
    let Ok(bytes) = proto::frame(&req) else {
        seen.failed += 1;
        return;
    };
    let Ok(mut s) = UnixStream::connect(&a.socket) else {
        seen.failed += 1;
        return;
    };
    if s.write_all(&bytes).is_err() {
        seen.failed += 1;
        return;
    }
    let mut de = Deframer::default();
    let mut buf = vec![0u8; 16 * 1024];
    let mut answered = false;
    loop {
        let ev = match de.take() {
            Ok(Some(ev)) => ev,
            Ok(None) => match s.read(&mut buf) {
                Ok(0) | Err(_) => {
                    seen.failed += 1;
                    return;
                }
                Ok(n) => {
                    de.push(buf.get(..n).unwrap_or_default());
                    continue;
                }
            },
            Err(e) => {
                seen.failed += 1;
                if seen.why.len() < 8 {
                    seen.why.push(e);
                }
                return;
            }
        };
        let at = intended.elapsed();
        if !answered {
            answered = true;
            if record {
                seen.answer.add(at);
            }
        }
        match ev {
            Ev::Started { .. } => {
                if record {
                    seen.started.add(at);
                    seen.measured += 1;
                }
                return;
            }
            Ev::Error(why) | Ev::Exit { error: Some(why), .. } => {
                seen.refused += 1;
                if seen.why.len() < 8 {
                    seen.why.push(why);
                }
                return;
            }
            Ev::Exit { .. } => {
                seen.refused += 1;
                return;
            }
            _ => {}
        }
    }
}

fn main() -> ExitCode {
    let a = match parse() {
        Ok(a) => Arc::new(a),
        Err(e) => {
            eprintln!("kiln-bench-e2e: {e}");
            return ExitCode::from(2);
        }
    };
    let (tx, rx) = mpsc::channel::<(u64, Instant)>();
    let rx = Arc::new(Mutex::new(rx));
    let t0 = Instant::now() + Duration::from_millis(200);
    let warm = t0 + Duration::from_secs_f64(a.warmup);
    let workers: Vec<_> = (0..a.workers)
        .map(|_| {
            let (a, rx) = (Arc::clone(&a), Arc::clone(&rx));
            std::thread::spawn(move || {
                let mut seen = Seen::default();
                loop {
                    let job = match rx.lock() {
                        Ok(r) => r.recv(),
                        Err(_) => break,
                    };
                    let Ok((_, intended)) = job else { break };
                    one(&a, intended, intended >= warm, &mut seen);
                }
                seen
            })
        })
        .collect();
    // The schedule: ask i is due at t0 + i / rate, sent then however the earlier ones are doing.
    let total = ((a.warmup + a.seconds) * a.rate).round() as u64;
    let mut late = Hist::default();
    for i in 0..total {
        let due = t0 + Duration::from_secs_f64(i as f64 / a.rate);
        let now = Instant::now();
        if due > now {
            std::thread::sleep(due - now);
        }
        late.add(Instant::now().saturating_duration_since(due));
        if tx.send((i, due)).is_err() {
            break;
        }
    }
    drop(tx);
    let mut all = Seen::default();
    for w in workers {
        if let Ok(s) = w.join() {
            all.answer.merge(&s.answer);
            all.started.merge(&s.started);
            all.refused += s.refused;
            all.failed += s.failed;
            all.measured += s.measured;
            all.why.extend(s.why.into_iter().take(8usize.saturating_sub(all.why.len())));
        }
    }
    let ran = t0.elapsed().as_secs_f64() - a.warmup;
    let json = format!(
        "{{\"who\":\"kiln-e2e\",\"target\":\"{}\",\"rate\":{},\"seconds\":{},\"warmup\":{},\"workers\":{},\"measured\":{},\"achieved_per_s\":{:.1},\"refused\":{},\"failed\":{},\"answer\":{},\"started\":{},\"send_lateness\":{},\"why\":{:?}}}",
        a.target,
        a.rate,
        a.seconds,
        a.warmup,
        a.workers,
        all.measured,
        if ran > 0.0 { all.measured as f64 / ran } else { 0.0 },
        all.refused,
        all.failed,
        all.answer.json(),
        all.started.json(),
        late.json(),
        all.why
    );
    println!("{json}");
    if let Some(p) = &a.json
        && std::fs::write(p, format!("{json}\n")).is_err()
    {
        eprintln!("kiln-bench-e2e: cannot write {p}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;
    use std::os::unix::net::UnixListener;

    /// What the fake socket says: `reach::local::Ev`'s first variants, by position.
    #[derive(Serialize)]
    enum Say {
        _Status(String),
        _Runners(String),
        Picked { runner: String, why: String },
        _Syncing { files: u64, bytes: u64 },
        _Synced { root: String, sent: u32, removed: u32, bytes: u64, ms: u64 },
        Started { runner: String, pid: u32, cwd: String },
    }

    /// A fake reach socket that answers each run with Picked after `pick`, Started after `start`:
    /// the generator's latencies come out at least that, from the intended send time.
    #[test]
    fn lb14_open_loop_measures_from_intended_time() -> Result<(), String> {
        let dir = std::env::var_os("CARGO_TARGET_TMPDIR").map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let path = dir.join(format!("e2e-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let l = UnixListener::bind(&path).map_err(|e| e.to_string())?;
        std::thread::spawn(move || {
            for s in l.incoming().take(3) {
                let Ok(mut s) = s else { continue };
                let mut b = [0u8; 4096];
                let _ = s.read(&mut b);
                std::thread::sleep(Duration::from_millis(2));
                let _ = s.write_all(&proto::frame(&Say::Picked { runner: "b".into(), why: "room".into() }).unwrap_or_default());
                std::thread::sleep(Duration::from_millis(3));
                let _ = s.write_all(&proto::frame(&Say::Started { runner: "b".into(), pid: 1, cwd: "/".into() }).unwrap_or_default());
            }
        });
        let a = Args { socket: path.to_string_lossy().into_owned(), target: "b".into(), rate: 1.0, seconds: 1.0, warmup: 0.0, workers: 1, command: "true".into(), json: None };
        let mut seen = Seen::default();
        // Intended 10 ms ago: the generator counts that wait too (no coordinated omission).
        let intended = Instant::now() - Duration::from_millis(10);
        one(&a, intended, true, &mut seen);
        let _ = std::fs::remove_file(&path);
        if seen.measured != 1 || seen.failed != 0 || seen.refused != 0 {
            return Err(format!("measured {} failed {} refused {} {:?}", seen.measured, seen.failed, seen.refused, seen.why));
        }
        let (ans, st) = (seen.answer.at(1.0), seen.started.at(1.0));
        if ans < 12_000_000 || st < 15_000_000 || st < ans {
            return Err(format!("answer {ans} ns, started {st} ns"));
        }
        Ok(())
    }
}
