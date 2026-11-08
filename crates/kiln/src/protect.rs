//! `kiln protect` (LB05, docs/protect.md "The guard"): runs a command (the gate's systemd unit,
//! `ci/gate.sh`) while it watches the machine's own services, and stops the command's unit when
//! the guard says. The decisions are `kiln_ci::protect::guard`'s, pure code; this is the thin side:
//! it reads what the kernel and the servers say when they say it, and carries out what comes back.
//! It waits on those events and on a deadline the guard set, never on a clock of its own.
//!
//! - **The kernel** (Linux 6.4 and later, any user): a PSI trigger on each of `/proc/pressure/{cpu,
//!   memory,io}` wakes it once a window of the machine's "some" stall passes the guard's trigger,
//!   and it reads then each service's "full" stall from its cgroup, which anyone may read. A
//!   trigger on a service's own file needs its owner (root, for system.slice's), so the machine's
//!   own stall is the wake and the services' own are what's read. An older kernel refuses an
//!   unprivileged trigger: stalls go unwatched, and it says so.
//! - **Servers**: each kiln server named says its strain as it happens (`GET /healthz/strain`), a
//!   line at least every deadline.
//! - **The work**: its exit, which a thread waits on; its cgroup's counters (`cpu.stat`, `io.stat`,
//!   `memory.stat`, `memory.current`), read as the rest is, for what it used.
//!
//! Every input and decision goes to `--log` as a JSON line, the first saying the setup and what was
//! learned before, so `kiln protect --replay <log>` makes the same decisions again.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use kiln_ci::protect::guard::Policy;

/// The subcommand, dispatched before the runtime starts (it needs none).
pub const ARG: &str = "protect";

/// What it exits with when it stopped the work, or didn't start it: the machine was strained (sysexits'
/// EX_TEMPFAIL, "the user is invited to retry the operation later"). The reason is in `--why`.
const STRAINED: u8 = 75;

const HELP: &str = "kiln protect: run a command while the machine's own services are watched, and stop it when they suffer (docs/protect.md)

  kiln protect [options] -- <command> [<args>...]
  kiln protect --replay <log>        make a run's decisions again from its log, and say whether they're the same

  --unit <unit>                  the systemd user unit the command runs (stopped with systemctl --user stop)
  --server <url>                 a kiln server whose strain to follow (GET /healthz/strain); more than one may be given
  --containers <a,b>             protect these Docker containers alone, by name, rather than every service
  --state <file>                 what the guard learned, read at start and kept at the end
  --log <file>                   every input and decision, one JSON line each
  --why <file>                   why it stopped the command, or didn't start it
  --deadline-ms <ms>             the deadline the machine's services are held to (default 1000: a person's flow of thought)
  --false-alarms-per-day <n>     how often it may stop work that harmed nothing (default 1)

It exits with the command's own code, or 75 when it stopped the command or didn't start it.";

/// What it was asked.
#[derive(Debug, Default, PartialEq)]
struct Args {
    unit: Option<String>,
    servers: Vec<String>,
    containers: Vec<String>,
    state: Option<PathBuf>,
    log: Option<PathBuf>,
    why: Option<PathBuf>,
    policy: Policy,
    replay: Option<PathBuf>,
    command: Vec<OsString>,
}

impl Args {
    fn parse(args: &[OsString]) -> Result<Self, String> {
        fn next(it: &mut std::slice::Iter<'_, OsString>, name: &str) -> Result<String, String> {
            it.next().map(|v| v.to_string_lossy().into_owned()).ok_or_else(|| format!("{name} needs a value"))
        }
        let mut a = Args::default();
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            let arg = arg.to_string_lossy();
            let mut value = |name: &str| next(&mut it, name);
            match arg.as_ref() {
                "--" => {
                    a.command = args.iter().skip_while(|x| *x != "--").skip(1).cloned().collect();
                    break;
                }
                "--unit" => a.unit = Some(value("--unit")?),
                "--server" => a.servers.push(value("--server")?),
                "--containers" => a.containers = value("--containers")?.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect(),
                "--state" => a.state = Some(PathBuf::from(value("--state")?)),
                "--log" => a.log = Some(PathBuf::from(value("--log")?)),
                "--why" => a.why = Some(PathBuf::from(value("--why")?)),
                "--replay" => a.replay = Some(PathBuf::from(value("--replay")?)),
                "--deadline-ms" => {
                    let ms: f64 = value("--deadline-ms")?.parse().map_err(|_| "--deadline-ms takes milliseconds".to_owned())?;
                    if !(ms.is_finite() && ms > 0.0) {
                        return Err("--deadline-ms must be more than 0".to_owned());
                    }
                    a.policy.deadline_us = ms * 1e3;
                }
                "--false-alarms-per-day" => {
                    let n: f64 = value("--false-alarms-per-day")?.parse().map_err(|_| "--false-alarms-per-day takes a number".to_owned())?;
                    if !(n.is_finite() && n > 0.0) {
                        return Err("--false-alarms-per-day must be more than 0".to_owned());
                    }
                    a.policy.false_alarms_per_day = n;
                }
                other => return Err(format!("no such option {other} (--help says what it takes)")),
            }
        }
        if a.replay.is_none() && a.command.is_empty() {
            return Err("nothing to run: kiln protect [options] -- <command>".to_owned());
        }
        Ok(a)
    }
}

/// The URL of a server's strain stream, from its base or any URL on it, asking for a line at least
/// every `within_ms`: host, port and path.
fn strain_url(url: &str, within_ms: u64) -> Result<(String, String), String> {
    let rest = url.strip_prefix("http://").ok_or_else(|| format!("{url}: only http:// servers are followed"))?;
    let host = rest.split('/').next().unwrap_or_default();
    if host.is_empty() {
        return Err(format!("{url}: no host"));
    }
    let addr = if host.contains(':') && !host.ends_with(']') { host.to_owned() } else { format!("{host}:80") };
    Ok((addr, format!("/healthz/strain?within_ms={within_ms}")))
}

/// A stream's body as it arrives, in lines: chunked (HTTP/1.1's transfer coding, RFC 9112 §7.1) or
/// running to the connection's end.
struct Body<R: std::io::BufRead> {
    r: R,
    chunked: bool,
    /// What's left of the chunk under way.
    left: usize,
    done: bool,
}

impl<R: std::io::BufRead> Body<R> {
    /// The next line, without its newline; none once the body ends.
    fn line(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        let mut out = Vec::new();
        loop {
            if self.done {
                return Ok((!out.is_empty()).then_some(out));
            }
            if self.chunked && self.left == 0 {
                let mut size = String::new();
                if self.r.read_line(&mut size)? == 0 {
                    self.done = true;
                    continue;
                }
                let hex = size.trim().split(';').next().unwrap_or_default();
                if hex.is_empty() {
                    // The CRLF that ends a chunk's data.
                    continue;
                }
                self.left = usize::from_str_radix(hex, 16).map_err(|_| std::io::Error::other(format!("a chunk's size isn't hex: {hex:?}")))?;
                if self.left == 0 {
                    self.done = true;
                }
                continue;
            }
            let buf = self.r.fill_buf()?;
            if buf.is_empty() {
                self.done = true;
                continue;
            }
            let room = if self.chunked { self.left.min(buf.len()) } else { buf.len() };
            let piece = buf.get(..room).unwrap_or_default();
            match piece.iter().position(|b| *b == b'\n') {
                Some(at) => {
                    out.extend_from_slice(piece.get(..at).unwrap_or_default());
                    self.r.consume(at + 1);
                    if self.chunked {
                        self.left -= at + 1;
                    }
                    return Ok(Some(out));
                }
                None => {
                    out.extend_from_slice(piece);
                    self.r.consume(room);
                    if self.chunked {
                        self.left -= room;
                    }
                }
            }
        }
    }
}

#[cfg(unix)]
pub fn main(args: &[OsString]) -> ExitCode {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{HELP}");
        return ExitCode::SUCCESS;
    }
    match Args::parse(args).and_then(|a| if let Some(log) = &a.replay { unix::replay(log).map(|(same, differ)| unix::said(same, &differ)) } else { unix::run(a) }) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("kiln protect: {e}");
            ExitCode::from(2)
        }
    }
}

#[cfg(not(unix))]
pub fn main(args: &[OsString]) -> ExitCode {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{HELP}");
        return ExitCode::SUCCESS;
    }
    eprintln!("kiln protect: this platform has nothing it can watch by event (no PSI, no cgroups)");
    ExitCode::from(2)
}

#[cfg(unix)]
mod unix {
    use std::collections::{BTreeMap, BTreeSet, VecDeque};
    use std::io::{BufRead as _, BufReader, Read as _, Write as _};
    use std::net::{TcpStream, ToSocketAddrs as _};
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use kiln_ci::protect::cause::Resource;
    use kiln_ci::protect::guard::{Decision, Guard, Input, Learned, Line, Pressure, Setup};
    use rustix::event::{PollFd, PollFlags, Timespec};

    use super::*;

    /// What a thread tells the loop.
    enum Event {
        Line { server: String, line: Line },
        Gone { server: String, why: String },
        Exited(Option<i32>),
    }

    /// A thread's word to the loop: the event, then a byte to wake its poll.
    #[derive(Clone)]
    struct Tell {
        tx: mpsc::Sender<Event>,
        wake: std::sync::Arc<UnixStream>,
    }

    impl Tell {
        fn send(&self, e: Event) {
            let _ = self.tx.send(e);
            let _ = (&*self.wake).write_all(&[1]);
        }
    }

    /// The log: the setup and what was learned, then each input and the decisions it made.
    struct Log(Option<std::fs::File>);

    impl Log {
        fn open(path: Option<&Path>, setup: &Setup, learned: &Learned) -> Result<Self, String> {
            let Some(path) = path else { return Ok(Log(None)) };
            let mut f = std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
            let _ = writeln!(f, "{}", serde_json::json!({ "setup": setup, "learned": learned, "began_us": 0 }));
            Ok(Log(Some(f)))
        }

        fn put(&mut self, key: &str, v: &impl serde::Serialize) {
            if let Some(f) = self.0.as_mut() {
                let _ = writeln!(f, "{}", serde_json::json!({ key: v }));
            }
        }
    }

    fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
        serde_json::from_slice(&std::fs::read(path).ok()?).ok()
    }

    /// Written whole or not at all: to a file beside it, then renamed over it.
    fn keep(path: &Path, bytes: &[u8]) {
        let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if std::fs::write(&tmp, bytes).is_ok() && std::fs::rename(&tmp, path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// Follows one server's strain stream, telling each line; once it ends, why.
    fn follow(server: String, within_ms: u64, tell: Tell) {
        let why = match open(&server, within_ms) {
            Ok(mut body) => loop {
                match body.line() {
                    Ok(Some(raw)) if raw.iter().all(u8::is_ascii_whitespace) => continue,
                    Ok(Some(raw)) => match serde_json::from_slice::<Line>(&raw) {
                        Ok(line) => tell.send(Event::Line { server: server.clone(), line }),
                        Err(e) => break format!("a line it sent doesn't read: {e}"),
                    },
                    Ok(None) => break "its stream ended".to_owned(),
                    Err(e) => break format!("its stream failed: {e}"),
                }
            },
            Err(why) => why,
        };
        tell.send(Event::Gone { server, why });
    }

    /// Opens a server's strain stream: its body, once it answered 200.
    fn open(server: &str, within_ms: u64) -> Result<Body<BufReader<TcpStream>>, String> {
        let (addr, path) = strain_url(server, within_ms)?;
        let to = addr.to_socket_addrs().map_err(|e| format!("{addr}: {e}"))?.next().ok_or_else(|| format!("{addr}: no address"))?;
        // RFC 6298 §2.1's timeout before anything is measured: 1 s.
        let s = TcpStream::connect_timeout(&to, Duration::from_secs(1)).map_err(|e| format!("can't connect to {addr}: {e}"))?;
        let host = addr.clone();
        (&s).write_all(format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: kiln-protect\r\nAccept: application/x-ndjson\r\n\r\n").as_bytes()).map_err(|e| format!("can't ask {addr}: {e}"))?;
        let mut r = BufReader::new(s);
        let mut status = String::new();
        r.read_line(&mut status).map_err(|e| format!("{addr} didn't answer: {e}"))?;
        let code = status.split_whitespace().nth(1).unwrap_or_default().to_owned();
        let mut chunked = false;
        loop {
            let mut h = String::new();
            if r.read_line(&mut h).map_err(|e| format!("{addr}'s answer broke off: {e}"))? == 0 {
                return Err(format!("{addr}'s answer broke off"));
            }
            let h = h.trim();
            if h.is_empty() {
                break;
            }
            if let Some((name, value)) = h.split_once(':')
                && name.trim().eq_ignore_ascii_case("transfer-encoding")
                && value.to_ascii_lowercase().contains("chunked")
            {
                chunked = true;
            }
        }
        if code != "200" {
            return Err(format!("{server} answered {code} to {path} (a kiln server from before LB05 says nothing of its strain)"));
        }
        Ok(Body { r, chunked, left: 0, done: false })
    }

    /// How fine the clock is (µs): RFC 6298's G.
    fn granularity_us() -> f64 {
        nix::time::clock_getres(nix::time::ClockId::CLOCK_MONOTONIC).map_or(1.0, |t| t.tv_sec() as f64 * 1e6 + t.tv_nsec() as f64 / 1e3)
    }

    /// Everything the loop holds.
    struct Run {
        args: Args,
        base: Instant,
        guard: Guard,
        log: Log,
        tell: Tell,
        triggers: BTreeMap<Resource, std::fs::File>,
        machine: os::Machine,
        child: Option<u32>,
        due: Option<u64>,
        admitted: bool,
        pending: BTreeSet<String>,
        stopped: Option<String>,
        refused: Option<String>,
        exited: Option<Option<i32>>,
    }

    impl Run {
        fn now(&self) -> u64 {
            u64::try_from(self.base.elapsed().as_micros()).unwrap_or(u64::MAX)
        }

        fn say(&self, what: &str) {
            eprintln!("kiln protect: {what}");
        }

        /// The machine's counters, read now, as an input.
        fn read(&mut self, woke: Vec<Resource>) -> Input {
            let now = self.now();
            let resources = self.guard.setup.resources.clone();
            let pressure: Vec<(Resource, Vec<Pressure>)> = resources.iter().map(|r| (*r, self.machine.pressure(*r))).collect();
            Input::Read { now_us: now, woke, pressure, own: self.machine.own(), written_bps: self.machine.written_bps() }
        }

        /// Feeds `first` to the guard, carrying out what comes back, and what that feeds it in turn.
        fn feed(&mut self, first: Input) {
            let mut queue = VecDeque::from([first]);
            while let Some(input) = queue.pop_front() {
                self.log.put("in", &input);
                let out = self.guard.step(&input);
                self.log.put("out", &out);
                for d in out {
                    if let Some(more) = self.apply(d) {
                        queue.push_back(more);
                    }
                }
            }
        }

        /// Carries out a decision: what it feeds the guard next, if anything.
        fn apply(&mut self, d: Decision) -> Option<Input> {
            match d {
                Decision::Start { why } => {
                    self.say(&why);
                    self.start();
                    None
                }
                Decision::Refuse { why } => {
                    self.say(&format!("not started: {why}"));
                    self.refused = Some(why);
                    None
                }
                Decision::Stop { why } => {
                    self.say(&format!("stopping it: {why}"));
                    self.stopped = Some(why);
                    let took = self.stop();
                    Some(Input::Stopped { now_us: self.now(), took_us: took })
                }
                Decision::Trigger { resource, stall_us, window_us } => match os::arm(resource, stall_us, window_us) {
                    Ok(f) => {
                        self.triggers.insert(resource, f);
                        None
                    }
                    Err(why) => {
                        self.triggers.remove(&resource);
                        Some(Input::Lost { now_us: self.now(), resource, why })
                    }
                },
                Decision::Due { at_us } => {
                    self.due = Some(at_us);
                    None
                }
                Decision::Note { what } => {
                    self.say(&what);
                    None
                }
            }
        }

        /// Starts the command, and a thread that waits for it to end.
        fn start(&mut self) {
            let Some((program, rest)) = self.args.command.split_first() else { return };
            match std::process::Command::new(program).args(rest).spawn() {
                Ok(mut child) => {
                    self.child = Some(child.id());
                    let tell = self.tell.clone();
                    let spawned = std::thread::Builder::new().name("kiln-protect-wait".into()).spawn(move || {
                        let code = child.wait().ok().and_then(|s| s.code());
                        tell.send(Event::Exited(code));
                    });
                    if let Err(e) = spawned {
                        self.say(&format!("can't wait for the command: {e}"));
                    }
                }
                Err(e) => {
                    self.say(&format!("can't start {}: {e}", program.to_string_lossy()));
                    self.exited = Some(Some(127));
                }
            }
        }

        /// Stops the work: its unit, else the command itself. How long that took (µs).
        fn stop(&mut self) -> f64 {
            let at = Instant::now();
            match (&self.args.unit, self.child) {
                (Some(unit), _) => {
                    let _ = std::process::Command::new("systemctl").args(["--user", "stop", unit]).status();
                }
                (None, Some(pid)) => {
                    if let Ok(pid) = i32::try_from(pid) {
                        let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), nix::sys::signal::Signal::SIGTERM);
                    }
                }
                (None, None) => {}
            }
            at.elapsed().as_micros() as f64
        }
    }

    /// Runs the command under the guard: its own exit code, or [`STRAINED`] when the guard stopped it
    /// or didn't start it.
    pub fn run(args: Args) -> Result<u8, String> {
        let base = Instant::now();
        let learned: Learned = args.state.as_deref().and_then(read_json).unwrap_or_default();
        let setup = Setup {
            policy: args.policy,
            privileged: nix::unistd::geteuid().is_root(),
            resources: if cfg!(target_os = "linux") { Resource::ALL.to_vec() } else { Vec::new() },
            servers: args.servers.clone(),
            granularity_us: granularity_us(),
            cpus: os::online_cpus(),
        };
        let log = Log::open(args.log.as_deref(), &setup, &learned)?;
        let (tx, rx) = mpsc::channel::<Event>();
        let (wake_r, wake_w) = UnixStream::pair().map_err(|e| format!("can't make its wake: {e}"))?;
        wake_r.set_nonblocking(true).map_err(|e| format!("can't make its wake: {e}"))?;
        let tell = Tell { tx, wake: std::sync::Arc::new(wake_w) };
        let guard = Guard::new(setup.clone(), learned, 0);
        let machine = os::Machine::new(args.unit.clone(), args.containers.clone());
        let pending: BTreeSet<String> = args.servers.iter().cloned().collect();
        let mut run = Run { args, base, guard, log, tell, triggers: BTreeMap::new(), machine, child: None, due: None, admitted: false, pending, stopped: None, refused: None, exited: None };
        // Each server followed on a thread of its own.
        let within_ms = (run.guard.within_us() / 1e3).ceil() as u64;
        for server in run.args.servers.clone() {
            let tell = run.tell.clone();
            if let Err(e) = std::thread::Builder::new().name("kiln-protect-server".into()).spawn(move || follow(server, within_ms, tell)) {
                return Err(format!("can't follow a server: {e}"));
            }
        }
        // The triggers first, then the counters they're read against.
        let begun = run.guard.begin();
        run.log.put("begin", &begun);
        for d in begun {
            if let Some(more) = run.apply(d) {
                run.feed(more);
            }
        }
        let first = run.read(Vec::new());
        run.feed(first);
        if run.pending.is_empty() {
            run.admitted = true;
            let now = run.now();
            run.feed(Input::Admit { now_us: now });
        }
        loop {
            if run.refused.is_some() || run.exited.is_some() {
                break;
            }
            let now = run.now();
            let timeout = run.due.map(|at| {
                let us = at.saturating_sub(now);
                Timespec { tv_sec: i64::try_from(us / 1_000_000).unwrap_or(i64::MAX), tv_nsec: i64::try_from((us % 1_000_000) * 1_000).unwrap_or(0) }
            });
            let order: Vec<Resource> = run.triggers.keys().copied().collect();
            let (woke, lost, waked) = {
                let mut fds: Vec<PollFd<'_>> = run.triggers.values().map(|f| PollFd::new(f, PollFlags::PRI)).collect();
                fds.push(PollFd::new(&wake_r, PollFlags::IN));
                if let Err(e) = rustix::event::poll(&mut fds, timeout.as_ref())
                    && e != rustix::io::Errno::INTR
                {
                    return Err(format!("can't wait: {e}"));
                }
                let mut woke = Vec::new();
                let mut lost = Vec::new();
                for (r, fd) in order.iter().zip(&fds) {
                    let ev = fd.revents();
                    if ev.contains(PollFlags::ERR) || ev.contains(PollFlags::NVAL) {
                        lost.push(*r);
                    } else if ev.contains(PollFlags::PRI) {
                        woke.push(*r);
                    }
                }
                let waked = fds.last().is_some_and(|f| f.revents().contains(PollFlags::IN));
                (woke, lost, waked)
            };
            for r in lost {
                run.triggers.remove(&r);
                let now = run.now();
                run.feed(Input::Lost { now_us: now, resource: r, why: "its trigger went away (PSI turned off?)".to_owned() });
            }
            if !woke.is_empty() {
                let input = run.read(woke);
                run.feed(input);
            }
            if waked {
                let mut sink = [0u8; 64];
                while matches!((&wake_r).read(&mut sink), Ok(n) if n > 0) {}
            }
            while let Ok(e) = rx.try_recv() {
                match e {
                    Event::Line { server, line } => {
                        let now = run.now();
                        let own = run.machine.own();
                        run.feed(Input::Line { now_us: now, server: server.clone(), line, own });
                        run.pending.remove(&server);
                    }
                    Event::Gone { server, why } => {
                        let now = run.now();
                        run.feed(Input::Gone { now_us: now, server: server.clone(), why });
                        run.pending.remove(&server);
                    }
                    Event::Exited(code) => run.exited = Some(code),
                }
                if !run.admitted && run.pending.is_empty() {
                    run.admitted = true;
                    let now = run.now();
                    run.feed(Input::Admit { now_us: now });
                }
            }
            if run.due.is_some_and(|at| run.now() >= at) {
                run.due = None;
                let now = run.now();
                run.feed(Input::Due { now_us: now });
            }
        }
        let now = run.now();
        run.feed(Input::Ended { now_us: now });
        if let Some(path) = &run.args.state {
            keep(path, &serde_json::to_vec(&run.guard.learned).unwrap_or_default());
        }
        let why = run.refused.as_ref().map(|w| format!("not started: {w}")).or_else(|| run.stopped.as_ref().map(|w| format!("stopped: {w}")));
        if let (Some(why), Some(path)) = (&why, &run.args.why) {
            keep(path, why.as_bytes());
        }
        Ok(match (why, run.exited.flatten()) {
            (Some(_), _) => STRAINED,
            (None, Some(code)) => u8::try_from(code).unwrap_or(1),
            (None, None) => 1,
        })
    }

    /// What a replay says, and its exit code: 0 when every decision came out the same.
    pub fn said(same: usize, differ: &[String]) -> u8 {
        if differ.is_empty() {
            println!("kiln protect: replayed: the same {same} decisions");
            0
        } else {
            for d in differ {
                println!("kiln protect: differs at {d}");
            }
            1
        }
    }

    /// `kiln protect --replay <log>`: the run's inputs through a guard begun from what it had learned;
    /// how many decisions came out the same, and where any didn't.
    pub fn replay(log: &Path) -> Result<(usize, Vec<String>), String> {
        let text = std::fs::read_to_string(log).map_err(|e| format!("{}: {e}", log.display()))?;
        let mut lines = text.lines().filter(|l| !l.trim().is_empty());
        let head: serde_json::Value = lines.next().and_then(|l| serde_json::from_str(l).ok()).ok_or("the log has no setup line")?;
        let setup: Setup = serde_json::from_value(head.get("setup").cloned().unwrap_or_default()).map_err(|e| format!("its setup doesn't read: {e}"))?;
        let learned: Learned = serde_json::from_value(head.get("learned").cloned().unwrap_or_default()).map_err(|e| format!("what it learned doesn't read: {e}"))?;
        let mut g = Guard::new(setup, learned, head.get("began_us").and_then(serde_json::Value::as_u64).unwrap_or(0));
        let (mut inputs, mut same, mut differ) = (0usize, 0usize, Vec::new());
        let mut pending: Option<(usize, Vec<Decision>)> = None;
        for (n, l) in lines.enumerate() {
            let v: serde_json::Value = serde_json::from_str(l).map_err(|e| format!("line {}: {e}", n + 2))?;
            if let Some(b) = v.get("begin") {
                let logged: Vec<Decision> = serde_json::from_value(b.clone()).map_err(|e| format!("line {}: {e}", n + 2))?;
                let made = g.begin();
                if logged != made {
                    differ.push(format!("begin: logged {logged:?}, made {made:?}"));
                }
            } else if let Some(i) = v.get("in") {
                let input: Input = serde_json::from_value(i.clone()).map_err(|e| format!("line {}: {e}", n + 2))?;
                inputs += 1;
                pending = Some((n + 2, g.step(&input)));
            } else if let Some(o) = v.get("out") {
                let logged: Vec<Decision> = serde_json::from_value(o.clone()).map_err(|e| format!("line {}: {e}", n + 2))?;
                if let Some((at, made)) = pending.take() {
                    if made == logged {
                        same += 1;
                    } else {
                        differ.push(format!("line {at}: logged {logged:?}, made {made:?}"));
                    }
                }
            }
        }
        if inputs == 0 {
            return Err("the log has no inputs".to_owned());
        }
        Ok((same, differ))
    }

    // What the guard reads of the machine: Linux's PSI and cgroups, whose files no other system has
    // (there, nothing is armed or read, and only servers are followed).
    use super::machine as os;
}

#[cfg(unix)]
mod machine {
    //! The Linux side, kept thin: files read and a trigger written, nothing decided. Only std's files
    //! and processes, so it builds, and its readers are tested, wherever kiln builds for unix (on macOS
    //! none of its files exist: nothing is armed, nothing read).

    use std::collections::BTreeMap;
    use std::io::Write as _;
    use std::path::{Path, PathBuf};

    use kiln_ci::protect::cause::Resource;
    use kiln_ci::protect::guard::{Own, Pressure};

    const ROOT: &str = "/sys/fs/cgroup";

    /// A PSI trigger on the whole machine's `resource`: woken once a window of `window_us` holds
    /// `stall_us` of "some" stall (Documentation/accounting/psi.rst: "<some|full> <stall amount in us>
    /// <time window in us>"; the kernel reads it up to its last byte, which it takes for a NUL).
    pub fn arm(resource: Resource, stall_us: u64, window_us: u64) -> Result<std::fs::File, String> {
        let path = format!("/proc/pressure/{}", resource.name());
        let mut f = std::fs::OpenOptions::new().read(true).write(true).open(&path).map_err(|e| format!("{path}: {e} (Linux before 6.4 takes a trigger from root alone)"))?;
        f.write_all(format!("some {stall_us} {window_us}\0").as_bytes()).map_err(|e| format!("{path}: the trigger \"some {stall_us} {window_us}\" was refused: {e}"))?;
        Ok(f)
    }

    /// The machine's CPUs online (/sys/devices/system/cpu/online: "0-31", "0-3,8-11"), however few
    /// this process may use; where it can't be read, those this process may.
    pub fn online_cpus() -> f64 {
        std::fs::read_to_string("/sys/devices/system/cpu/online").ok().and_then(|t| cpu_count(&t)).unwrap_or_else(|| std::thread::available_parallelism().map_or(1, std::num::NonZero::get)) as f64
    }

    /// How many CPUs a cpu list names ("0-31", "0-3,8-11", "5").
    fn cpu_count(list: &str) -> Option<usize> {
        let mut n = 0usize;
        for part in list.trim().split(',').filter(|p| !p.is_empty()) {
            match part.split_once('-') {
                Some((a, b)) => n += b.trim().parse::<usize>().ok()?.checked_sub(a.trim().parse::<usize>().ok()?)? + 1,
                None => {
                    part.trim().parse::<usize>().ok()?;
                    n += 1;
                }
            }
        }
        (n > 0).then_some(n)
    }

    /// A pressure file's "full" line: its total (µs) and its avg10 (percent).
    fn full(text: &str) -> Option<(f64, f64)> {
        let line = text.lines().find(|l| l.starts_with("full "))?;
        let field = |name: &str| line.split_whitespace().find_map(|kv| kv.strip_prefix(name)).and_then(|v| v.parse::<f64>().ok());
        Some((field("total=")?, field("avg10=").unwrap_or(0.0)))
    }

    /// The machine as the guard reads it.
    pub struct Machine {
        unit: Option<String>,
        /// The work's cgroup, once systemd says where it is.
        cgroup: Option<PathBuf>,
        /// Where this process runs: never a service to protect.
        own: Option<String>,
        containers: Vec<String>,
        /// What each Docker container is: (its name, whether kiln made it).
        docker: BTreeMap<String, (String, bool)>,
        disks: Option<(std::time::Instant, u64)>,
    }

    impl Machine {
        pub fn new(unit: Option<String>, containers: Vec<String>) -> Self {
            let own = std::fs::read_to_string("/proc/self/cgroup").ok().and_then(|t| t.lines().find_map(|l| l.strip_prefix("0::").map(|p| p.trim_start_matches('/').to_owned())));
            Self { unit, cgroup: None, own, containers, docker: BTreeMap::new(), disks: None }
        }

        /// The work's cgroup (relative to the root), asked of systemd until it has one.
        fn work(&mut self) -> Option<PathBuf> {
            if self.cgroup.is_none()
                && let Some(unit) = &self.unit
                && let Ok(out) = std::process::Command::new("systemctl").args(["--user", "show", "-P", "ControlGroup", unit]).output()
            {
                let path = String::from_utf8_lossy(&out.stdout).trim().trim_start_matches('/').to_owned();
                if !path.is_empty() {
                    self.cgroup = Some(PathBuf::from(path));
                }
            }
            self.cgroup.clone()
        }

        /// A Docker container's name and whether kiln made it (its `kiln.runner-work` label), asked once.
        fn container(&mut self, id: &str) -> (String, bool) {
            if let Some(c) = self.docker.get(id) {
                return c.clone();
            }
            let said = std::process::Command::new("docker").args(["inspect", "-f", "{{.Name}}|{{index .Config.Labels \"kiln.runner-work\"}}", id]).output().ok().filter(|o| o.status.success());
            let c = said.map_or((String::new(), false), |o| {
                let text = String::from_utf8_lossy(&o.stdout).trim().to_owned();
                let (name, label) = text.split_once('|').unwrap_or((text.as_str(), ""));
                (name.trim_start_matches('/').to_owned(), !label.is_empty() && label != "<no value>")
            });
            self.docker.insert(id.to_owned(), c.clone());
            c
        }

        /// The services protected, by their cgroup relative to the root: every cgroup a level below the
        /// root's own (each slice's units; a top-level leaf such as init.scope itself), but the work's
        /// and this process's own and those under them, and containers kiln made; or, with containers
        /// named, those containers' alone.
        fn protected(&mut self) -> Vec<String> {
            let work = self.work().map(|p| p.display().to_string());
            let mine: Vec<String> = [work, self.own.clone()].into_iter().flatten().collect();
            let theirs = |c: &str| mine.iter().any(|m| m == c || m.starts_with(&format!("{c}/")));
            let dirs = |p: &Path| -> Vec<String> { std::fs::read_dir(p).map(|d| d.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default() };
            let mut out = Vec::new();
            for top in dirs(Path::new(ROOT)) {
                let kids = dirs(&Path::new(ROOT).join(&top));
                let candidates: Vec<String> = if kids.is_empty() { vec![top.clone()] } else { kids.into_iter().map(|k| format!("{top}/{k}")).collect() };
                for c in candidates {
                    if theirs(&c) {
                        continue;
                    }
                    let id = c.rsplit('/').next().and_then(|n| n.strip_prefix("docker-")).and_then(|n| n.strip_suffix(".scope")).map(str::to_owned);
                    match (&id, self.containers.is_empty()) {
                        (Some(id), true) => {
                            if !self.container(id).1 {
                                out.push(c);
                            }
                        }
                        (Some(id), false) => {
                            let name = self.container(id).0;
                            if self.containers.contains(&name) {
                                out.push(c);
                            }
                        }
                        (None, true) => out.push(c),
                        (None, false) => {}
                    }
                }
            }
            out
        }

        /// Each protected service's "full" stall on `r`, as its cgroup's pressure file says now.
        pub fn pressure(&mut self, r: Resource) -> Vec<Pressure> {
            self.protected()
                .into_iter()
                .filter_map(|c| {
                    let text = std::fs::read_to_string(Path::new(ROOT).join(&c).join(format!("{}.pressure", r.name()))).ok()?;
                    let (full_us, avg10) = full(&text)?;
                    Some(Pressure { cgroup: c, full_us, avg10 })
                })
                .collect()
        }

        /// The work's own counters, once its cgroup is known.
        pub fn own(&mut self) -> Option<Own> {
            let dir = Path::new(ROOT).join(self.work()?);
            let read = |f: &str| std::fs::read_to_string(dir.join(f)).ok();
            let key = |text: &str, k: &str| text.lines().find_map(|l| l.strip_prefix(k).and_then(|v| v.trim().parse::<f64>().ok()));
            let cpu = read("cpu.stat").and_then(|t| key(&t, "usage_usec "))?;
            let io = read("io.stat").map(|t| {
                t.lines().flat_map(|l| l.split_whitespace().filter_map(|kv| kv.strip_prefix("rbytes=").or_else(|| kv.strip_prefix("wbytes="))).filter_map(|v| v.parse::<f64>().ok())).fold(0.0, |a, b| a + b)
            });
            let stat = read("memory.stat").unwrap_or_default();
            let dirty = key(&stat, "file_dirty ").unwrap_or(0.0) + key(&stat, "file_writeback ").unwrap_or(0.0);
            let mem = read("memory.current").and_then(|t| t.trim().parse::<f64>().ok()).unwrap_or(0.0);
            Some(Own { cpu_us: cpu, io_bytes: io, dirty_bytes: dirty, mem_bytes: mem })
        }

        /// What the machine's disks wrote a second since the last read (whole devices' sectors, as
        /// /proc/diskstats counts them); none on the first.
        pub fn written_bps(&mut self) -> Option<f64> {
            let text = std::fs::read_to_string("/proc/diskstats").ok()?;
            let sectors: u64 = text
                .lines()
                .filter_map(|l| {
                    let f: Vec<&str> = l.split_whitespace().collect();
                    let name = f.get(2)?;
                    Path::new("/sys/block").join(name).join("device").exists().then(|| f.get(9).and_then(|v| v.parse::<u64>().ok())).flatten()
                })
                .sum();
            let now = std::time::Instant::now();
            let rate = self.disks.map(|(at, s0)| sectors.saturating_sub(s0) as f64 * 512.0 / now.duration_since(at).as_secs_f64().max(f64::MIN_POSITIVE));
            self.disks = Some((now, sectors));
            rate
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn the_online_cpus_read() {
            assert_eq!(cpu_count("0-31\n"), Some(32));
            assert_eq!(cpu_count("0-3,8-11"), Some(8));
            assert_eq!(cpu_count("5"), Some(1));
            assert_eq!(cpu_count("3-1"), None);
            assert_eq!(cpu_count(""), None);
            assert!(online_cpus() >= 1.0);
        }

        #[test]
        fn a_pressure_files_full_line_reads() {
            let text = "some avg10=1.50 avg60=0.70 avg300=0.20 total=8812345\nfull avg10=0.90 avg60=0.40 avg300=0.10 total=4412000\n";
            assert_eq!(full(text), Some((4_412_000.0, 0.9)));
            assert_eq!(full("some avg10=0.00 avg60=0.00 avg300=0.00 total=0\n"), None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_ci::protect::objective::FLOW_US;

    #[test]
    fn its_arguments_read_and_bad_ones_say_why() {
        let os = |v: &[&str]| v.iter().map(OsString::from).collect::<Vec<_>>();
        let a = Args::parse(&os(&["--unit", "kiln-gate-x", "--server", "http://127.0.0.1:7070", "--deadline-ms", "500", "--", "systemd-run", "--user"]));
        let a = a.unwrap_or_default();
        assert_eq!(a.unit.as_deref(), Some("kiln-gate-x"));
        assert_eq!(a.servers, vec!["http://127.0.0.1:7070".to_owned()]);
        assert_eq!(a.policy.deadline_us, 500_000.0);
        assert_eq!(a.policy.false_alarms_per_day, 1.0);
        assert_eq!(a.command, os(&["systemd-run", "--user"]));
        assert_eq!(Args::parse(&os(&["--deadline-ms", "0", "--", "x"])), Err("--deadline-ms must be more than 0".to_owned()));
        assert!(Args::parse(&os(&["--unit", "x"])).is_err_and(|e| e.contains("nothing to run")));
        assert!(Args::parse(&os(&["--bogus"])).is_err_and(|e| e.contains("no such option")));
        assert_eq!(Args::default().policy.deadline_us, FLOW_US);
    }

    #[test]
    fn a_servers_strain_url_is_its_own() {
        assert_eq!(strain_url("http://127.0.0.1:7070", 1000), Ok(("127.0.0.1:7070".to_owned(), "/healthz/strain?within_ms=1000".to_owned())));
        assert_eq!(strain_url("http://kiln/healthz", 250), Ok(("kiln:80".to_owned(), "/healthz/strain?within_ms=250".to_owned())));
        assert!(strain_url("https://kiln", 1).is_err());
    }

    /// The thin side end to end, on any unix (no PSI to arm off Linux, so stalls go unwatched): a
    /// command runs under the guard and its own code comes back; beside a server that doesn't answer
    /// it isn't started (75, and why); and a run's log makes the same decisions again.
    #[cfg(unix)]
    #[test]
    fn lb05_g11_kiln_protect_runs_refuses_and_replays() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let args = |cmd: &[&str], servers: Vec<String>, log: &str| Args {
            servers,
            state: Some(dir.path().join("guard.json")),
            log: Some(dir.path().join(log)),
            why: Some(dir.path().join(format!("{log}.why"))),
            command: cmd.iter().map(OsString::from).collect(),
            ..Args::default()
        };
        anyhow::ensure!(unix::run(args(&["true"], vec![], "ok.ndjson")).map_err(anyhow::Error::msg)? == 0);
        anyhow::ensure!(unix::run(args(&["false"], vec![], "fail.ndjson")).map_err(anyhow::Error::msg)? == 1);
        // A port nobody listens on: refused at once, so the command never runs.
        let closed = std::net::TcpListener::bind("127.0.0.1:0")?;
        let url = format!("http://{}", closed.local_addr()?);
        drop(closed);
        let marker = dir.path().join("ran");
        let touch = format!("touch {}", marker.display());
        anyhow::ensure!(unix::run(args(&["sh", "-c", &touch], vec![url], "refused.ndjson")).map_err(anyhow::Error::msg)? == STRAINED);
        anyhow::ensure!(!marker.exists(), "it started the command beside a server that doesn't answer");
        let why = std::fs::read_to_string(dir.path().join("refused.ndjson.why"))?;
        anyhow::ensure!(why.starts_with("not started:") && why.contains("doesn't answer"), "{why}");
        // Every run replays to the same decisions.
        for log in ["ok.ndjson", "fail.ndjson", "refused.ndjson"] {
            let (same, differ) = unix::replay(&dir.path().join(log)).map_err(anyhow::Error::msg)?;
            anyhow::ensure!(differ.is_empty() && same > 0, "{log}: {differ:?}");
        }
        Ok(())
    }

    /// A chunked stream split anywhere reads as the same lines: within a chunk, across chunks, a line
    /// broken over two.
    #[test]
    fn a_chunked_stream_reads_in_lines() {
        let body = b"5\r\n{\"a\":\r\n7\r\n1}\n{\"b\"\r\n3\r\n:2}\r\n2\r\n\n\n\r\n0\r\n\r\n";
        let mut b = Body { r: std::io::BufReader::new(&body[..]), chunked: true, left: 0, done: false };
        let mut lines = Vec::new();
        while let Ok(Some(l)) = b.line() {
            lines.push(String::from_utf8_lossy(&l).into_owned());
        }
        assert_eq!(lines, vec!["{\"a\":1}".to_owned(), "{\"b\":2}".to_owned(), String::new()]);
        let mut plain = Body { r: std::io::BufReader::new(&b"x\ny"[..]), chunked: false, left: 0, done: false };
        assert_eq!(plain.line().ok().flatten(), Some(b"x".to_vec()));
        assert_eq!(plain.line().ok().flatten(), Some(b"y".to_vec()));
        assert_eq!(plain.line().ok().flatten(), None);
    }
}
