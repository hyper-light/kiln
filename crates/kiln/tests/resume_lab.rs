//! Plan step 5 of "No crash or reconnect replays a whole session" (docs/wire.md, "Revisions" and
//! "Restarts"): the lab. Three agent sessions stream on a runner whose link is cut every so often,
//! watched by four viewers, one of which stops reading long enough to fall behind the copy. A viewer drops its socket
//! and comes back; the runner is killed (kill -9) and started again; then so is the server. At the
//! end a checker holds the server's copies and every viewer to each line once, in order. The lab
//! prints what each fault cost in bytes, each within its gap plus 4 KiB a session.
//!
//! The harness is a stand-in for Claude Code (`tests/fixtures/fake-claude`): it calls no model.
//!
//! `cargo test -p kiln --features chaos --test resume_lab --release`

#![cfg(all(unix, feature = "chaos"))]

mod common;

use std::collections::BTreeMap;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use common::agents::{self, Talk};
use common::Options;
use serde_json::Value;

const LINES: u64 = 2000;

/// What a viewer's socket brought: a text frame, or its end.
enum Got {
    Text(Vec<u8>),
    Ended(String),
}

/// A viewer of one session, keeping what it shows as the UI's session store does.
struct Viewer {
    name: &'static str,
    session: String,
    rx: Option<mpsc::Receiver<Got>>,
    socket: Option<std::net::TcpStream>,
    born: u64,
    rev: u64,
    events: BTreeMap<String, Value>,
    /// After how many frames it stops reading, and for how long: long enough to fall behind the
    /// copy's broadcast, which it's then caught up from in-band (a `resumed` on its socket).
    stall: Option<(usize, Duration)>,
    /// Catch-ups it got without reconnecting.
    caught_up: usize,
    /// Frames since it last connected.
    frames: usize,
    /// Bytes its socket brought since it last connected: all of them, and those besides the
    /// events and the newest state.
    bytes: usize,
    beyond: usize,
    /// How many times it connected, whether it last got the session whole, why each of its
    /// connections ended, and how its tries to connect again failed (how many, the last's why).
    connects: usize,
    whole: bool,
    ends: Vec<String>,
    failed: (usize, String),
    /// The kinds of the last frames it got, for what a connection's end says.
    last: std::collections::VecDeque<String>,
}

impl Viewer {
    fn new(name: &'static str, session: &str, stall: Option<(usize, Duration)>) -> Self {
        Self { name, session: session.to_owned(), rx: None, socket: None, born: 0, rev: 0, events: BTreeMap::new(), stall, caught_up: 0, frames: 0, bytes: 0, beyond: 0, connects: 0, whole: false, ends: Vec::new(), failed: (0, String::new()), last: std::collections::VecDeque::new() }
    }

    /// Connects, saying what it has (`?after=`) once it has had some of the copy.
    fn connect(&mut self, port: u16, admin: &str) -> anyhow::Result<()> {
        let after = if self.born != 0 { format!("?after={}.{}", self.born, self.rev) } else { String::new() };
        let mut s = agents::socket(port, admin, &format!("/api/harness/sessions/{}/ws{after}", self.session))?;
        if self.stall.is_some() {
            // A small window, so falling behind reaches the server soon.
            let _ = nix::sys::socket::setsockopt(&s, nix::sys::socket::sockopt::RcvBuf, &4096);
        }
        self.socket = s.try_clone().ok();
        let (tx, rx) = mpsc::channel();
        // Once, on its first connection only.
        let stall = self.stall.take();
        std::thread::spawn(move || {
            let mut n = 0usize;
            loop {
                match agents::frame(&mut s) {
                    Ok((1, payload)) => {
                        if tx.send(Got::Text(payload)).is_err() {
                            return;
                        }
                        n += 1;
                        if let Some((after, d)) = stall
                            && n == after
                        {
                            std::thread::sleep(d);
                            // Fallen behind: from now on it reads as a viewer does.
                            let _ = nix::sys::socket::setsockopt(&s, nix::sys::socket::sockopt::RcvBuf, &(1 << 20));
                        }
                    }
                    Ok((8, payload)) => {
                        let _ = tx.send(Got::Ended(format!("closed by the server: {}", String::from_utf8_lossy(&payload))));
                        return;
                    }
                    Err(e) => {
                        let _ = tx.send(Got::Ended(format!("{e:#}")));
                        return;
                    }
                    Ok(_) => {}
                }
            }
        });
        self.rx = Some(rx);
        self.bytes = 0;
        self.beyond = 0;
        self.frames = 0;
        self.whole = false;
        self.connects += 1;
        Ok(())
    }

    /// Its socket ends, as a page reload or a lost Wi-Fi would end it.
    fn drop_socket(&mut self) {
        if let Some(s) = self.socket.take() {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
    }

    /// Takes in what its socket brought; reconnects once it ended.
    fn pump(&mut self, port: u16, admin: &str) -> anyhow::Result<()> {
        let mut ended = false;
        let got: Vec<Got> = self.rx.as_ref().map(|rx| std::iter::from_fn(|| rx.try_recv().ok()).collect()).unwrap_or_default();
        for g in got {
            match g {
                Got::Text(payload) => {
                    self.bytes += payload.len();
                    let f: Value = serde_json::from_slice(&payload)?;
                    self.apply(&f, payload.len());
                }
                Got::Ended(why) => {
                    self.ends.push(format!("{why}, after {} frames, the last {:?}", self.frames, self.last));
                    ended = true;
                }
            }
        }
        if ended || self.rx.is_none() {
            self.rx = None;
            // The server may be starting again: a refused connection is tried again next time.
            if let Err(e) = self.connect(port, admin) {
                self.failed = (self.failed.0 + 1, format!("{e:#}"));
            }
        }
        Ok(())
    }

    fn apply(&mut self, f: &Value, len: usize) {
        let t = f.get("t").and_then(Value::as_str).unwrap_or_default();
        match t {
            "open" => {
                self.events.clear();
                self.born = f.get("born").and_then(Value::as_u64).unwrap_or(self.born);
                self.whole = true;
            }
            "resumed" => {
                self.whole = false;
                // Mid-socket, not as it connected: it fell behind and was caught up.
                if self.frames > 0 {
                    self.caught_up += 1;
                }
            }
            "reset" => self.events.clear(),
            "events" => {
                for e in f.get("events").and_then(Value::as_array).into_iter().flatten() {
                    if let Some(id) = e.get("id").and_then(Value::as_str) {
                        self.events.insert(id.to_owned(), e.clone());
                    }
                }
            }
            _ => {}
        }
        if !matches!(t, "events" | "state" | "state_patch" | "meta" | "changes") {
            self.beyond += len;
        }
        self.frames += 1;
        self.last.push_back(t.to_owned());
        while self.last.len() > 4 {
            self.last.pop_front();
        }
        self.rev = self.rev.max(f.get("rev").and_then(Value::as_u64).unwrap_or(0));
    }
}

/// The stand-in's lines in `events`, by the process and turn that said them, in the order the
/// copy holds them (an event's id is `msg_<pid>_<turn>_<line>:0`); and any line said twice.
fn turns(events: &[Value]) -> BTreeMap<(u64, u64), Vec<u64>> {
    let mut out: BTreeMap<(u64, u64), Vec<u64>> = BTreeMap::new();
    for id in events.iter().filter_map(|e| e.get("id").and_then(Value::as_str)) {
        let Some(rest) = id.strip_prefix("msg_").and_then(|r| r.strip_suffix(":0")) else { continue };
        let n: Vec<u64> = rest.split('_').filter_map(|p| p.parse().ok()).collect();
        if let [pid, turn, line] = n.as_slice() {
            out.entry((*pid, *turn)).or_default().push(*line);
        }
    }
    out
}

/// A field of a log line (`key=value`), as a number.
fn field(line: &str, key: &str) -> Option<usize> {
    line.split_whitespace().find_map(|w| w.strip_prefix(&format!("{key}="))).and_then(|v| v.trim_matches('"').parse().ok())
}

/// The copy's events of session `id`, as a viewer attaching now gets them.
fn copy(port: u16, admin: &str, id: &str) -> anyhow::Result<Vec<Value>> {
    let (frames, _) = agents::attach(port, admin, id, "")?;
    Ok(agents::events(&frames))
}

/// Whether each of `ids`' turns has ended with its newest turn's every line in its copy.
fn settled(port: u16, admin: &str, ids: &[String]) -> anyhow::Result<bool> {
    for id in ids {
        let (_, info) = common::http(port, "GET", &format!("/api/harness/sessions/{id}"), admin, None)?;
        let info: Value = serde_json::from_str(&info).unwrap_or_default();
        if info.pointer("/last_turn/how").and_then(Value::as_str) != Some("done") {
            return Ok(false);
        }
        let said = turns(&copy(port, admin, id)?);
        if !said.values().any(|l| l.len() as u64 == LINES) {
            return Ok(false);
        }
    }
    Ok(true)
}

#[test]
fn every_frame_once_in_order_through_every_fault() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    // Its sessions' live feeds keep 64 frames for their viewers: one that stops reading soon falls
    // behind.
    let mut s = common::start_env(dir.path(), &Options { agent: false, ci: "", extra: "" }, false, &[("KILN_CHAOS", "fanout=64")])?;
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work)?;
    // Its link cut for 3 s every 9 s, throughout.
    let talk = Talk { lines: LINES as usize, every_ms: 5 };
    let (mut runner, log) = agents::runner(dir.path(), s.port, "lab-runner", Some("drop=9s,down=3s"), talk)?;
    agents::connected(s.port, &s.admin, "lab-runner", &log, Duration::from_secs(120))?;
    let mut ids = Vec::new();
    for _ in 0..3 {
        let w = work.join(format!("s{}", ids.len()));
        std::fs::create_dir_all(&w)?;
        ids.push(agents::open_session(s.port, &s.admin, "lab-runner", &w, Duration::from_secs(90))?);
    }
    let (first, second, third) = match ids.as_slice() {
        [a, b, c] => (a.clone(), b.clone(), c.clone()),
        _ => anyhow::bail!("three sessions: {ids:?}"),
    };
    let mut viewers = vec![
        Viewer::new("v1", &first, None),
        Viewer::new("v2 (stalls)", &first, Some((50, Duration::from_secs(14)))),
        Viewer::new("v3", &second, None),
        Viewer::new("v4", &third, None),
    ];
    for v in &mut viewers {
        v.connect(s.port, &s.admin)?;
    }
    let runner_log = || std::fs::read_to_string(&log).unwrap_or_default();
    let mut costs: Vec<String> = Vec::new();
    let pump = |viewers: &mut Vec<Viewer>, port: u16, admin: &str, for_: Duration| -> anyhow::Result<()> {
        let until = Instant::now() + for_;
        while Instant::now() < until {
            for v in viewers.iter_mut() {
                v.pump(port, admin)?;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Ok(())
    };

    // A viewer drops its socket and comes back, saying what it had.
    pump(&mut viewers, s.port, &s.admin, Duration::from_secs(2))?;
    if let Some(v) = viewers.first_mut() {
        v.drop_socket();
    }
    pump(&mut viewers, s.port, &s.admin, Duration::from_secs(2))?;
    let v = viewers.first().ok_or_else(|| anyhow::anyhow!("no viewers"))?;
    if v.whole || v.connects != 2 {
        let about = |file: &std::path::Path| std::fs::read_to_string(file).map(|l| { let all: Vec<&str> = l.lines().collect(); all.iter().rev().take(150).rev().map(|x| x.chars().take(260).collect::<String>()).collect::<Vec<_>>().join("\n") }).unwrap_or_default();
        eprintln!("--- the server about {}:\n{}", v.session, about(&dir.path().join("server.log")));
        eprintln!("--- the runner about {}:\n{}", v.session, about(&log));
    }
    assert!(!v.whole && v.connects == 2, "{} came back to {} after {} connections ({} bytes, rev {} of {}), which ended: {:?}", v.name, if v.whole { "the session whole" } else { "a resume" }, v.connects, v.bytes, v.rev, v.born, v.ends);
    costs.push(format!("{} reconnected: {} bytes, {} of them beyond the events changed since and the newest state", v.name, v.bytes, v.beyond));
    assert!(v.beyond <= 4096, "{}", costs.last().cloned().unwrap_or_default());

    // The stalled viewer falls behind its copy's broadcast, and is caught up in-band.
    let stalled = |viewers: &[Viewer]| viewers.get(1).map_or(0, |v| v.caught_up);
    let deadline = Instant::now() + Duration::from_secs(60);
    while stalled(&viewers) == 0 && Instant::now() < deadline {
        pump(&mut viewers, s.port, &s.admin, Duration::from_millis(500))?;
    }
    assert!(stalled(&viewers) > 0, "the viewer that stopped reading wasn't caught up in-band");

    // The runner killed mid-turn and started again on its state.
    let upto = runner_log().len();
    runner.0.kill()?;
    runner.0.wait()?;
    pump(&mut viewers, s.port, &s.admin, Duration::from_secs(1))?;
    let (again, _) = agents::runner(dir.path(), s.port, "lab-runner", Some("drop=9s,down=3s"), talk)?;
    drop(runner);
    let runner_kept = again;
    pump(&mut viewers, s.port, &s.admin, Duration::from_secs(8))?;
    let starts = |from: usize| -> Vec<String> {
        runner_log().get(from..).unwrap_or_default().lines().filter(|l| l.contains("updates start on the channel") && ids.iter().any(|id| l.contains(id))).map(str::to_owned).collect()
    };
    let after_runner = starts(upto);
    assert!(!after_runner.iter().any(|l| l.contains("on the channel, whole")), "a session went whole after the runner started again:\n{}", after_runner.join("\n"));
    for l in &after_runner {
        let beyond = field(l, "beyond").unwrap_or(usize::MAX);
        costs.push(format!("runner restart: a stream started again, {beyond} bytes beyond its events after the copy's and its newest state"));
        assert!(beyond <= 4096, "{l}");
    }

    // The server killed mid-turn and started again on its data.
    let upto = runner_log().len();
    s.restart()?;
    pump(&mut viewers, s.port, &s.admin, Duration::from_secs(10))?;
    let after_server = starts(upto);
    assert!(!after_server.iter().any(|l| l.contains("on the channel, whole")), "a session went whole after the server started again:\n{}", after_server.join("\n"));
    for l in &after_server {
        let beyond = field(l, "beyond").unwrap_or(usize::MAX);
        costs.push(format!("server restart: a stream started again, {beyond} bytes beyond its events after the copy's and its newest state"));
        assert!(beyond <= 4096, "{l}");
    }

    // Once every session's turns have ended: the copies, and every viewer as it caught up.
    let deadline = Instant::now() + Duration::from_secs(300);
    while !settled(s.port, &s.admin, &ids)? {
        anyhow::ensure!(Instant::now() < deadline, "the sessions never settled\nrunner:\n{}\nserver:\n{}", runner_log(), s.log());
        pump(&mut viewers, s.port, &s.admin, Duration::from_secs(1))?;
    }
    let mut lost = 0usize;
    let mut doubled = 0usize;
    for id in &ids {
        let events = copy(s.port, &s.admin, id)?;
        let mut seen = std::collections::HashSet::new();
        doubled += events.iter().filter_map(|e| e.get("id").and_then(Value::as_str)).filter(|i| !seen.insert(i.to_owned())).count();
        let seqs: Vec<u64> = events.iter().filter_map(|e| e.get("seq").and_then(Value::as_u64)).collect();
        assert!(seqs.windows(2).all(|w| w.first() < w.get(1)), "the copy of {id} out of order");
        let said = turns(&events);
        for (turn, lines) in &said {
            // Each turn's lines once, in order, one after another: a turn the runner's restart cut
            // ends early, and the oldest may begin past its first lines, the copy keeping a
            // bounded window of the timeline.
            let start = lines.first().copied().unwrap_or(0);
            assert_eq!(*lines, (start..start + lines.len() as u64).collect::<Vec<_>>(), "turn {turn:?} of {id} isn't each line once, in order");
        }
        assert!(said.values().any(|l| l.len() as u64 == LINES && l.first() == Some(&0)), "a turn of {id}, whole");
        // Each of its viewers caught up to it: everything the copy has, and nothing newer it lacks
        // (a viewer keeps what the copy let go of past its window).
        let want: BTreeMap<String, Value> = events.iter().filter_map(|e| Some((e.get("id")?.as_str()?.to_owned(), e.clone()))).collect();
        let oldest = events.iter().filter_map(|e| e.get("seq").and_then(Value::as_u64)).min().unwrap_or(0);
        let off = |v: &Viewer| -> (usize, usize) {
            let missing = want.iter().filter(|(k, e)| v.events.get(*k) != Some(*e)).count();
            let extra = v.events.iter().filter(|(k, e)| !want.contains_key(*k) && e.get("seq").and_then(Value::as_u64).is_some_and(|q| q >= oldest)).count();
            (missing, extra)
        };
        let deadline = Instant::now() + Duration::from_secs(90);
        while viewers.iter().any(|v| v.session == *id && off(v) != (0, 0)) && Instant::now() < deadline {
            pump(&mut viewers, s.port, &s.admin, Duration::from_millis(500))?;
        }
        for v in viewers.iter().filter(|v| v.session == *id) {
            let (missing, extra) = off(v);
            lost += missing;
            doubled += extra;
            if (missing, extra) != (0, 0) {
                eprintln!(
                    "{} never caught up to the copy of {id}: {missing} events missing or behind, {extra} it shouldn't have; connected {} times, its connections ended: {:?}; {} tries to connect again failed, the last: {}; {} frames on its socket now, the last {:?}",
                    v.name, v.connects, v.ends, v.failed.0, v.failed.1, v.frames, v.last
                );
            }
        }
    }
    for v in &viewers {
        costs.push(format!("{}: connected {} times, caught up in-band {} times", v.name, v.connects, v.caught_up));
    }
    for c in &costs {
        eprintln!("{c}");
    }
    eprintln!("frames lost: {lost}, doubled: {doubled}, over {} viewers and {} copies", viewers.len(), ids.len());
    let cuts = runner_log().matches("chaos: dropping the connection to the server").count();
    assert!(cuts >= 2, "the link was cut {cuts} times; the lab needs it cut while the sessions stream");
    assert_eq!((lost, doubled), (0, 0), "every frame once, in order");
    drop(runner_kept);
    Ok(())
}
