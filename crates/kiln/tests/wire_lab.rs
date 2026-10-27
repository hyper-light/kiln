//! Plan step 9 of "Nothing kiln sends between its processes is fire-and-forget" (docs/wire.md,
//! "The lab"): the wire lab. A server and three runners, kiln's real net layer between them, a
//! hundred agent sessions streaming, and ten thousand commands from the API and from viewers'
//! sockets, through faults:
//! - each runner's link cut every few seconds;
//! - each side's channel frames dropped, sent twice and sent late;
//! - each runner killed (kill -9) and started again, then the server;
//! - viewers' sockets dropped and opened again, as a page reloads.
//!
//! The sessions all open before any streams (`FAKE_CLAUDE_GO`), then stream a first turn of
//! `WIRE_LAB_LINES` lines each while the commands come. A checker then holds every command to one
//! answer and to one application at the most: exactly one when it was answered taken, accepted or
//! parked, none when it was refused. It holds every update to the server's copy once and in order,
//! and each viewer to its copy. It prints its counts and its seed, and at full scale needs 200
//! faults and 100,000 updates: with frames made unreadable (below), 50,000, and 10,000 of those
//! frames come.
//!
//! The commands are the UI's:
//! - models to change to, live commands refused while their runner is away, over the API and on
//!   sockets;
//! - messages and skill runs under their ids, on sockets, which their sessions' inboxes take once.
//!
//! A runner isn't killed within a moment of a command it was sent, from the API or a socket, nor
//! while a model change it was sent is unanswered or within a moment of one's answer (frames made
//! unreadable can hold one up for seconds after it was sent), nor within a moment of one of its
//! sessions handing its harness a message that waited in its queue.
//! Its record of what its inbox took and of its sessions' queues is written half a second after it
//! changes: a crash before then loses a message (CP-T3.1's journal closes that), or hands one again
//! that its session had already handed its harness (CP-T3.5's). What its session took and its
//! harness hadn't yet applied goes with it too. Nor is the server killed with a socket command it
//! hasn't answered: sent again to the server that starts again, which doesn't have its id, a model
//! would change twice (docs/wire.md, "The UI's sockets"). For the same reason an API call the
//! server's death cut off isn't asked again.
//!
//! Its seed (`WIRE_LAB_SEED`, 1 by default) picks the frames the faults hit and when the rest
//! fall. `WIRE_LAB_SESSIONS`, `WIRE_LAB_COMMANDS`, `WIRE_LAB_LINES` and `WIRE_LAB_SECONDS` scale
//! it down for a quick run; `WIRE_LAB_FRAMES` is how rare a frame's fault is (one in 101 dropped
//! by default; 0: none). kiln's CI runs it nightly on more seeds (`.kiln/wire-lab.yaml`).
//!
//! `WIRE_LAB_GARBLE` (0, none, by default) makes frames unreadable too, besides those dropped,
//! doubled and late: about one in that many of each side's channel frames a kind the other doesn't
//! know (`unknown=`), and as many cut short, given bytes after them or mangled (`garble=`;
//! docs/wire.md, "What can't be read"). The channel makes up for each as for a frame lost below it,
//! so the commands and updates are held as before. A frame made unreadable goes marked, and its
//! receiver says it came (`kiln::net`), so the account counts them from the processes' logs, which
//! outlive their kills and the server's restart: the frames made unreadable, those that came (the
//! rest were lost below the channel with their connection), and those refused. Every one that came
//! must have been refused: the server's refusals are what `kiln_wire_refused_total` counts, each
//! said in its log (`kiln::refused`), and a runner says each frame of the server's it couldn't read.
//!
//! `cargo test -p kiln --features chaos --test wire_lab --release`: its account prints as it passes
//! too, and `-- --nocapture` shows its progress as it goes. Without `chaos` it has no faults to
//! drive, and cargo says so (Cargo.toml's `required-features`).

#![cfg(all(unix, feature = "chaos"))]

mod common;

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context as _;

use common::agents::{self, Runner, Talk};
use common::lab::{Viewer, copy, field, turns};
use common::{Options, http};
use serde_json::{Value, json};

const RUNNERS: usize = 3;
const API_THREADS: usize = 4;
/// Sessions with a viewer of their own, which their socket commands go on.
const VIEWED: usize = 25;
/// How long a runner hears no message before it's killed (see above).
const QUIET_BEFORE_KILL: Duration = Duration::from_millis(1500);
/// How long none of a runner's sessions has handed its harness a message before it's killed: its
/// record of their queues is written half a second after it changes, and one killed just after a
/// session handed its harness what waited in its queue hands that again once it starts again
/// (docs/sends.md, CP-T3.5). A turn under way may go on: a kill mid-turn is what the updates are
/// held to.
const HANDED_QUIET: Duration = Duration::from_millis(1500);
/// How long a kill waits for that, at the most.
const HANDED_WAIT: Duration = Duration::from_secs(60);

/// A line of the lab's account, written to stderr as it is: a run that passes shows it without
/// `--nocapture` (libtest keeps what `eprintln!` writes for the runs that fail).
fn say(line: &str) {
    use std::io::Write as _;
    let _ = std::io::stderr().write_all(format!("{line}\n").as_bytes());
}

/// A knob from the environment.
fn knob(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

/// When the lab's commands and faults fall, and where: xorshift64*, from the lab's seed.
struct Draw(u64);

impl Draw {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15).max(1))
    }

    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        usize::try_from(self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) % u64::try_from(n.max(1)).unwrap_or(1)).unwrap_or(0)
    }
}

/// How a command went.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum How {
    /// A model to change to, over the API.
    ApiModel,
    /// A model to change to, on a viewer's socket under an id.
    SocketModel,
    /// A message under its id, on a viewer's socket.
    SocketMessage,
    /// A skill run under its id, on a viewer's socket.
    SocketSkill,
}

/// What a command was answered.
#[derive(Debug, Clone)]
enum Answered {
    /// The API's status.
    Status(u16),
    /// The API call broke off: the server died under it.
    CutOff,
    /// On viewer `viewer`'s socket under `id`: its answers are the viewer's (`Viewer::asked`).
    Socket { viewer: usize, id: String },
}

/// A command the lab sent: to which session, how, the token its harness gets of it (a model's
/// name, a message's words), and what it was answered.
#[derive(Debug, Clone)]
struct Sent {
    session: usize,
    how: How,
    token: String,
    answer: Answered,
}

/// A session of the lab: its id, its runner, and the files its harness writes what it gets to.
struct Session {
    id: String,
    runner: usize,
    controls: PathBuf,
    messages: PathBuf,
}

/// Session `id` as the server has it: whether a turn runs, and how many messages wait their turn
/// (none: the server too busy to say).
fn work(port: u16, admin: &str, id: &str) -> Option<(bool, u64)> {
    let (_, info) = http(port, "GET", &format!("/api/harness/sessions/{id}"), admin, None).ok()?;
    let info: Value = serde_json::from_str(&info).ok()?;
    let running = matches!(info.get("status").and_then(Value::as_str), Some("running" | "starting" | "waiting"));
    Some((running, info.get("queued").and_then(Value::as_u64).unwrap_or(0)))
}

/// What a session's copy holds of its turns (`turns`): its lines, those it has twice, and what's
/// wrong with it (each an update lost): out of order, a turn's lines not each once in order, or its
/// first turn not from its first line, or not whole though its process ran on. A runner's kill that
/// ended its process may have ended its first turn short. Whether a process still runs is asked of
/// `alive` (by its pid: this machine's).
struct Judged {
    updates: usize,
    doubled: usize,
    lost: Vec<String>,
}

fn judge(events: &[Value], lines: u64, id: &str, alive: impl Fn(u64) -> bool) -> Judged {
    let mut lost = Vec::new();
    let mut seen = HashSet::new();
    let doubled = events.iter().filter_map(|e| e.get("id").and_then(Value::as_str)).filter(|i| !seen.insert(i.to_owned())).count();
    let seqs: Vec<u64> = events.iter().filter_map(|e| e.get("seq").and_then(Value::as_u64)).collect();
    if !seqs.windows(2).all(|w| w.first() < w.get(1)) {
        lost.push(format!("the copy of {id} out of order"));
    }
    let ran = turns(events);
    let mut updates = 0;
    for (turn, said) in &ran {
        updates += said.len();
        let start = said.first().copied().unwrap_or(0);
        if *said != (start..start + said.len() as u64).collect::<Vec<_>>() {
            lost.push(format!("turn {turn:?} of {id} isn't each line once, in order"));
        }
    }
    let first_pid = events.iter().filter_map(|e| e.get("id")?.as_str()?.strip_prefix("msg_")?.split('_').next()?.parse::<u64>().ok()).next();
    let first = first_pid.and_then(|pid| ran.get(&(pid, 1)));
    let ran_on = first_pid.is_some_and(&alive);
    if !first.is_some_and(|l| l.first() == Some(&0) && (!ran_on || l.len() as u64 == lines)) {
        lost.push(format!("the first turn of {id} isn't whole, its process {}: {:?}", if ran_on { "running still" } else { "gone" }, ran.iter().map(|(k, l)| (*k, l.len())).collect::<Vec<_>>()));
    }
    Judged { updates, doubled, lost }
}

/// Whether process `pid`, a harness this lab's runners started on this machine, still runs.
fn alive(pid: u64) -> bool {
    i32::try_from(pid).is_ok_and(|p| nix::sys::signal::kill(nix::unistd::Pid::from_raw(p), None).is_ok())
}

/// Whether session `id` is at work: a turn running, or messages waiting their turn.
fn at_work(port: u16, admin: &str, id: &str) -> bool {
    work(port, admin, id).is_none_or(|(running, queued)| running || queued > 0)
}

impl Session {
    /// How many times its harness got `token`, a command's.
    fn applied(&self, how: How, token: &str) -> usize {
        let file = if matches!(how, How::ApiModel | How::SocketModel) { &self.controls } else { &self.messages };
        std::fs::read_to_string(file).unwrap_or_default().lines().filter(|l| l.contains(token)).count()
    }
}

/// What must have come of a command, as it was answered: applied exactly once, never, or once at
/// the most (its answer can't say: its link dropped before it, or the server died under it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Must {
    Once,
    Never,
    AtMostOnce,
}

/// What a viewer's answer to a command says must have come of it, and its words.
fn must_of(answer: &Value) -> (Must, String) {
    match (answer.get("t").and_then(Value::as_str), answer.get("outcome").and_then(Value::as_str), answer.get("code").and_then(Value::as_str)) {
        (Some("ack"), Some(o), _) => (Must::Once, format!("ack {o}")),
        (Some("nack"), _, Some("unanswered")) => (Must::AtMostOnce, "nack unanswered".into()),
        (Some("nack"), _, Some(c)) => (Must::Never, format!("nack {c}")),
        _ => (Must::AtMostOnce, format!("{answer}")),
    }
}

/// Starts runner `n` again (or the first time) on its state, with the lab's faults.
/// Each new session's first turn waits for `go` (`FAKE_CLAUDE_GO`), so the sessions are all open
/// before they stream.
fn start_runner(dir: &Path, port: u16, n: usize, seed: u64, lines: &str, go: &Path, (framed, garble): (u64, u64)) -> anyhow::Result<(Runner, PathBuf)> {
    let mut chaos = format!("drop={}s,down=1s", 5 + n);
    if framed != 0 || garble != 0 {
        chaos = format!("{chaos},{}", frames("term", 46, seed + 1 + u64::try_from(n)?, framed, garble));
    }
    let go = go.to_string_lossy();
    agents::runner_with(dir, port, &format!("lab-{n}"), Some(&chaos), Talk { lines: 1, every_ms: 10 }, &[("FAKE_CLAUDE_FIRST_LINES", lines), ("FAKE_CLAUDE_GO", &go)])
}

/// The faults a side's channel frames go through (`crate::runner::chaos`), of `lane`'s frames of
/// message `variant`: about one in `every` dropped, one in `every` × 7/11 sent twice and one in
/// `every` × 5/11 sent late. At one in 11 dropped, the channel's repairs (an item's resend after a
/// gap, or after its patience) take most of its time. With `garble`, about one in that many made a
/// kind the receiver doesn't know, and as many garbled (none: 0).
fn frames(lane: &str, variant: u8, seed: u64, every: u64, garble: u64) -> String {
    let (twice, late) = ((every * 7 / 11).max(2), (every * 5 / 11).max(2));
    let mut faults = String::new();
    if every != 0 {
        faults = format!("shed={lane}/{variant}:{every},twice={lane}/{variant}:{twice},late={lane}/{variant}:{late},");
    }
    if garble != 0 {
        faults.push_str(&format!("unknown={lane}/{variant}:{garble},garble={lane}/{variant}:{garble},"));
    }
    format!("{faults}seed={seed}")
}

/// What the lab's processes said of the frames made unreadable, from their logs (which outlive
/// their kills and the server's restart): how many each side made (a copy sent twice counts twice),
/// how many came, and how many their readers refused.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Unreadable {
    made: usize,
    came: usize,
    refused: usize,
}

impl Unreadable {
    fn of(logs: &[PathBuf]) -> Unreadable {
        let mut u = Unreadable::default();
        for log in logs {
            for line in std::fs::read_to_string(log).unwrap_or_default().lines() {
                if line.contains("chaos: frames made unreadable") {
                    u.made += field(line, "copies").unwrap_or(1);
                } else if line.contains("chaos: an unreadable frame came") {
                    u.came += 1;
                } else if line.contains("a frame from the server couldn't be read") || (line.contains("refused what it couldn't read") && (line.contains("wire=\"runner\"") || line.contains("wire=runner "))) {
                    // A runner's of the server's frames; the server's of a runner's (`kiln::refused`,
                    // where `kiln_wire_refused_total` counts them).
                    u.refused += 1;
                }
            }
        }
        u
    }
}

#[test]
fn every_command_once_and_every_update_once_in_order_through_every_fault() -> anyhow::Result<()> {
    let seed = knob("WIRE_LAB_SEED", 1);
    let sessions_n = usize::try_from(knob("WIRE_LAB_SESSIONS", 100))?;
    let commands_n = usize::try_from(knob("WIRE_LAB_COMMANDS", 10_000))?;
    let lines = knob("WIRE_LAB_LINES", 2000);
    let run_for = Duration::from_secs(knob("WIRE_LAB_SECONDS", 180));
    // About one frame in this many dropped on each side, and some doubled and late (0: none, to
    // compare).
    let framed = knob("WIRE_LAB_FRAMES", 101);
    // About one frame in this many made unreadable on each side, and as many garbled (0: none).
    let garble = knob("WIRE_LAB_GARBLE", 0);
    let faulted = (framed, garble);
    let lines_s = lines.to_string();
    let mut draw = Draw::new(seed);
    let dir = tempfile::tempdir()?;
    let go = dir.path().join("go");
    let server_faults = if framed != 0 || garble != 0 { frames("control", 54, seed, framed, garble) } else { String::new() };
    let mut chaos_env: Vec<(&str, &str)> = if server_faults.is_empty() { Vec::new() } else { vec![("KILN_CHAOS", server_faults.as_str())] };
    // Each refusal the server notes, in its log (`kiln::refused`), for the account.
    if garble != 0 {
        chaos_env.push(("KILN_LOG", "info,kiln::refused=debug"));
    }
    let mut s = common::start_env(dir.path(), &Options { agent: false, ci: "", extra: "" }, false, &chaos_env)?;
    let mut runners = Vec::new();
    for n in 0..RUNNERS {
        runners.push(start_runner(dir.path(), s.port, n, seed, &lines_s, &go, faulted)?);
    }
    for (n, (_, log)) in runners.iter().enumerate() {
        agents::connected(s.port, &s.admin, &format!("lab-{n}"), log, Duration::from_secs(120))?;
    }

    // The sessions, over the runners in turn, opened ten at a time; none streams until all are.
    let work = dir.path().join("work");
    let mut sessions: Vec<Session> = Vec::new();
    let (opening, mut slowest) = (Instant::now(), Duration::ZERO);
    for chunk in (0..sessions_n).collect::<Vec<_>>().chunks(10) {
        let opened: Vec<std::thread::JoinHandle<anyhow::Result<(Session, Duration)>>> = chunk
            .iter()
            .map(|&i| {
                let (port, admin, w) = (s.port, s.admin.clone(), work.join(format!("s{i:03}")));
                std::thread::spawn(move || {
                    let t = Instant::now();
                    std::fs::create_dir_all(&w)?;
                    let (controls, messages) = (w.join(".fake-claude-controls"), w.join(".fake-claude-messages"));
                    std::fs::write(&controls, "")?;
                    std::fs::write(&messages, "")?;
                    let id = agents::open_session(port, &admin, &format!("lab-{}", i % RUNNERS), &w, Duration::from_secs(180)).with_context(|| format!("opening session {i} on lab-{}", i % RUNNERS))?;
                    Ok((Session { id, runner: i % RUNNERS, controls, messages }, t.elapsed()))
                })
            })
            .collect();
        for t in opened {
            let (session, took) = t.join().map_err(|_| anyhow::anyhow!("an open panicked"))??;
            slowest = slowest.max(took);
            sessions.push(session);
        }
    }
    let mut viewers: Vec<Viewer> = sessions.iter().take(VIEWED).enumerate().map(|(i, x)| Viewer::new(format!("v{i}"), &x.id, None)).collect();
    for v in &mut viewers {
        v.connect(s.port, &s.admin).with_context(|| format!("viewer {} connecting", v.name))?;
    }
    println!("wire lab: {} sessions open in {:.1?} (the slowest {:.1?}), {} viewers: go", sessions.len(), opening.elapsed(), slowest, viewers.len());
    std::fs::write(&go, "")?;

    // Over the API: models to change to, on threads of their own, paced over the run.
    let api_n = commands_n / 2;
    let api_sent: Arc<Mutex<Vec<Sent>>> = Arc::new(Mutex::new(Vec::new()));
    let ids: Vec<(String, usize)> = sessions.iter().map(|x| (x.id.clone(), x.runner)).collect();
    // The runner about to be killed (none: usize::MAX): it hears no command for a moment first,
    // from the API or a socket. What it took and hadn't yet applied would go with it.
    let quiet_runner = Arc::new(AtomicUsize::new(usize::MAX));
    // Each runner's model changes from the API that are on their way, and how many have been
    // answered: it isn't killed while one is unanswered, nor a moment after one's answer.
    let api_open: Arc<Vec<AtomicUsize>> = Arc::new((0..RUNNERS).map(|_| AtomicUsize::new(0)).collect());
    let api_answered: Arc<Vec<AtomicUsize>> = Arc::new((0..RUNNERS).map(|_| AtomicUsize::new(0)).collect());
    let started = Instant::now();
    let api: Vec<std::thread::JoinHandle<()>> = (0..API_THREADS)
        .map(|t| {
            let (port, admin, ids, out, quiet_runner) = (s.port, s.admin.clone(), ids.clone(), Arc::clone(&api_sent), Arc::clone(&quiet_runner));
            let (open, answered) = (Arc::clone(&api_open), Arc::clone(&api_answered));
            let share = api_n / API_THREADS + usize::from(t < api_n % API_THREADS);
            let every = run_for / u32::try_from(share.max(1)).unwrap_or(1);
            std::thread::spawn(move || {
                let mut draw = Draw::new(seed ^ (0x51 + t as u64));
                for k in 0..share {
                    let due = started + every * u32::try_from(k).unwrap_or(0);
                    if let Some(wait) = due.checked_duration_since(Instant::now()) {
                        std::thread::sleep(wait);
                    }
                    let session = loop {
                        let i = draw.below(ids.len());
                        if ids.get(i).is_some_and(|(_, runner)| *runner != quiet_runner.load(Ordering::Relaxed)) {
                            break i;
                        }
                    };
                    let token = format!("m-a{t}-{k:06}");
                    let path = format!("/api/harness/sessions/{}/command", ids.get(session).map_or("", |(id, _)| id.as_str()));
                    let runner = ids.get(session).map_or(usize::MAX, |(_, r)| *r);
                    if let Some(o) = open.get(runner) {
                        o.fetch_add(1, Ordering::SeqCst);
                    }
                    let answer = match http(port, "POST", &path, &admin, Some(&json!({"type": "set_model", "model": token}))) {
                        // No status: the answer broke off as it came.
                        Ok((0, _)) | Err(_) => Answered::CutOff,
                        Ok((st, _)) => Answered::Status(st),
                    };
                    if let (Some(o), Some(a)) = (open.get(runner), answered.get(runner)) {
                        a.fetch_add(1, Ordering::SeqCst);
                        o.fetch_sub(1, Ordering::SeqCst);
                    }
                    if let Ok(mut o) = out.lock() {
                        o.push(Sent { session, how: How::ApiModel, token, answer });
                    }
                }
            })
        })
        .collect();

    // On the viewers' sockets: the rest, beside the faults.
    let socket_n = commands_n - api_n;
    let socket_every = run_for / u32::try_from(socket_n.max(1)).unwrap_or(1);
    let mut socket_sent: Vec<Sent> = Vec::new();
    // Each runner's kill, then the server's, as parts of the run.
    let kills: Vec<(Duration, Option<usize>)> = (0..RUNNERS).map(|n| (run_for.mul_f64(0.2 + 0.2 * n as f64), Some(n))).chain([(run_for.mul_f64(0.85), None)]).collect();
    let mut next_kill = 0;
    let mut quiet: Option<(usize, Instant)> = None;
    // A runner whose kill is due, its sessions watched for what their harnesses are handed and its
    // model changes' answers: since when, when either last changed, and the sizes of their files of
    // messages and how many of its model changes had been answered then.
    let mut handing: Option<(Instant, Instant, (Vec<u64>, usize))> = None;
    let (mut reloads, mut kills_done, mut server_restarts) = (0usize, 0usize, 0usize);
    let mut next_reload = started + Duration::from_millis(500);
    let mut next_said = started + Duration::from_secs(15);
    while socket_sent.len() < socket_n || started.elapsed() < run_for {
        let now = Instant::now();
        // The next kill. A runner hears no message for a moment first; the server, no socket
        // command, and it has answered all of them (see above).
        let mut hold = false;
        if let Some(&(at, who)) = kills.get(next_kill) {
            let when = started + at;
            let near = now + QUIET_BEFORE_KILL >= when;
            match who {
                // However late the loop comes to it, the runner hears no command for a moment
                // before it's killed.
                Some(n) if near && quiet.is_none() => {
                    quiet = Some((n, when.max(now + QUIET_BEFORE_KILL)));
                    quiet_runner.store(n, Ordering::Relaxed);
                }
                Some(n) if quiet.is_some_and(|(_, at)| now >= at) && {
                    // Killed once its sessions' harnesses have been handed nothing for a moment and
                    // every model change it was sent has been answered a moment before, the rest of
                    // the lab going on meanwhile: one it took just before and hadn't applied yet
                    // would go with it. (Its messages are its inboxes', watched by what their
                    // harnesses are handed.)
                    let theirs = |v: &&Viewer| ids.iter().any(|(id, r)| *r == n && *id == v.session);
                    let models = |done: bool| viewers.iter().filter(theirs).map(|v| v.asked.values().filter(|a| !a.message && a.answer().is_some() == done).count()).sum::<usize>();
                    let open = api_open.get(n).map_or(0, |o| o.load(Ordering::SeqCst)) + models(false);
                    let answered = api_answered.get(n).map_or(0, |a| a.load(Ordering::SeqCst)) + models(true);
                    let sizes: Vec<u64> = sessions.iter().filter(|x| x.runner == n).map(|x| std::fs::metadata(&x.messages).map_or(0, |m| m.len())).collect();
                    let state = (sizes, answered);
                    let (since, last, before) = handing.get_or_insert_with(|| (now, now, state.clone()));
                    if *before != state {
                        (*last, *before) = (now, state);
                    }
                    let (handed_quiet, waited) = (now.saturating_duration_since(*last), now.saturating_duration_since(*since));
                    let calm = handed_quiet >= HANDED_QUIET && open == 0;
                    if !calm && waited >= HANDED_WAIT {
                        eprintln!("wire lab: lab-{n} was still being handed messages or answering model changes ({open} unanswered) {} s on: killed anyway", HANDED_WAIT.as_secs());
                    }
                    calm || waited >= HANDED_WAIT
                } => {
                    handing = None;
                    let (mut runner, _) = runners.remove(n);
                    let _ = runner.0.kill();
                    let _ = runner.0.wait();
                    drop(runner);
                    runners.insert(n, start_runner(dir.path(), s.port, n, seed, &lines_s, &go, faulted)?);
                    kills_done += 1;
                    quiet = None;
                    quiet_runner.store(usize::MAX, Ordering::Relaxed);
                    next_kill += 1;
                }
                None if near && now < when => hold = true,
                None if now >= when => {
                    let until = Instant::now() + Duration::from_secs(10);
                    while viewers.iter().map(Viewer::unanswered).sum::<usize>() > 0 && Instant::now() < until {
                        for v in &mut viewers {
                            v.pump(s.port, &s.admin)?;
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    s.restart()?;
                    server_restarts += 1;
                    next_kill += 1;
                }
                _ => {}
            }
        }
        if now >= next_said {
            let seen: usize = viewers.iter().map(|v| v.events.len()).sum();
            let api_so_far = api_sent.lock().map_or(0, |o| o.len());
            eprintln!("wire lab, {:.0?} in: {} commands sent, {seen} events with the {} viewers, {kills_done} runners killed, {reloads} reloads", started.elapsed(), api_so_far + socket_sent.len(), viewers.len());
            next_said = now + Duration::from_secs(15);
        }
        // A viewer's page reloads.
        if now >= next_reload && !viewers.is_empty() {
            let v = draw.below(viewers.len());
            if let Some(v) = viewers.get_mut(v) {
                v.drop_socket();
            }
            reloads += 1;
            next_reload = now + Duration::from_millis(600 + u64::try_from(draw.below(800)).unwrap_or(0));
        }
        // The socket commands due.
        while !hold && socket_sent.len() < socket_n && started + socket_every * u32::try_from(socket_sent.len()).unwrap_or(0) <= now {
            let k = socket_sent.len();
            let viewer = loop {
                let v = draw.below(viewers.len());
                if quiet.is_none_or(|(n, _)| sessions.get(v).is_some_and(|x| x.runner != n)) {
                    break v;
                }
            };
            let session = viewer;
            let how = match draw.below(3) {
                1 => How::SocketMessage,
                2 => How::SocketSkill,
                _ => How::SocketModel,
            };
            let (token, c) = match how {
                How::SocketMessage => {
                    let t = format!("t-s-{k:06}");
                    (t.clone(), json!({"type": "send", "id": format!("lab-msg-{seed}-{k}"), "text": t}))
                }
                How::SocketSkill => {
                    let t = format!("k-s-{k:06}");
                    (t.clone(), json!({"type": "skill_run", "name": "kiln-lab", "args": t, "id": format!("lab-skill-{seed}-{k}")}))
                }
                _ => {
                    let t = format!("m-s-{k:06}");
                    (t.clone(), json!({"type": "set_model", "model": t}))
                }
            };
            let id = format!("lab-cmd-{seed}-{k}");
            if let Some(v) = viewers.get_mut(viewer) {
                v.command(&id, &c);
            }
            socket_sent.push(Sent { session, how, token, answer: Answered::Socket { viewer, id } });
        }
        for v in &mut viewers {
            v.pump(s.port, &s.admin)?;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    for t in api {
        t.join().map_err(|_| anyhow::anyhow!("an API thread panicked"))?;
    }

    // Every socket command answered, every turn ended, every message taken in.
    let deadline = Instant::now() + Duration::from_secs(240);
    loop {
        for v in &mut viewers {
            v.pump(s.port, &s.admin)?;
        }
        let waiting: usize = viewers.iter().map(Viewer::unanswered).sum();
        let busy = sessions.iter().filter(|x| at_work(s.port, &s.admin, &x.id)).count();
        if waiting == 0 && busy == 0 {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "never settled: {waiting} socket commands unanswered, {busy} sessions busy\n{}", s.log());
        std::thread::sleep(Duration::from_millis(500));
    }

    // The commands: each answered, applied as its answer says.
    let mut sent = api_sent.lock().map_err(|_| anyhow::anyhow!("poisoned"))?.clone();
    sent.extend(socket_sent);
    let (mut lost, mut doubled, mut unanswered, mut wrongly) = (0usize, 0usize, 0usize, 0usize);
    let mut answers: BTreeMap<(How, String), usize> = BTreeMap::new();
    let mut odd: Vec<String> = Vec::new();
    let mut lost_tokens: Vec<String> = Vec::new();
    for c in &sent {
        let (must, words) = match &c.answer {
            Answered::Status(st @ (200 | 202 | 204)) => (Must::Once, format!("{st}")),
            Answered::Status(504) => (Must::AtMostOnce, "504".into()),
            Answered::Status(st) => (Must::Never, format!("{st}")),
            Answered::CutOff => (Must::AtMostOnce, "cut off".into()),
            Answered::Socket { viewer, id } => {
                let a = viewers.get(*viewer).and_then(|v| v.asked.get(id));
                match a.and_then(|a| a.answer()) {
                    Some(first) => {
                        if a.is_some_and(|a| a.answers.len() > a.sockets) {
                            doubled += 1;
                            odd.push(format!("{} answered {} times on {} sockets", c.token, a.map_or(0, |a| a.answers.len()), a.map_or(0, |a| a.sockets)));
                        }
                        must_of(first)
                    }
                    None => {
                        unanswered += 1;
                        odd.push(format!("{} never answered", c.token));
                        (Must::AtMostOnce, "unanswered".into())
                    }
                }
            }
        };
        *answers.entry((c.how, words.clone())).or_default() += 1;
        let x = sessions.get(c.session).ok_or_else(|| anyhow::anyhow!("no session {}", c.session))?;
        let n = x.applied(c.how, &c.token);
        match (must, n) {
            (_, n) if n > 1 => {
                doubled += 1;
                odd.push(format!("{} applied {n} times", c.token));
                lost_tokens.push(c.token.clone());
            }
            (Must::Once, 0) => {
                lost += 1;
                odd.push(format!("{} ({:?}, to {}) answered {words}, never applied", c.token, c.how, x.id));
                lost_tokens.push(c.token.clone());
            }
            (Must::Never, 1) => {
                wrongly += 1;
                odd.push(format!("{} ({:?}, to {}) answered {words}, yet applied", c.token, c.how, x.id));
            }
            _ => {}
        }
    }

    // The updates: each session's copy, each turn's lines once and in order, and each viewer as
    // its copy is.
    let (mut updates, mut updates_lost, mut updates_doubled) = (0usize, 0usize, 0usize);
    // What's still on its way may come yet: an update lost below the channel with nothing after it
    // goes again once its sender's patience is up (15 s for a runner's updates).
    let (checking, on_its_way) = (Instant::now(), Instant::now() + Duration::from_secs(45));
    let mut came_late = 0usize;
    for x in &sessions {
        let mut waited = false;
        let (events, judged) = loop {
            let events = copy(s.port, &s.admin, &x.id)?;
            let judged = judge(&events, lines, &x.id, alive);
            if judged.lost.is_empty() || Instant::now() >= on_its_way {
                came_late += usize::from(waited && judged.lost.is_empty());
                break (events, judged);
            }
            waited = true;
            std::thread::sleep(Duration::from_secs(2));
        };
        updates += judged.updates;
        updates_doubled += judged.doubled;
        updates_lost += judged.lost.len();
        odd.extend(judged.lost);
        let want: BTreeMap<String, Value> = events.iter().filter_map(|e| Some((e.get("id")?.as_str()?.to_owned(), e.clone()))).collect();
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let off: usize = viewers.iter().filter(|v| v.session == x.id).map(|v| want.iter().filter(|(k, e)| v.events.get(*k) != Some(*e)).count()).sum();
            if off == 0 {
                break;
            }
            if Instant::now() > deadline {
                updates_lost += off;
                odd.push(format!("a viewer of {} lacks {off} of its copy's events", x.id));
                break;
            }
            for v in &mut viewers {
                v.pump(s.port, &s.admin)?;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    if came_late > 0 {
        say(&format!("wire lab: {came_late} sessions' copies came whole only as the lab waited for them, within {:.0?}", checking.elapsed()));
    }
    let cuts: usize = runners.iter().map(|(_, log)| std::fs::read_to_string(log).unwrap_or_default().matches("chaos: dropping the connection").count()).sum();
    let faults = cuts + reloads + kills_done + server_restarts;
    say(&format!("wire lab, seed {seed}: {} sessions on {RUNNERS} runners, {} commands, {updates} updates", sessions.len(), sent.len()));
    for ((how, words), n) in &answers {
        say(&format!("  {how:?} answered {words}: {n}"));
    }
    let framing = if server_faults.is_empty() { "no frame faults".to_owned() } else { format!("and frame faults on both sides ({server_faults} at the server)") };
    say(&format!("faults: {faults} ({cuts} link cuts, {reloads} socket reloads, {kills_done} runners killed, {server_restarts} server restarts), {framing}"));
    say(&format!("commands lost {lost}, doubled {doubled}, unanswered {unanswered}, applied though refused {wrongly}; updates lost {updates_lost}, doubled {updates_doubled}"));
    // The frames made unreadable: each that came refused by its reader, none taken.
    let mut logs: Vec<PathBuf> = runners.iter().map(|(_, l)| l.clone()).collect();
    logs.push(dir.path().join("server.log"));
    let unreadable = Unreadable::of(&logs);
    if garble != 0 {
        let since = ["runner", "server"].iter().flat_map(|w| ["unknown", "malformed"].map(|c| common::refused(s.port, &s.admin, w, c))).sum::<u64>();
        say(&format!(
            "frames made unreadable: {} ({} came, {} lost below the channel with their connections); refused {}; came and not refused {} (kiln_wire_refused_total of runner and server frames since the server's last start: {since})",
            unreadable.made,
            unreadable.came,
            unreadable.made.saturating_sub(unreadable.came),
            unreadable.refused,
            unreadable.came.abs_diff(unreadable.refused),
        ));
    }
    for o in odd.iter().take(40) {
        eprintln!("  {o}");
    }
    // What the server and the runners said of each command that went missing or came twice, and
    // where it was applied.
    for token in lost_tokens.iter().take(5) {
        let c = sent.iter().find(|c| &c.token == token);
        let viewer_id = c.and_then(|c| match &c.answer {
            Answered::Socket { id, .. } => Some(id.clone()),
            _ => None,
        });
        // A message's own id, the inbox's: lab-msg-<seed>-<k> or lab-skill-<seed>-<k>.
        let k = token.rsplit('-').next().and_then(|n| n.parse::<usize>().ok()).unwrap_or(usize::MAX);
        let message_id = c.and_then(|c| match c.how {
            How::SocketMessage => Some(format!("lab-msg-{seed}-{k}")),
            How::SocketSkill => Some(format!("lab-skill-{seed}-{k}")),
            _ => None,
        });
        if let Some(x) = c.and_then(|c| sessions.get(c.session)) {
            let file = if c.is_some_and(|c| matches!(c.how, How::ApiModel | How::SocketModel)) { &x.controls } else { &x.messages };
            for l in std::fs::read_to_string(file).unwrap_or_default().lines().filter(|l| l.contains(token.as_str())) {
                eprintln!("  [{token} applied in {}] {}", x.id, l.chars().take(300).collect::<String>());
            }
        }
        let mut logs: Vec<PathBuf> = runners.iter().map(|(_, l)| l.clone()).collect();
        logs.push(dir.path().join("server.log"));
        for log in logs {
            for l in std::fs::read_to_string(&log).unwrap_or_default().lines().filter(|l| l.contains(token.as_str()) || viewer_id.as_ref().is_some_and(|i| l.contains(i.as_str())) || message_id.as_ref().is_some_and(|i| l.contains(i.as_str()))) {
                eprintln!("  [{}] {}", log.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default(), l.chars().take(300).collect::<String>());
            }
        }
    }
    anyhow::ensure!(sent.len() == commands_n, "{} commands sent of {commands_n}", sent.len());
    anyhow::ensure!((lost, doubled, unanswered, wrongly) == (0, 0, 0, 0), "commands lost {lost}, doubled {doubled}, unanswered {unanswered}, applied though refused {wrongly}");
    anyhow::ensure!((updates_lost, updates_doubled) == (0, 0), "updates lost {updates_lost}, doubled {updates_doubled}");
    if garble != 0 {
        anyhow::ensure!(unreadable.made > 0 && unreadable.came > 0, "no frame was made unreadable: {unreadable:?}");
        anyhow::ensure!(unreadable.came == unreadable.refused, "frames made unreadable that came: {}, refused: {}", unreadable.came, unreadable.refused);
    }
    if commands_n >= 10_000 {
        anyhow::ensure!(faults >= 200, "{faults} faults: the lab needs 200");
        // A frame made unreadable is made up for as one lost below the channel is: at one in 64 of
        // each kind they about double its repairs, and updates come about half as fast (seed 1 on
        // one machine, the same hour: 173,587 without them, 80,978 with, and 65,247 with twice the
        // frames dropped and late instead, WIRE_LAB_FRAMES=50). The repairs are what's held then,
        // so it needs half the updates, and that many of those frames to have come.
        let floor = if garble == 0 { 100_000 } else { 50_000 };
        anyhow::ensure!(updates >= floor, "{updates} updates: the lab needs {floor}");
        if garble != 0 {
            anyhow::ensure!(unreadable.came >= 10_000, "{} frames made unreadable came: the lab needs 10,000", unreadable.came);
        }
    }
    Ok(())
}
