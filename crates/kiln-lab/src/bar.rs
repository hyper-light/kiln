//! `kiln-lab bar`: kiln's conversations at the sizes T3 Code published its own speedups at (its week of
//! 2026-11-09 to 13, "T3 Code loads a lot faster this week"), read as kiln's UI reads them, against a real
//! server with real runners. The bar Ada set (2026-11-13): each of kiln's numbers a tenth of T3 Code's or less.
//!
//! What T3 Code measured, from its pull requests' own words (pingdotgg/t3code):
//! - **Open a 41,000 item thread** (#34490): "One real thread (230 turns, 41,000 items, still running)", the
//!   server's read of its first window: first read 2,230 ms to 31 ms, warm 244 ms to 19 ms, the wire snapshot
//!   3.15 MB to 0.39 MB.
//! - **Load earlier turns, 45,000 items** (#34573): "One thread of 45,154 turn items, 1,926 runs and 315 MB
//!   of payloads", one older page read by a fresh process: a cold file cache 9.6 s to 0.57 to 0.76 s (664 MB to
//!   29 MB read from disk), warm 801 to 834 ms to 80 to 89 ms.
//! - **Send to a 75,000 item thread** (#35130): what a send reads of its thread before it plans, warm: 220 ms
//!   to 90 ms (461 MB to 43 MB read).
//!
//! Here: a fresh kiln of the build given, `--runners` runners (T3 Code's lab had 11 environments), a fleet of
//! `--fleet` small sessions (production's 134 live feeds: `crate::lab::Profile::many`), and three long ones of
//! those sizes and turns, each written by the lab's stand-in through its runner as a session's are
//! (`lab:history:<events>:<a turn's>`, `crate::standin`). Then, over loopback:
//! - **open**: the session socket as `ui/src/api/harness.ts` attaches (`TAKES`), to its snapshot's end: the
//!   first after the fill, then `--samples` more, and the same while the session works (a turn under way);
//!   the bytes as they came (the packed snapshot), and unpacked;
//! - **older**: the page before the window as the UI asks for it (`OLDER_PAGE`: 100 events, 262,144 bytes),
//!   then pages a quarter, half and three quarters of the way back and at the session's start: the first read
//!   of each, then `--samples` more, with the server's own time for each (`Server-Timing`);
//! - **send**: a message to the longest session (`POST .../command`), answered: `--samples` of them;
//! - **launch**: what Mission control's first reads bring for the fleet (`ui/src/lib/early-script.ts`, and those
//!   its page asks as it renders), as sent (Brotli or gzip, as a browser asks) and unpacked.
//!
//! `--cold` then stops the server, lets go of its files' pages (Linux; elsewhere it says it couldn't), starts it
//! again on the same data and reads the first open and the first older page of a fresh process. `--hold` keeps
//! the kiln up afterwards (its address and token in `<out>/hold.json`) until `<out>/stop` appears, for the
//! browser's own probes.
//!
//! Every time is the lab's clock around the whole exchange (connect, upgrade, the server's work, the bytes
//! back), so it holds more than T3 Code's figures do (a read inside its server): where kiln's is under the bar
//! so measured, it is under it. The server's own time is beside it where the server says it.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use serde::Serialize;
use serde_json::{Value, json};

use crate::client::{Frame, Http, Socket};
use crate::fleet::Fleet;

/// The session socket's query as the UI asks it (`ui/src/api/harness.ts`, `TAKES`), packed and not: its window's first
/// part ahead (`first_bytes`), the rest after it. A kiln from before parts sends the window at once all the same.
const TAKES_PACKED: &str = "?z=gzip&patch=state&patch=changes&window=1&lean=1&recent_turns=8&event_bytes=262144&first_bytes=16384";
const TAKES: &str = "?patch=state&patch=changes&window=1&lean=1&recent_turns=8&event_bytes=262144&first_bytes=16384";
/// An older page as the UI asks it (`ui/src/api/harness.ts`, `OLDER_PAGE`).
const OLDER_PAGE: &str = "limit=100&bytes=262144";
/// Mission control's first reads: those its document asks as it's parsed (`ui/src/lib/early-script.ts`, `SHELL`,
/// `SIGNED` and `EARLY["/agents"]`), then those its page asks as it renders (`ui/src/views/harness/mission.tsx`):
/// its visit's attention snapshot among them, with the last words of the rows a page 900 px tall opens on
/// (`ui/src/views/harness/first-rows.ts`: those that begin within two viewports, none shorter than 84 px: 22).
const LAUNCH: &[&str] = &[
    "/api/me",
    "/api/peers/open",
    "/api/harness/sessions?lean=1",
    "/api/harness/attention?mode=arrival&visit=kiln-lab-bar&lean=1&said=22",
    "/api/harness/kinds?shared=1",
    "/api/agents?lean=1",
    "/api/terminal/sessions",
    "/api/reach/runners?lean=1",
    "/api/harness/workspaces",
    "/api/push",
    "/api/push/subscriptions",
    "/api/voice",
    "/api/agentfiles?lean=1",
    "/api/harness/changes",
    "/api/ai-sessions",
    "/api/ai-sessions/silent",
    "/api/push/notices",
];

/// The events socket as Mission control's page opens it (`ui/src/api/events.ts`): in batches, the agents lean
/// (W21-R02: each one's entry once, then what changed of it), the types its page listens to as it opens.
const EVENTS: &str = "/api/events?batch=1&lean=1&topics=agent,harness,harness_closed,queue,update_job";
/// The same socket as a page asked for it before the agents came lean: every change of an agent, its entry whole.
const EVENTS_WHOLE: &str = "/api/events?batch=1&topics=agent,harness,harness_closed,queue,update_job";
/// How long the events socket is counted beside the launch's reads: as long as T3 Code counted what its own
/// client was sent at launch ("socket frames counted on the client for 12 seconds after each socket opened",
/// pingdotgg/t3code#35072), so the two are the same count: what a client takes in over its first twelve seconds.
const LAUNCH_SOCKET: Duration = Duration::from_secs(12);
/// How long a read of an events socket waits before the clock is looked at again: the window the socket gathers
/// a batch for (`kiln::wire::events::feed::WINDOW`), so no oftener than a frame can come.
const EVENTS_WINDOW: Duration = Duration::from_millis(50);

#[derive(clap::Args, Clone)]
pub struct Args {
    /// The build's `kiln` binary.
    #[arg(long)]
    kiln: PathBuf,
    /// `KEY=VALUE` for its server's environment (repeatable).
    #[arg(long = "env")]
    env: Vec<String>,
    /// Where the kiln and the report go (`~/.cache/kiln-lab/bar-<when>` otherwise).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Runners: T3 Code's lab had 11 environments (#35072).
    #[arg(long, default_value_t = 11)]
    runners: usize,
    /// The fleet's other sessions, each short: production's 134 live feeds (`Profile::many`).
    #[arg(long, default_value_t = 134)]
    fleet: usize,
    /// The session opened: its events and its turns (T3 Code #34490: 41,000 items, 230 turns).
    #[arg(long, default_value = "41000:230")]
    open: String,
    /// The session paged back through (T3 Code #34573: 45,154 items, 1,926 runs).
    #[arg(long, default_value = "45154:1926")]
    older: String,
    /// The session sent to (T3 Code #35130: 75,000 items; #35286 names it 75,324; turns as the 41,000's, 178 events each).
    #[arg(long, default_value = "75324:423")]
    send: String,
    /// Reads after the first, for each case: T3 Code's own count for its formatter's figures (#30038: 21).
    #[arg(long, default_value_t = 21)]
    samples: usize,
    /// Stop the server, let go of its files' pages, start it again, and read the first open and older page.
    #[arg(long)]
    cold: bool,
    /// Keep the kiln up afterwards, until `<out>/stop` appears.
    #[arg(long)]
    hold: bool,
}

/// A case's times: the first, and the rest's median, least and most, in microseconds.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Times {
    pub first_us: u64,
    pub median_us: u64,
    pub min_us: u64,
    pub max_us: u64,
    pub n: usize,
}

impl Times {
    fn of(all: &[u64]) -> Self {
        let first = all.first().copied().unwrap_or_default();
        let mut rest: Vec<u64> = all.iter().skip(1).copied().collect();
        if rest.is_empty() {
            rest.push(first);
        }
        rest.sort_unstable();
        Self { first_us: first, median_us: rest.get(rest.len() / 2).copied().unwrap_or_default(), min_us: rest.first().copied().unwrap_or_default(), max_us: rest.last().copied().unwrap_or_default(), n: rest.len() }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Open {
    pub what: String,
    pub events: u64,
    pub turns: u64,
    pub took: Times,
    /// The packed snapshot as it came, bytes.
    pub wire: usize,
    /// Its frames unpacked, bytes, and how many.
    pub decoded: usize,
    pub frames: usize,
    /// The events its window brought.
    pub window_events: usize,
    /// Where the snapshot was its window's first part (`history`'s `part`): the rest of the window, which follows it.
    pub rest: Option<Rest>,
}

/// The rest of a window whose first part the snapshot was: to its message's end from the socket's opening, its message as
/// it came packed and unpacked, and the events it brought.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Rest {
    pub took: Times,
    pub wire: usize,
    pub decoded: usize,
    pub events: usize,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Page {
    pub what: String,
    pub before: u64,
    pub took: Times,
    /// The server's own time with each (`Server-Timing`'s `took`), where it said.
    pub server: Option<Times>,
    pub bytes: usize,
    pub events: usize,
    pub status: u16,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Read {
    pub path: String,
    pub status: u16,
    pub sent: usize,
    pub unpacked: usize,
    pub server_us: Option<u64>,
}

/// What a socket brought over a while: its frames and their bytes, as sent (the lab asks for no packing).
#[derive(Clone, Debug, Default, Serialize)]
pub struct Brought {
    pub path: String,
    pub seconds: u64,
    pub frames: usize,
    pub bytes: usize,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Report {
    pub command: String,
    pub kiln: String,
    pub host: String,
    pub os: String,
    pub cpus: usize,
    pub load1: Option<f64>,
    pub runners: usize,
    pub fleet: usize,
    pub fill_s: f64,
    pub opens: Vec<Open>,
    pub pages: Vec<Page>,
    pub send: Option<Page>,
    pub launch: Vec<Read>,
    /// What the events socket brought beside the launch's reads, as the page asks for it and as it did before the
    /// agents came lean, both over the same [`LAUNCH_SOCKET`].
    pub launch_events: Vec<Brought>,
    pub cold: Vec<String>,
    pub notes: Vec<String>,
}

fn us(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

/// `events:turns` as a session's size is given.
fn size(s: &str) -> anyhow::Result<(u64, u64)> {
    let (e, t) = s.split_once(':').with_context(|| format!("a session's size is events:turns, not {s:?}"))?;
    let (e, t): (u64, u64) = (e.parse()?, t.parse()?);
    anyhow::ensure!(e > 0 && t > 0 && t <= e, "a session of {e} events in {t} turns");
    Ok((e, t))
}

/// What one attach brought.
struct Attached {
    took: Duration,
    wire: usize,
    decoded: usize,
    frames: usize,
    events: usize,
    /// Its `history` frame's cursor and where its window begins, where it said.
    history: Option<(String, u64)>,
    /// Its `history` said its snapshot is its window's first part.
    part: bool,
    /// The rest of its window, where it was read for: when its message ended, its bytes as it came and unpacked, its events.
    rest: Option<(Duration, usize, usize, usize)>,
}

/// The session socket of `id` opened with `query`, read to its snapshot's end: a packed one's message, or `synced`. With
/// `rest`, on to the end of the message that follows a window's first part (the next packed message, or `earlier`).
fn attach(port: u16, admin: &str, id: &str, query: &str, rest: bool) -> anyhow::Result<Attached> {
    let t0 = Instant::now();
    let mut sock = Socket::open(port, admin, &format!("/api/harness/sessions/{id}/ws{query}"), Duration::from_secs(30))?;
    let mut a = Attached { took: Duration::ZERO, wire: 0, decoded: 0, frames: 0, events: 0, history: None, part: false, rest: None };
    let mut ended = false;
    loop {
        match sock.next()? {
            Frame::Binary if ended => {
                a.rest = Some((t0.elapsed(), sock.last_len(), 0, 0));
                break;
            }
            Frame::Binary => {
                a.took = t0.elapsed();
                a.wire = sock.last_len();
                ended = true;
            }
            Frame::Text(text) if ended => {
                // Live frames may follow a snapshot that had no rest to send: only `earlier` is the rest.
                if text.starts_with(br#"{"t":"earlier""#) {
                    let v: Value = serde_json::from_slice(&text).unwrap_or_default();
                    a.rest = Some((t0.elapsed(), 0, text.len(), v.get("events").and_then(Value::as_array).map_or(0, Vec::len)));
                    // The window's own head: where it begins, which is where the pages before it end.
                    if let (Some(c), Some(b)) = (v.get("cursor").and_then(Value::as_str), v.get("before").and_then(Value::as_u64)) {
                        a.history = Some((c.to_owned(), b));
                    }
                    break;
                }
            }
            Frame::Text(text) => {
                a.frames += 1;
                a.decoded += text.len();
                if text.starts_with(br#"{"t":"synced""#) {
                    a.took = t0.elapsed();
                    ended = true;
                } else if text.starts_with(br#"{"t":"history""#) {
                    let v: Value = serde_json::from_slice(&text).unwrap_or_default();
                    if let (Some(c), Some(b)) = (v.get("cursor").and_then(Value::as_str), v.get("before").and_then(Value::as_u64)) {
                        a.history = Some((c.to_owned(), b));
                    }
                    a.part = v.get("part").and_then(Value::as_bool) == Some(true);
                } else if text.starts_with(br#"{"t":"events""#) {
                    let v: Value = serde_json::from_slice(&text).unwrap_or_default();
                    a.events += v.get("events").and_then(Value::as_array).map_or(0, Vec::len);
                } else if text.starts_with(br#"{"t":"closed""#) {
                    anyhow::bail!("the session's socket says it's closed");
                }
            }
            Frame::Nothing if ended && a.rest.is_none() && rest => anyhow::bail!("no rest of the window within 30 s of its first part"),
            Frame::Nothing => anyhow::bail!("no snapshot within 30 s"),
            Frame::Closed => anyhow::bail!("the socket closed before its snapshot ended"),
        }
        if ended && !rest {
            break;
        }
    }
    sock.close();
    Ok(a)
}

/// `n` opens of `id` as the UI opens it, after one that counts what it brings unpacked (and says whether the snapshot is
/// its window's first part: then each open is read on to the rest's end too).
fn opens(port: u16, admin: &str, id: &str, what: &str, (events, turns): (u64, u64), n: usize) -> anyhow::Result<(Open, Option<(String, u64)>)> {
    let parted = attach(port, admin, id, TAKES, false)?.part;
    let plain = attach(port, admin, id, TAKES, parted)?;
    let (mut took, mut rest_took) = (Vec::with_capacity(n + 1), Vec::with_capacity(n + 1));
    let (mut wire, mut rest_wire) = (0, 0);
    for _ in 0..=n {
        let a = attach(port, admin, id, TAKES_PACKED, parted)?;
        took.push(us(a.took));
        wire = a.wire;
        if let Some((at, bytes, _, _)) = a.rest {
            rest_took.push(us(at));
            rest_wire = bytes;
        }
    }
    let rest = plain.rest.map(|(_, _, decoded, events)| Rest { took: Times::of(&rest_took), wire: rest_wire, decoded, events });
    Ok((Open { what: what.to_owned(), events, turns, took: Times::of(&took), wire, decoded: plain.decoded, frames: plain.frames, window_events: plain.events, rest }, plain.history))
}

/// The page of `id` before `before`, read `n` times after the first.
fn page(http: &mut Http, id: &str, cursor: &str, before: u64, what: &str, n: usize) -> anyhow::Result<Page> {
    let path = format!("/api/harness/sessions/{id}/events?before={before}&cursor={cursor}&{OLDER_PAGE}");
    let (mut took, mut server) = (Vec::with_capacity(n + 1), Vec::with_capacity(n + 1));
    let mut p = Page { what: what.to_owned(), before, ..Page::default() };
    for _ in 0..=n {
        let t0 = Instant::now();
        let a = http.send("GET", &path, None)?;
        took.push(us(t0.elapsed()));
        if let Some(s) = a.server {
            server.push(s.took_us);
        }
        p.status = a.status;
        p.bytes = a.body.len();
        p.events = serde_json::from_slice::<Value>(&a.body).ok().and_then(|v| v.get("events").and_then(Value::as_array).map(Vec::len)).unwrap_or_default();
    }
    p.took = Times::of(&took);
    p.server = (server.len() == took.len()).then(|| Times::of(&server));
    Ok(p)
}

/// The sessions' ids and statuses.
fn statuses(http: &mut Http) -> anyhow::Result<std::collections::BTreeMap<String, String>> {
    let a = http.send_settled("GET", "/api/harness/sessions", None)?;
    let v: Value = serde_json::from_slice(&a.body)?;
    let list = v.as_array().cloned().or_else(|| v.get("sessions").and_then(Value::as_array).cloned()).unwrap_or_default();
    Ok(list.iter().filter_map(|i| Some((i.get("id")?.as_str()?.to_owned(), i.get("status")?.as_str()?.to_owned()))).collect())
}

/// Waits until each of `sessions` (its id and its events) is idle with its newest event numbered that or more.
fn filled(fleet: &Fleet, sessions: &[(String, u64)], within: Duration) -> anyhow::Result<()> {
    let mut http = Http::new(fleet.port, &fleet.admin);
    let deadline = Instant::now() + within;
    let mut done = std::collections::HashSet::new();
    let mut said = Instant::now();
    loop {
        let st = statuses(&mut http)?;
        for (id, n) in sessions {
            if done.contains(id) || st.get(id).is_none_or(|s| s != "idle") {
                continue;
            }
            let newest = http.send_settled("GET", &format!("/api/harness/sessions/{id}/events?limit=1"), None)?;
            let seq = serde_json::from_slice::<Value>(&newest.body).ok().and_then(|v| v.pointer("/events/0/seq").and_then(Value::as_u64));
            if newest.status != 200 || seq.is_none_or(|s| s >= *n) {
                done.insert(id.clone());
            }
        }
        if done.len() == sessions.len() {
            return Ok(());
        }
        anyhow::ensure!(Instant::now() < deadline, "the fill didn't finish: {} of {} sessions in", done.len(), sessions.len());
        if said.elapsed() > Duration::from_secs(30) {
            eprintln!("kiln-lab bar: filling, {} of {} sessions in", done.len(), sessions.len());
            said = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// The machine's one-minute load, where it says.
fn load1() -> Option<f64> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/loadavg").ok()?.split_whitespace().next()?.parse().ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let out = std::process::Command::new("sysctl").args(["-n", "vm.loadavg"]).output().ok()?;
        String::from_utf8_lossy(&out.stdout).split_whitespace().find_map(|w| w.parse().ok())
    }
}

fn hostname() -> String {
    std::process::Command::new("hostname").output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned()).unwrap_or_default()
}

/// Lets go of the pages the OS holds of every file under `dir` (their data first written out): whether it could.
fn evict(dir: &Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        let mut all = true;
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if let Ok(f) = std::fs::File::open(&p) {
                    let _ = f.sync_all();
                    all &= rustix::fs::fadvise(&f, 0, None, rustix::fs::Advice::DontNeed).is_ok();
                }
            }
        }
        all
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = dir;
        false
    }
}

fn ms(us: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let v = us as f64 / 1e3;
    format!("{v:.2}")
}

/// The report as a person reads it.
pub fn markdown(r: &Report) -> String {
    let mut s = format!("# kiln-lab bar\n\n`{}`\n\n{} on {} ({}, {} CPUs, load {}), {} runners, a fleet of {} sessions, filled in {:.0} s.\n\n", r.command, r.kiln, r.host, r.os, r.cpus, r.load1.map_or_else(|| "?".to_owned(), |l| format!("{l:.1}")), r.runners, r.fleet, r.fill_s);
    s.push_str("## Open (the session socket to its snapshot's end)\n\n| session | first ms | median ms | least | most | packed B | unpacked B | frames | events in the window |\n|---|---|---|---|---|---|---|---|---|\n");
    for o in &r.opens {
        s.push_str(&format!("| {} ({} events, {} turns) | {} | {} | {} | {} | {} | {} | {} | {} |\n", o.what, o.events, o.turns, ms(o.took.first_us), ms(o.took.median_us), ms(o.took.min_us), ms(o.took.max_us), o.wire, o.decoded, o.frames, o.window_events));
    }
    if r.opens.iter().any(|o| o.rest.is_some()) {
        s.push_str("\nEach snapshot above is its window's first part. The rest of the window, which follows it on the socket (from the socket's opening to the rest's end):\n\n| session | first ms | median ms | least | most | packed B | unpacked B | events |\n|---|---|---|---|---|---|---|---|\n");
        for o in &r.opens {
            if let Some(rest) = &o.rest {
                s.push_str(&format!("| {} | {} | {} | {} | {} | {} | {} | {} |\n", o.what, ms(rest.took.first_us), ms(rest.took.median_us), ms(rest.took.min_us), ms(rest.took.max_us), rest.wire, rest.decoded, rest.events));
            }
        }
    }
    s.push_str("\n## Older pages (100 events, 262,144 bytes at the most)\n\n| page | before | status | first ms | median ms | least | most | the server's own median ms | bytes | events |\n|---|---|---|---|---|---|---|---|---|---|\n");
    for p in &r.pages {
        s.push_str(&format!("| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n", p.what, p.before, p.status, ms(p.took.first_us), ms(p.took.median_us), ms(p.took.min_us), ms(p.took.max_us), p.server.as_ref().map_or_else(|| "-".to_owned(), |t| ms(t.median_us)), p.bytes, p.events));
    }
    if let Some(p) = &r.send {
        s.push_str(&format!("\n## Send ({})\n\nfirst {} ms, median {} ms (least {}, most {}); the server's own median {} ms; status {}.\n", p.what, ms(p.took.first_us), ms(p.took.median_us), ms(p.took.min_us), ms(p.took.max_us), p.server.as_ref().map_or_else(|| "-".to_owned(), |t| ms(t.median_us)), p.status));
    }
    s.push_str("\n## Launch (Mission control's first reads)\n\n| read | status | as sent B | unpacked B | the server's own ms |\n|---|---|---|---|---|\n");
    let (mut sent, mut unpacked) = (0, 0);
    for l in &r.launch {
        sent += l.sent;
        unpacked += l.unpacked;
        s.push_str(&format!("| {} | {} | {} | {} | {} |\n", l.path, l.status, l.sent, l.unpacked, l.server_us.map_or_else(|| "-".to_owned(), ms)));
    }
    s.push_str(&format!("| all | | {sent} | {unpacked} | |\n"));
    if let Some(lean) = r.launch_events.first() {
        s.push_str(&format!(
            "\nIts events socket meanwhile (`{}`), for the {} s T3 Code counted its own client's launch: {} frames, {} B. Reads and socket, all told: {} B.\n",
            lean.path,
            lean.seconds,
            lean.frames,
            lean.bytes,
            unpacked + lean.bytes
        ));
    }
    for whole in r.launch_events.iter().skip(1) {
        s.push_str(&format!("The same {} s of the socket asked for without the agents lean (`{}`): {} frames, {} B.\n", whole.seconds, whole.path, whole.frames, whole.bytes));
    }
    for (title, lines) in [("Cold", &r.cold), ("Notes", &r.notes)] {
        if !lines.is_empty() {
            s.push_str(&format!("\n## {title}\n\n"));
            for l in lines {
                s.push_str(&format!("- {l}\n"));
            }
        }
    }
    s
}

pub fn main(a: &Args, started_secs: u64) -> anyhow::Result<()> {
    let out = match &a.out {
        Some(o) => o.clone(),
        None => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache/kiln-lab").join(format!("bar-{started_secs}")),
    };
    std::fs::create_dir_all(&out)?;
    let (open_size, older_size, send_size) = (size(&a.open)?, size(&a.older)?, size(&a.send)?);
    let env: Vec<(String, String)> = a.env.iter().map(|e| e.split_once('=').map(|(k, v)| (k.to_owned(), v.to_owned())).with_context(|| format!("--env takes KEY=VALUE, not {e:?}"))).collect::<anyhow::Result<_>>()?;
    let place = crate::lab::Place::of(None, "shared")?;
    let bin = crate::lab::stand_in(&out)?;
    let dir = out.join("kiln");
    std::fs::create_dir_all(&dir)?;
    let unit = format!("kiln-lab-bar-{}", std::process::id());
    let mut fleet = Fleet::server(&a.kiln, &dir, &env, None, &place, &unit).context("the bar's server")?;
    let names: Vec<String> = (0..a.runners.max(1)).map(|r| format!("bar-runner-{r}")).collect();
    for n in &names {
        fleet.runner(&a.kiln, n, &bin)?;
    }
    fleet.runners_ready(Duration::from_secs(300)).context("the bar's runners")?;
    eprintln!("kiln-lab bar: the kiln is up on port {} with {} runners; filling", fleet.port, names.len());

    // The fill: the three long sessions on the first runners, the fleet's short ones across them all.
    let filled_at = Instant::now();
    let (port, admin) = (fleet.port, fleet.admin.clone());
    let name = |i: usize| names.get(i % names.len()).cloned().unwrap_or_default();
    let long = [("open", open_size), ("older", older_size), ("send", send_size)];
    let mut sessions: Vec<(String, u64)> = Vec::new();
    let mut ids = std::collections::BTreeMap::new();
    for (i, (what, (events, turns))) in long.iter().enumerate() {
        let per = events.div_ceil(*turns);
        let id = crate::fleet::open(port, &admin, &name(i), &dir.join(format!("w/{what}")), &format!("lab:history:{events}:{per}"), Duration::from_secs(120))?;
        ids.insert((*what).to_owned(), id.clone());
        sessions.push((id, *events));
    }
    let mut small = Vec::new();
    for i in 0..a.fleet {
        small.push((crate::fleet::open(port, &admin, &name(i + long.len()), &dir.join(format!("w/f{i}")), "lab:prefill:40", Duration::from_secs(120))?, 40));
    }
    sessions.extend(small.iter().cloned());
    filled(&fleet, &sessions, Duration::from_secs(3600))?;
    let fill_s = filled_at.elapsed().as_secs_f64();
    eprintln!("kiln-lab bar: filled in {fill_s:.0} s");

    let mut report = Report {
        command: std::env::args().collect::<Vec<_>>().join(" "),
        kiln: a.kiln.display().to_string(),
        host: hostname(),
        os: format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH),
        cpus: std::thread::available_parallelism().map_or(1, std::num::NonZero::get),
        load1: load1(),
        runners: names.len(),
        fleet: a.fleet,
        fill_s,
        ..Report::default()
    };
    let id_of = |what: &str| ids.get(what).cloned().unwrap_or_default();

    // Open: each long session, then the first again while it works.
    let mut cursors = std::collections::BTreeMap::new();
    for (what, size) in &long {
        let (o, history) = opens(port, &admin, &id_of(what), &format!("{what}, idle"), *size, a.samples).with_context(|| format!("opening the {what} session"))?;
        report.opens.push(o);
        if let Some(h) = history {
            cursors.insert((*what).to_owned(), h);
        }
    }
    {
        // A turn under way: an event every 20 ms for as long as the opens take (T3 Code's thread was "still running").
        let mut http = Http::new(port, &admin);
        let body = json!({"type": "send", "id": "bar-working", "text": "lab:turn:3000:20"}).to_string();
        let sent = http.send_settled("POST", &format!("/api/harness/sessions/{}/command", id_of("open")), Some(body.as_bytes()))?;
        // Taken (200), or accepted for its inbox (202).
        if (200..300).contains(&sent.status) {
            std::thread::sleep(Duration::from_secs(1));
            let (o, _) = opens(port, &admin, &id_of("open"), "open, a turn under way", open_size, a.samples)?;
            report.opens.push(o);
        } else {
            report.notes.push(format!("the open session's turn wasn't started ({}): no open while it works", sent.status));
        }
    }

    // Older pages of the session paged back through.
    match cursors.get("older") {
        Some((cursor, before)) => {
            let mut http = Http::new(port, &admin).timed();
            let id = id_of("older");
            let (events, _) = older_size;
            let places = [("just before the window", *before), ("three quarters of the way", events * 3 / 4), ("half way", events / 2), ("a quarter of the way", events / 4), ("at its start", 150.min(events))];
            for (what, at) in places {
                report.pages.push(page(&mut http, &id, cursor, at, what, a.samples).with_context(|| format!("the page {what}"))?);
            }
        }
        None => report.notes.push("the older session's snapshot had no history frame: no pages read".to_owned()),
    }

    // Sends to the longest session, each answered before the next.
    {
        let mut http = Http::new(port, &admin).timed();
        let id = id_of("send");
        let path = format!("/api/harness/sessions/{id}/command");
        let (mut took, mut server) = (Vec::new(), Vec::new());
        let mut status = 0;
        for i in 0..=a.samples {
            let body = json!({"type": "send", "id": format!("bar-send-{i}"), "text": "lab:turn:1:0"}).to_string();
            let t0 = Instant::now();
            let answer = http.send("POST", &path, Some(body.as_bytes()))?;
            took.push(us(t0.elapsed()));
            if let Some(s) = answer.server {
                server.push(s.took_us);
            }
            status = answer.status;
            // Its one-event turn ends before the next is sent.
            let deadline = Instant::now() + Duration::from_secs(30);
            let mut plain = Http::new(port, &admin);
            while statuses(&mut plain)?.get(&id).is_some_and(|s| s != "idle") && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        let (events, turns) = send_size;
        report.send = Some(Page { what: format!("{events} events, {turns} turns"), took: Times::of(&took), server: (server.len() == took.len()).then(|| Times::of(&server)), status, ..Page::default() });
    }

    // Launch: Mission control's first reads, as a browser asks and unpacked.
    {
        let mut packed = Http::browser(port, &admin).timed();
        let mut plain = Http::new(port, &admin);
        for path in LAUNCH {
            let (p, u) = (packed.send("GET", path, None)?, plain.send("GET", path, None)?);
            report.launch.push(Read { path: (*path).to_owned(), status: p.status, sent: p.body.len(), unpacked: u.body.len(), server_us: p.server.map(|s| s.took_us) });
        }
    }
    // And what its events socket brings meanwhile, for as long as T3 Code counted its own launch: the fleet's
    // news, as the page asks for it (the agents lean) and, side by side over the same seconds, as it came before.
    {
        let mut sockets = Vec::new();
        for path in [EVENTS, EVENTS_WHOLE] {
            sockets.push((Brought { path: path.to_owned(), seconds: LAUNCH_SOCKET.as_secs(), ..Brought::default() }, Socket::open(port, &admin, path, EVENTS_WINDOW)?));
        }
        let began = Instant::now();
        while began.elapsed() < LAUNCH_SOCKET {
            for (brought, socket) in &mut sockets {
                match socket.next()? {
                    Frame::Text(text) => {
                        brought.frames += 1;
                        brought.bytes += text.len();
                    }
                    Frame::Closed => anyhow::bail!("the events socket closed within {} s of opening: {}", LAUNCH_SOCKET.as_secs(), brought.path),
                    Frame::Binary | Frame::Nothing => {}
                }
            }
        }
        for (brought, socket) in sockets {
            socket.close();
            report.launch_events.push(brought);
        }
    }

    if a.cold {
        cold(&mut fleet, a, &env, &place, &unit, &ids, older_size, &mut report)?;
    }

    std::fs::write(out.join("bar.json"), serde_json::to_vec_pretty(&report)?)?;
    let md = markdown(&report);
    std::fs::write(out.join("bar.md"), &md)?;
    println!("{md}");
    eprintln!("kiln-lab bar: the report is in {}", out.display());

    if a.hold {
        std::fs::write(out.join("hold.json"), serde_json::to_vec_pretty(&json!({"port": fleet.port, "admin": fleet.admin, "sessions": ids, "fleet": small.iter().map(|(id, _)| id).collect::<Vec<_>>()}))?)?;
        eprintln!("kiln-lab bar: holding the kiln on port {} (its address and token are in {}); touch {} to end", fleet.port, out.join("hold.json").display(), out.join("stop").display());
        while !out.join("stop").exists() {
            std::thread::sleep(Duration::from_millis(500));
        }
    }
    Ok(())
}

/// The first reads of a fresh process: the server stopped, its files' pages let go of where the OS lets the lab, started
/// again on the same data, then the first open of the opened session and the first older page of the paged one.
#[allow(clippy::too_many_arguments)]
fn cold(fleet: &mut Fleet, a: &Args, env: &[(String, String)], place: &crate::lab::Place, unit: &str, ids: &std::collections::BTreeMap<String, String>, (older_events, _): (u64, u64), report: &mut Report) -> anyhow::Result<()> {
    fleet.stop_server();
    let evicted = evict(&fleet.dir.join("data"));
    report.cold.push(if evicted { "the server's files' pages were let go of before it started again".to_owned() } else { "the OS's file cache was NOT emptied (this lab can't here): these are a fresh process's first reads, its files still cached".to_owned() });
    fleet.start_again(&a.kiln, env, place, &format!("{unit}-again")).context("the server again")?;
    let (port, admin) = (fleet.port, fleet.admin.clone());
    // Its sessions are back once their runners are (the server's copies are made again as they connect).
    let deadline = Instant::now() + Duration::from_secs(300);
    let wanted: Vec<&String> = ids.values().collect();
    let mut http = Http::new(port, &admin);
    loop {
        let st = statuses(&mut http).unwrap_or_default();
        if wanted.iter().all(|id| st.contains_key(*id)) {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "the sessions didn't come back after the restart");
        std::thread::sleep(Duration::from_millis(200));
    }
    let id_of = |what: &str| ids.get(what).cloned().unwrap_or_default();
    let first = attach(port, &admin, &id_of("open"), TAKES_PACKED, false)?;
    report.cold.push(format!("open, the first after the restart: {} ms, {} B packed", ms(us(first.took)), first.wire));
    let plain = attach(port, &admin, &id_of("older"), TAKES, false)?;
    report.cold.push(format!("the paged session's own first open: {} ms", ms(us(plain.took))));
    if let Some((cursor, _)) = plain.history {
        let mut timed = Http::new(port, &admin).timed();
        for (what, at) in [("half way", older_events / 2), ("a quarter of the way", older_events / 4)] {
            let p = page(&mut timed, &id_of("older"), &cursor, at, what, 0)?;
            report.cold.push(format!("older page {what}, the first read: {} ms (the server's own {} ms), status {}, {} B, {} events", ms(p.took.first_us), p.server.as_ref().map_or_else(|| "-".to_owned(), |t| ms(t.first_us)), p.status, p.bytes, p.events));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w21_r02_bar_a_cases_times_are_its_first_and_the_rests_median() {
        let t = Times::of(&[900, 30, 10, 20]);
        assert_eq!((t.first_us, t.median_us, t.min_us, t.max_us, t.n), (900, 20, 10, 30, 3));
        let one = Times::of(&[7]);
        assert_eq!((one.first_us, one.median_us, one.n), (7, 7, 1));
    }

    #[test]
    fn w21_r02_bar_a_sessions_size_is_its_events_and_turns() {
        assert_eq!(size("41000:230").ok(), Some((41_000, 230)));
        assert!(size("41000").is_err());
        assert!(size("10:20").is_err(), "more turns than events");
    }

    #[test]
    fn w21_r02_bar_the_report_says_each_case() {
        let r = Report { opens: vec![Open { what: "open, idle".into(), events: 41_000, turns: 230, took: Times::of(&[3_000, 1_000]), wire: 9, decoded: 99, frames: 8, window_events: 100, rest: None }], launch: vec![Read { path: "/api/me".into(), status: 200, sent: 10, unpacked: 20, server_us: Some(150) }], ..Report::default() };
        let md = markdown(&r);
        assert!(md.contains("| open, idle (41000 events, 230 turns) | 3.00 | 1.00 |"), "{md}");
        assert!(md.contains("| all | | 10 | 20 | |"), "{md}");
    }
}
