//! `Sandbox`: a harness launched in a boundary (CP-T4.1; kiln plan 01a1cd8e862e7169a074db3d5fa33f3d,
//! step 17): its program, its folder and the folders shared with it, its environment, its limits
//! (memory, processes) and the hosts it may reach, with its stdio streamed back, the signals it's
//! sent, and its end said once, last. Its implementations are `host` (today's launch), the microVMs
//! kiln's harnesses run in (Shards' members, through Hex), then Hex's other classes (CP-T4.1,
//! CP-T7.2). A sandbox says what it holds its runs to ([`Holds`]): what it holds it enforces, and a
//! launch that asks for what it doesn't hold is refused `Unsupported`, never taken and ignored. The
//! suite asks its fixture for programs by what they do (echo, exit with a code, use memory, start
//! processes, connect), so each implementation supplies its own; [`SimSandbox`] runs built-in
//! programs that do those things.

use std::collections::VecDeque;
use std::sync::Arc;

use futures::future::BoxFuture;
use parking_lot::Mutex;

use crate::conformance::{Case, Failure, ensure, fine, same};

/// What a run may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Its memory. `None`: no limit of its boundary's own (a process on the host, as kiln runs
    /// harnesses today; a microVM sized as its boundary sizes one).
    pub memory_bytes: Option<u64>,
    /// Its processes, itself among them; `None` as for memory.
    pub processes: Option<u32>,
    /// Its share of the CPU against others', on cgroup v2's scale ([`WEIGHT_MIN`] to
    /// [`WEIGHT_MAX`]; [`WEIGHT_EVEN`] is an even share): a build held below an agent.
    pub cpu_weight: u16,
    /// Its share of the disk, on the same scale (2026-10-19's builds starved a disk, not a CPU).
    pub io_weight: u16,
}

/// The weights' scale (cgroup v2's `cpu.weight`, `io.weight`).
pub const WEIGHT_MIN: u16 = 1;
pub const WEIGHT_MAX: u16 = 10_000;
/// An even share (cgroup v2's default `cpu.weight` and `io.weight`).
pub const WEIGHT_EVEN: u16 = 100;

impl Default for Limits {
    fn default() -> Self {
        Self { memory_bytes: Some(1 << 30), processes: Some(64), cpu_weight: WEIGHT_EVEN, io_weight: WEIGHT_EVEN }
    }
}

impl Limits {
    /// No limits of the boundary's own, and even shares: a process on the host, as today.
    pub const NONE: Limits = Limits { memory_bytes: None, processes: None, cpu_weight: WEIGHT_EVEN, io_weight: WEIGHT_EVEN };
}

/// The hosts a run may reach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Egress {
    /// Whatever its boundary itself lets it reach, narrowed no further: any host from the host's;
    /// a microVM's image's own grants from a microVM (its Agentfile's, default deny), never wider.
    Any,
    /// These alone, each `host` or `host:port`; none at all when empty.
    Only(Vec<String>),
}

impl Default for Egress {
    fn default() -> Self {
        Egress::Only(Vec::new())
    }
}

/// A folder of the machine's a run sees: `host`'s files at `at`. At its own path (`at` is `host`),
/// what the run says of its files names them as the machine does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Share {
    pub host: String,
    pub at: String,
    pub writable: bool,
}

impl Share {
    /// `path`, writable, at its own path.
    pub fn at_its_path(path: &str) -> Self {
        Self { host: path.to_owned(), at: path.to_owned(), writable: true }
    }
}

/// A signal a run's program is sent: to its process group on the host, to a microVM's member
/// through its boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// SIGINT: stop what it's doing.
    Interrupt,
    /// SIGTERM: end, in its own time.
    Terminate,
}

impl Signal {
    /// Its number on Linux (signal(7)): what a microVM's Linux delivers, and what [`SimSandbox`]'s
    /// runs end by.
    pub const fn number(self) -> i32 {
        match self {
            Signal::Interrupt => 2,
            Signal::Terminate => 15,
        }
    }

    /// The code of a run it ended: 128 plus its number, as a POSIX shell says a command a signal
    /// ended (POSIX.1-2017, Shell Command Language §2.8.2), and as Docker reports one.
    pub const fn ended_code(self) -> i32 {
        match self {
            Signal::Interrupt => 130,
            Signal::Terminate => 143,
        }
    }
}

/// What a sandbox holds its runs to. What it holds it enforces; a launch that asks for what it
/// doesn't hold is refused [`Refusal::Unsupported`], never taken and ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Holds {
    /// A memory limit: a run over it ends `OverLimit(Memory)`.
    pub memory: bool,
    /// A process limit: a run refused one past it, that then fails, ends `OverLimit(Processes)`.
    pub processes: bool,
    /// Shares of the CPU and the disk other than an even one.
    pub weights: bool,
    /// No egress at all.
    pub no_egress: bool,
    /// Egress to named hosts alone.
    pub egress_by_host: bool,
    /// A folder shared at another path than its own, or read-only.
    pub shares: bool,
    /// Signals to a run ([`Run::signal`]).
    pub signals: bool,
}

impl Holds {
    /// Everything (the simulator's).
    pub const ALL: Holds = Holds { memory: true, processes: true, weights: true, no_egress: true, egress_by_host: true, shares: true, signals: true };

    /// `Ok` when `launch`'s weights are on their scale and it asks only for what these hold; else
    /// its refusal, `Invalid` or `Unsupported`, naming what it asked for.
    pub fn check(&self, launch: &Launch) -> Result<(), Refusal> {
        let l = &launch.limits;
        for (what, w) in [("a cpu weight", l.cpu_weight), ("an io weight", l.io_weight)] {
            if !(WEIGHT_MIN..=WEIGHT_MAX).contains(&w) {
                return Err(Refusal::Invalid { what, why: format!("{w} (from {WEIGHT_MIN} to {WEIGHT_MAX})") });
            }
        }
        let refuse = |what: &str| Err(Refusal::Unsupported(what.to_owned()));
        if l.memory_bytes.is_some() && !self.memory {
            return refuse("a memory limit");
        }
        if l.processes.is_some() && !self.processes {
            return refuse("a process limit");
        }
        if (l.cpu_weight != WEIGHT_EVEN || l.io_weight != WEIGHT_EVEN) && !self.weights {
            return refuse("a share of the CPU or the disk other than an even one");
        }
        if !self.shares && launch.shares.iter().any(|s| s.at != s.host || !s.writable) {
            return refuse("a folder shared at another path or read-only");
        }
        match &launch.egress {
            Egress::Any => Ok(()),
            Egress::Only(hosts) if hosts.is_empty() && !self.no_egress => refuse("a run with no egress"),
            Egress::Only(hosts) if !hosts.is_empty() && !self.egress_by_host => refuse("egress to named hosts alone"),
            Egress::Only(_) => Ok(()),
        }
    }
}

/// Where a run is held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Boundary {
    /// A process on the host, as kiln runs harnesses today.
    Host,
    /// An OCI image, run rootless (Hex's `oci` class).
    Container { image: String },
    /// A microVM from `image`: an Agentfile's, whose members run in their own domains as it declares
    /// them (Shards' D231), through Hex.
    MicroVm { image: String },
}

/// A run to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    /// What runs: a program by its path, or by a name its boundary finds it by (the host's `PATH`);
    /// in a microVM, the member of its image that runs (an Agentfile's HARNESS or AGENT, by name),
    /// which runs its image's command for it.
    pub program: String,
    /// Its arguments; in a microVM, those past its image's command for the member (none: the
    /// member runs as its image declares it).
    pub args: Vec<String>,
    /// Its working folder, which it may write.
    pub folder: Option<String>,
    /// More of the machine's folders it sees.
    pub shares: Vec<Share>,
    /// Its environment, over its boundary's own: the machine's on the host, its image's in a
    /// container or a microVM. Secrets don't belong here: they're injected outside the boundary
    /// (CP-T4.3).
    pub env: Vec<(String, String)>,
    pub limits: Limits,
    /// The hosts it may reach; every other is refused.
    pub egress: Egress,
    pub boundary: Boundary,
}

impl Launch {
    /// `program` with `args`, on the host, default limits, no egress.
    pub fn of(program: &str, args: &[&str]) -> Self {
        Self {
            program: program.to_owned(),
            args: args.iter().map(|a| (*a).to_owned()).collect(),
            folder: None,
            shares: Vec::new(),
            env: Vec::new(),
            limits: Limits::default(),
            egress: Egress::default(),
            boundary: Boundary::Host,
        }
    }
}

/// Which limit a run went over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    Memory,
    Processes,
}

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Code(i32),
    /// Killed, through `Run::kill`.
    Killed,
    /// Ended by a limit: memory when its boundary killed it for going over (cgroup v2's
    /// `memory.events` counting an `oom_kill`); processes when it was refused one past its limit
    /// (`pids.max`: the fork fails, the run isn't stopped) and then failed. A run refused a process
    /// that coped and ended well ends `Code(0)`.
    OverLimit(Limit),
}

/// What a run says, in order: its output, then its end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Exited(Exit),
}

/// Why a request was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("no program {0:?}")]
    NoSuchProgram(String),
    #[error("{what} isn't valid: {why}")]
    Invalid { what: &'static str, why: String },
    #[error("this sandbox can't hold {0}")]
    Unsupported(String),
    #[error("the sandbox runs its most ({limit})")]
    Full { limit: usize },
    #[error("the run has ended")]
    Ended,
}

/// The link a run's stdio rides into its boundary (a microVM's: Shards' agent control, kiln plan
/// 01a1cd8e862e7169a074db3d5fa33f3d step 11), as each end says it negotiated
/// ([`kiln_ci::facts::Negotiated`]): the controller's, on the machine, and the endpoint's own report
/// from inside; and the lanes its controller was granted on it as it dialed, by Shards' names
/// (`control`, `stdout:0`, `stderr:0`, `stdin:0`, `exec`, `socket`, `mcp:0`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub controller: kiln_ci::facts::Negotiated,
    pub endpoint: kiln_ci::facts::Negotiated,
    pub lanes: Vec<String>,
}

/// A run under way: what it's given on stdin, what it says (its output, then its end, once), the
/// signals it's sent, a way to end it, and where its boundary holds it.
pub trait Run: Send + Sync {
    /// Gives the program `bytes` on its stdin, after what it was given before; refused `Ended`
    /// once the run has ended.
    fn stdin<'a>(&'a self, bytes: Vec<u8>) -> BoxFuture<'a, Result<(), Refusal>>;
    /// Closes its stdin: the program reads the end of its input.
    fn close_stdin(&self);
    /// What it says next; `None` once its end has been given.
    fn next(&self) -> BoxFuture<'_, Option<Output>>;
    /// Sends its program `signal`, which it handles as it does (most end); refused `Ended` once it
    /// has ended, and `Unsupported` where its boundary delivers none, the run going on unsignalled.
    fn signal(&self, signal: Signal) -> Result<(), Refusal>;
    /// Ends it, unless it has ended: its end is then `Exited(Killed)`. A microVM's run ends with its
    /// microVM.
    fn kill(&self);
    /// Its boundary's own name for it, where its boundary names its runs (a microVM's, as its runtime
    /// names it); none for a process of the machine's, which its pid names. The same each time.
    fn id(&self) -> Option<String>;
    /// The link its stdio rides into its boundary, as each end says it negotiated, once it's made: it
    /// waits for the link's handshake and its endpoint's report. None at once for a run whose stdio
    /// ride no link (a process of the machine's, whose stdio are its pipes), and none where its link
    /// ends before it's made. Asked again, it says the same.
    fn link(&self) -> BoxFuture<'_, Option<Link>>;
}

/// Harnesses launched in boundaries.
pub trait Sandbox: Send + Sync + 'static {
    /// Starts `launch` in its boundary, within its limits; refused, typed, when it can't.
    fn launch<'a>(&'a self, launch: Launch) -> BoxFuture<'a, Result<Box<dyn Run>, Refusal>>;
    /// The boundaries it can hold runs in, by name (`host`, `container`, `microvm`).
    fn boundaries(&self) -> Vec<&'static str>;
    /// What it holds its runs to; a launch asking for more is refused `Unsupported`.
    fn holds(&self) -> Holds;
}

// ---------------------------------------------------------------------------------- simulator

/// Most runs a [`SimSandbox`] holds at once.
pub const SIM_RUNS: usize = 1000;
/// Most output a simulated run keeps unread; past it, the run is killed (its reader too slow).
pub const SIM_PENDING: usize = 10_000;

/// Built-in programs, run in memory:
///
/// - `say <words…>`: writes them to stdout, exits 0;
/// - `echo`: writes back what comes in on stdin, exits 0 once it closes;
/// - `fail <code>`: writes to stderr, exits with `code`;
/// - `wait`: runs until killed;
/// - `hog <bytes>`: uses that much memory, then exits 0;
/// - `spawn <n>`: starts `n` processes, then exits 0;
/// - `connect <host>`: exits 0 when the host is granted (or any is), 1 when it's refused;
/// - `pwd`: writes its working folder.
///
/// A signal ends any of them with its [`Signal::ended_code`]. It holds everything, unless made
/// [`SimSandbox::holding`] less, as a real boundary may.
#[derive(Clone)]
pub struct SimSandbox {
    runs: Arc<Mutex<usize>>,
    holds: Holds,
}

impl Default for SimSandbox {
    fn default() -> Self {
        Self { runs: Arc::default(), holds: Holds::ALL }
    }
}

impl SimSandbox {
    pub fn new() -> Self {
        Self::default()
    }

    /// One that holds only `holds`, refusing the rest `Unsupported`.
    pub fn holding(holds: Holds) -> Self {
        Self { holds, ..Self::default() }
    }
}

struct SimRun {
    state: Arc<Mutex<RunState>>,
    said: Arc<event_listener::Event>,
    runs: Arc<Mutex<usize>>,
    signals: bool,
}

#[derive(Default)]
struct RunState {
    out: VecDeque<Output>,
    echo: bool,
    ended: bool,
    given_end: bool,
}

impl RunState {
    fn say(&mut self, o: Output) {
        if self.ended {
            return;
        }
        if self.out.len() >= SIM_PENDING {
            self.out.push_back(Output::Exited(Exit::Killed));
            self.ended = true;
            return;
        }
        if matches!(o, Output::Exited(_)) {
            self.ended = true;
        }
        self.out.push_back(o);
    }
}

impl Drop for SimRun {
    fn drop(&mut self) {
        let mut n = self.runs.lock();
        *n = n.saturating_sub(1);
    }
}

impl Run for SimRun {
    fn stdin<'a>(&'a self, bytes: Vec<u8>) -> BoxFuture<'a, Result<(), Refusal>> {
        let out = {
            let mut st = self.state.lock();
            if st.ended {
                Err(Refusal::Ended)
            } else {
                if st.echo {
                    st.say(Output::Stdout(bytes));
                }
                Ok(())
            }
        };
        self.said.notify(usize::MAX);
        Box::pin(async move { out })
    }

    fn close_stdin(&self) {
        {
            let mut st = self.state.lock();
            if st.echo {
                st.say(Output::Exited(Exit::Code(0)));
            }
        }
        self.said.notify(usize::MAX);
    }

    fn next(&self) -> BoxFuture<'_, Option<Output>> {
        Box::pin(async move {
            loop {
                let listener = self.said.listen();
                {
                    let mut st = self.state.lock();
                    if let Some(o) = st.out.pop_front() {
                        if matches!(o, Output::Exited(_)) {
                            st.given_end = true;
                        }
                        return Some(o);
                    }
                    if st.given_end {
                        return None;
                    }
                }
                listener.await;
            }
        })
    }

    fn signal(&self, signal: Signal) -> Result<(), Refusal> {
        {
            let mut st = self.state.lock();
            if st.ended {
                return Err(Refusal::Ended);
            }
            if !self.signals {
                return Err(Refusal::Unsupported("a signal to a run".into()));
            }
            st.say(Output::Exited(Exit::Code(signal.ended_code())));
        }
        self.said.notify(usize::MAX);
        Ok(())
    }

    fn kill(&self) {
        self.state.lock().say(Output::Exited(Exit::Killed));
        self.said.notify(usize::MAX);
    }

    // Its runs are the host boundary's: no name of a boundary's, and their stdio no link.
    fn id(&self) -> Option<String> {
        None
    }

    fn link(&self) -> BoxFuture<'_, Option<Link>> {
        Box::pin(async { None })
    }
}

impl Sandbox for SimSandbox {
    fn launch<'a>(&'a self, launch: Launch) -> BoxFuture<'a, Result<Box<dyn Run>, Refusal>> {
        let out = (|| {
            self.holds().check(&launch)?;
            match launch.boundary {
                Boundary::Host => {}
                Boundary::Container { .. } => return Err(Refusal::Unsupported("a run in a container".into())),
                Boundary::MicroVm { .. } => return Err(Refusal::Unsupported("a run in a microVM".into())),
            }
            let arg = |i: usize| launch.args.get(i).cloned().unwrap_or_default();
            let mut st = RunState::default();
            match launch.program.as_str() {
                "say" => {
                    st.say(Output::Stdout(launch.args.join(" ").into_bytes()));
                    st.say(Output::Exited(Exit::Code(0)));
                }
                "echo" => st.echo = true,
                "fail" => {
                    let code: i32 = arg(0).parse().map_err(|_| Refusal::Invalid { what: "fail's code", why: arg(0) })?;
                    st.say(Output::Stderr(format!("failing with {code}").into_bytes()));
                    st.say(Output::Exited(Exit::Code(code)));
                }
                "wait" => {}
                "hog" => {
                    let bytes: u64 = arg(0).parse().map_err(|_| Refusal::Invalid { what: "hog's bytes", why: arg(0) })?;
                    let over = launch.limits.memory_bytes.is_some_and(|m| bytes > m);
                    st.say(if over { Output::Exited(Exit::OverLimit(Limit::Memory)) } else { Output::Exited(Exit::Code(0)) });
                }
                "spawn" => {
                    let n: u32 = arg(0).parse().map_err(|_| Refusal::Invalid { what: "spawn's count", why: arg(0) })?;
                    // It and what it starts.
                    let over = launch.limits.processes.is_some_and(|p| n.saturating_add(1) > p);
                    st.say(if over { Output::Exited(Exit::OverLimit(Limit::Processes)) } else { Output::Exited(Exit::Code(0)) });
                }
                "connect" => {
                    let host = arg(0);
                    let granted = match &launch.egress {
                        Egress::Any => true,
                        Egress::Only(hosts) => hosts.contains(&host),
                    };
                    if granted {
                        st.say(Output::Stdout(format!("connected to {host}").into_bytes()));
                        st.say(Output::Exited(Exit::Code(0)));
                    } else {
                        st.say(Output::Stderr(format!("{host}: refused").into_bytes()));
                        st.say(Output::Exited(Exit::Code(1)));
                    }
                }
                "pwd" => {
                    st.say(Output::Stdout(launch.folder.clone().unwrap_or_else(|| "/".into()).into_bytes()));
                    st.say(Output::Exited(Exit::Code(0)));
                }
                other => return Err(Refusal::NoSuchProgram(other.to_owned())),
            }
            let mut n = self.runs.lock();
            if *n >= SIM_RUNS {
                return Err(Refusal::Full { limit: SIM_RUNS });
            }
            *n = n.saturating_add(1);
            let run: Box<dyn Run> = Box::new(SimRun { state: Arc::new(Mutex::new(st)), said: Arc::new(event_listener::Event::new()), runs: Arc::clone(&self.runs), signals: self.holds.signals });
            Ok(run)
        })();
        Box::pin(async move { out })
    }

    fn boundaries(&self) -> Vec<&'static str> {
        vec!["host"]
    }

    fn holds(&self) -> Holds {
        self.holds
    }
}

// -------------------------------------------------------------------------------- conformance

/// What a `Sandbox` conformance suite runs against: launches for programs that do what each case
/// needs, in this sandbox's terms.
pub trait SandboxFixture: Send + Sync {
    fn sandbox(&self) -> &dyn Sandbox;
    /// Writes `text` to stdout and exits 0.
    fn say(&self, text: &str) -> Launch;
    /// Writes back what comes in on stdin; exits 0 once it closes.
    fn echo(&self) -> Launch;
    /// Writes to stderr and exits with `code`.
    fn fail(&self, code: i32) -> Launch;
    /// Runs until killed, or a signal ends it.
    fn wait(&self) -> Launch;
    /// Uses `bytes` of memory, under a limit of `limit` bytes (none: no limit).
    fn hog(&self, bytes: u64, limit: Option<u64>) -> Launch;
    /// Starts `n` processes, under a limit of `limit` (none: no limit), and fails when one is
    /// refused it.
    fn spawn(&self, n: u32, limit: Option<u32>) -> Launch;
    /// Connects to `host` (the suite's names, `api.anthropic.com` and `example.com`, which the
    /// fixture may serve itself), granted `egress`.
    fn connect(&self, host: &str, egress: Egress) -> Launch;
    /// Writes its working folder, given `folder`.
    fn pwd(&self, folder: &str) -> Launch;
    /// A folder that exists for a run.
    fn folder(&self) -> String;
}

/// The fixture for a [`SimSandbox`].
#[derive(Default)]
pub struct SimSandboxFixture(pub SimSandbox);

impl SandboxFixture for SimSandboxFixture {
    fn sandbox(&self) -> &dyn Sandbox {
        &self.0
    }
    fn say(&self, text: &str) -> Launch {
        Launch::of("say", &[text])
    }
    fn echo(&self) -> Launch {
        Launch::of("echo", &[])
    }
    fn fail(&self, code: i32) -> Launch {
        Launch::of("fail", &[&code.to_string()])
    }
    fn wait(&self) -> Launch {
        Launch::of("wait", &[])
    }
    fn hog(&self, bytes: u64, limit: Option<u64>) -> Launch {
        Launch { limits: Limits { memory_bytes: limit, ..Limits::default() }, ..Launch::of("hog", &[&bytes.to_string()]) }
    }
    fn spawn(&self, n: u32, limit: Option<u32>) -> Launch {
        Launch { limits: Limits { processes: limit, ..Limits::default() }, ..Launch::of("spawn", &[&n.to_string()]) }
    }
    fn connect(&self, host: &str, egress: Egress) -> Launch {
        Launch { egress, ..Launch::of("connect", &[host]) }
    }
    fn pwd(&self, folder: &str) -> Launch {
        Launch { folder: Some(folder.to_owned()), ..Launch::of("pwd", &[]) }
    }
    fn folder(&self) -> String {
        "/work/conformance".into()
    }
}

/// Everything a run says until its end (at most 1000 outputs).
async fn drain(run: &dyn Run) -> Vec<Output> {
    let mut out = Vec::new();
    while out.len() < 1000 {
        match run.next().await {
            Some(o) => out.push(o),
            None => break,
        }
    }
    out
}

fn stdout(outs: &[Output]) -> Vec<u8> {
    outs.iter().filter_map(|o| if let Output::Stdout(b) = o { Some(b.clone()) } else { None }).flatten().collect()
}

fn end(outs: &[Output]) -> Option<Exit> {
    match outs.last() {
        Some(Output::Exited(e)) => Some(*e),
        _ => None,
    }
}

fn c1<F: SandboxFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let run = fine("launch", f.sandbox().launch(f.say("hello there")).await)?;
        let outs = drain(run.as_ref()).await;
        same("its stdout", String::from_utf8_lossy(&stdout(&outs)).trim().to_owned(), "hello there".to_owned())?;
        same("its end", end(&outs), Some(Exit::Code(0)))?;
        same("after its end", run.next().await, None)
    })
}

fn c2<F: SandboxFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let run = fine("launch", f.sandbox().launch(f.echo()).await)?;
        for line in ["one\n", "two\n", "three\n"] {
            fine("stdin", run.stdin(line.as_bytes().to_vec()).await)?;
        }
        run.close_stdin();
        let outs = drain(run.as_ref()).await;
        same("what came back", String::from_utf8_lossy(&stdout(&outs)).into_owned(), "one\ntwo\nthree\n".to_owned())?;
        same("its end once stdin closed", end(&outs), Some(Exit::Code(0)))
    })
}

fn c3<F: SandboxFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let run = fine("launch", f.sandbox().launch(f.fail(3)).await)?;
        let outs = drain(run.as_ref()).await;
        ensure(outs.iter().any(|o| matches!(o, Output::Stderr(b) if !b.is_empty())), || "nothing on stderr".into())?;
        same("its end", end(&outs), Some(Exit::Code(3)))?;
        same("ends said", outs.iter().filter(|o| matches!(o, Output::Exited(_))).count(), 1)
    })
}

fn c4<F: SandboxFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let run = fine("launch", f.sandbox().launch(f.wait()).await)?;
        run.kill();
        let outs = drain(run.as_ref()).await;
        same("its end", end(&outs), Some(Exit::Killed))?;
        ensure(run.stdin(b"late".to_vec()).await.is_err(), || "stdin to an ended run was taken".into())
    })
}

/// `Ok` when `v` was refused `Unsupported`, else a failure saying what it was.
fn unsupported(what: &str, v: Result<Box<dyn Run>, Refusal>) -> Result<(), Failure> {
    match v {
        Err(Refusal::Unsupported(_)) => Ok(()),
        Err(e) => Err(format!("{what}: refused as {e:?}, not Unsupported")),
        Ok(_) => Err(format!("{what}: taken, though the sandbox says it doesn't hold it")),
    }
}

fn c5<F: SandboxFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        if !f.sandbox().holds().memory {
            unsupported("a memory limit", f.sandbox().launch(f.hog(4 << 20, Some(64 << 20))).await)?;
            let free = fine("launch", f.sandbox().launch(f.hog(4 << 20, None)).await)?;
            return same("with no limit", end(&drain(free.as_ref()).await), Some(Exit::Code(0)));
        }
        let over = fine("launch", f.sandbox().launch(f.hog(64 << 20, Some(16 << 20))).await)?;
        same("over its memory limit", end(&drain(over.as_ref()).await), Some(Exit::OverLimit(Limit::Memory)))?;
        let under = fine("launch", f.sandbox().launch(f.hog(4 << 20, Some(64 << 20))).await)?;
        same("under it", end(&drain(under.as_ref()).await), Some(Exit::Code(0)))
    })
}

fn c6<F: SandboxFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        if !f.sandbox().holds().processes {
            unsupported("a process limit", f.sandbox().launch(f.spawn(2, Some(8))).await)?;
            let free = fine("launch", f.sandbox().launch(f.spawn(2, None)).await)?;
            return same("with no limit", end(&drain(free.as_ref()).await), Some(Exit::Code(0)));
        }
        let over = fine("launch", f.sandbox().launch(f.spawn(20, Some(8))).await)?;
        same("over its process limit", end(&drain(over.as_ref()).await), Some(Exit::OverLimit(Limit::Processes)))?;
        let under = fine("launch", f.sandbox().launch(f.spawn(2, Some(8))).await)?;
        same("under it", end(&drain(under.as_ref()).await), Some(Exit::Code(0)))
    })
}

fn c7<F: SandboxFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let holds = f.sandbox().holds();
        let api = || Egress::Only(vec!["api.anthropic.com".to_owned()]);
        if holds.egress_by_host {
            let denied = fine("launch", f.sandbox().launch(f.connect("example.com", api())).await)?;
            ensure(!matches!(end(&drain(denied.as_ref()).await), Some(Exit::Code(0))), || "a host not granted was reached".into())?;
            let granted = fine("launch", f.sandbox().launch(f.connect("api.anthropic.com", api())).await)?;
            same("a granted host", end(&drain(granted.as_ref()).await), Some(Exit::Code(0)))?;
        } else {
            unsupported("egress to named hosts", f.sandbox().launch(f.connect("api.anthropic.com", api())).await)?;
        }
        if holds.no_egress {
            let cut = fine("launch", f.sandbox().launch(f.connect("api.anthropic.com", Egress::Only(Vec::new()))).await)?;
            ensure(!matches!(end(&drain(cut.as_ref()).await), Some(Exit::Code(0))), || "a run granted no host reached one".into())?;
        } else {
            unsupported("no egress", f.sandbox().launch(f.connect("api.anthropic.com", Egress::Only(Vec::new()))).await)?;
        }
        let any = fine("launch", f.sandbox().launch(f.connect("api.anthropic.com", Egress::Any)).await)?;
        same("a host, granted any", end(&drain(any.as_ref()).await), Some(Exit::Code(0)))
    })
}

fn c8<F: SandboxFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let mut l = f.say("x");
        l.program = "conformance-no-such-program".into();
        match f.sandbox().launch(l).await {
            Err(Refusal::NoSuchProgram(_)) => Ok(()),
            Err(e) => Err(format!("refused as {e:?}, not NoSuchProgram")),
            Ok(_) => Err("a program that doesn't exist was launched".into()),
        }
    })
}

fn c9<F: SandboxFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let folder = f.folder();
        let run = fine("launch", f.sandbox().launch(f.pwd(&folder)).await)?;
        let outs = drain(run.as_ref()).await;
        same("its working folder", String::from_utf8_lossy(&stdout(&outs)).trim().to_owned(), folder)
    })
}

fn c10<F: SandboxFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        for (cpu, io) in [(0, 100), (100, 0), (WEIGHT_MAX.saturating_add(1), 100), (100, WEIGHT_MAX.saturating_add(1))] {
            let mut l = f.say("x");
            l.limits.cpu_weight = cpu;
            l.limits.io_weight = io;
            match f.sandbox().launch(l).await {
                Err(Refusal::Invalid { .. }) => {}
                Err(e) => return Err(format!("weights {cpu}/{io} refused as {e:?}, not Invalid")),
                Ok(_) => return Err(format!("weights {cpu}/{io} were taken")),
            }
        }
        let mut l = f.say("weighted");
        (l.limits.cpu_weight, l.limits.io_weight) = (WEIGHT_MIN, WEIGHT_MAX);
        if !f.sandbox().holds().weights {
            unsupported("weights other than an even share", f.sandbox().launch(l).await)?;
            let even = fine("a launch at even shares", f.sandbox().launch(f.say("even")).await)?;
            return same("its end", end(&drain(even.as_ref()).await), Some(Exit::Code(0)));
        }
        let run = fine("a launch at the weights' ends", f.sandbox().launch(l).await)?;
        same("its end", end(&drain(run.as_ref()).await), Some(Exit::Code(0)))
    })
}

fn c11<F: SandboxFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let run = fine("launch", f.sandbox().launch(f.wait()).await)?;
        if !f.sandbox().holds().signals {
            match run.signal(Signal::Terminate) {
                Err(Refusal::Unsupported(_)) => {}
                Err(e) => return Err(format!("a signal refused as {e:?}, not Unsupported")),
                Ok(()) => return Err("a signal taken, though the sandbox says it doesn't hold it".into()),
            }
            // Unsignalled, it goes on: only its kill ends it.
            run.kill();
            return same("its end, unsignalled", end(&drain(run.as_ref()).await), Some(Exit::Killed));
        }
        fine("a signal", run.signal(Signal::Terminate))?;
        let outs = drain(run.as_ref()).await;
        ensure(matches!(end(&outs), Some(Exit::Code(_) | Exit::Killed)), || format!("no end after its signal: {outs:?}"))?;
        same("ends said", outs.iter().filter(|o| matches!(o, Output::Exited(_))).count(), 1)?;
        match run.signal(Signal::Terminate) {
            Err(Refusal::Ended) => Ok(()),
            other => Err(format!("a signal to an ended run: {other:?}, not Ended")),
        }
    })
}

fn c12<F: SandboxFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let launch = f.say("where");
        let host = launch.boundary == Boundary::Host;
        let run = fine("launch", f.sandbox().launch(launch).await)?;
        let (id, link) = (run.id(), run.link().await);
        if host {
            same("a process of the machine's id", id.clone(), None)?;
            same("a process of the machine's link", link.clone(), None)?;
        }
        drain(run.as_ref()).await;
        same("its id, asked again after its end", run.id(), id)?;
        same("its link, asked again after its end", run.link().await, link)
    })
}

/// The `Sandbox` contract, clause by clause (docs/seams.md, "Sandbox").
pub fn cases<F: SandboxFixture>() -> Vec<Case<F>> {
    vec![
        Case { id: "SANDBOX-1", says: "what a run writes reaches its stdout in order, then its end, then nothing", check: c1::<F> },
        Case { id: "SANDBOX-2", says: "stdin reaches the program in order, and closing it is the end of its input", check: c2::<F> },
        Case { id: "SANDBOX-3", says: "a run's stderr and exit code are said as they are, and its end comes once, last", check: c3::<F> },
        Case { id: "SANDBOX-4", says: "a run killed ends Killed, and takes no more input", check: c4::<F> },
        Case {
            id: "SANDBOX-5",
            says: "a run over its memory limit is ended OverLimit(Memory); under it, it runs to its end; a sandbox that holds no memory limit refuses one Unsupported, and runs a launch without one",
            check: c5::<F>,
        },
        Case {
            id: "SANDBOX-6",
            says: "a run refused processes past its process limit, that then fails, ends OverLimit(Processes); under it, it runs to its end; a sandbox that holds no process limit refuses one Unsupported, and runs a launch without one",
            check: c6::<F>,
        },
        Case {
            id: "SANDBOX-7",
            says: "a run reaches only the hosts it's granted, none when granted none, and a host when granted any; a sandbox refuses Unsupported the grants it doesn't hold",
            check: c7::<F>,
        },
        Case { id: "SANDBOX-8", says: "a launch of a program that doesn't exist is refused NoSuchProgram", check: c8::<F> },
        Case { id: "SANDBOX-9", says: "a run works in the folder it's given", check: c9::<F> },
        Case {
            id: "SANDBOX-10",
            says: "cpu and io weights outside cgroup v2's 1 to 10,000 are refused Invalid; at its ends a run goes ahead where the sandbox holds weights, and is refused Unsupported where it doesn't",
            check: c10::<F>,
        },
        Case {
            id: "SANDBOX-11",
            says: "a run sent a signal to end ends, its end said once, and a signal after its end is refused Ended; a sandbox that delivers no signals refuses one Unsupported, and its run goes on",
            check: c11::<F>,
        },
        Case {
            id: "SANDBOX-12",
            says: "a run on the host has no id of a boundary's and no link, and a run says the same id and link after its end as before",
            check: c12::<F>,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::run;

    /// CP-M2, AC-M2.2: the simulator and its suite.
    #[test]
    fn the_simulated_sandbox_passes_the_sandbox_suite() -> Result<(), String> {
        futures::executor::block_on(run("Sandbox", &cases::<SimSandboxFixture>(), || async { SimSandboxFixture::default() })).into_result().map(|_| ())
    }

    /// A simulator for the suite's other half: launches without limits or grants (a host's), the
    /// sandbox under test said to hold `holds`, and `lies` a sandbox that takes everything anyway.
    struct Holding {
        sim: SimSandboxFixture,
        lies: Option<Liar>,
    }

    /// Says it holds nothing, and takes everything.
    struct Liar(SimSandbox);

    impl Sandbox for Liar {
        fn launch<'a>(&'a self, launch: Launch) -> BoxFuture<'a, Result<Box<dyn Run>, Refusal>> {
            self.0.launch(launch)
        }
        fn boundaries(&self) -> Vec<&'static str> {
            self.0.boundaries()
        }
        fn holds(&self) -> Holds {
            Holds::default()
        }
    }

    fn free(l: Launch) -> Launch {
        Launch { limits: Limits::NONE, egress: Egress::Any, ..l }
    }

    impl SandboxFixture for Holding {
        fn sandbox(&self) -> &dyn Sandbox {
            match &self.lies {
                Some(l) => l,
                None => self.sim.sandbox(),
            }
        }
        fn say(&self, text: &str) -> Launch {
            free(self.sim.say(text))
        }
        fn echo(&self) -> Launch {
            free(self.sim.echo())
        }
        fn fail(&self, code: i32) -> Launch {
            free(self.sim.fail(code))
        }
        fn wait(&self) -> Launch {
            free(self.sim.wait())
        }
        fn hog(&self, bytes: u64, limit: Option<u64>) -> Launch {
            Launch { limits: Limits { memory_bytes: limit, ..Limits::NONE }, egress: Egress::Any, ..self.sim.hog(bytes, limit) }
        }
        fn spawn(&self, n: u32, limit: Option<u32>) -> Launch {
            Launch { limits: Limits { processes: limit, ..Limits::NONE }, egress: Egress::Any, ..self.sim.spawn(n, limit) }
        }
        fn connect(&self, host: &str, egress: Egress) -> Launch {
            Launch { limits: Limits::NONE, ..self.sim.connect(host, egress) }
        }
        fn pwd(&self, folder: &str) -> Launch {
            free(self.sim.pwd(folder))
        }
        fn folder(&self) -> String {
            self.sim.folder()
        }
    }

    /// CP-T4.1: a sandbox that holds nothing (the host's) passes the suite by refusing, typed, what
    /// it doesn't hold, and running what asks for none of it.
    #[test]
    fn a_sandbox_that_holds_nothing_passes_the_suite_refusing_what_it_does_not_hold() -> Result<(), String> {
        let make = || async { Holding { sim: SimSandboxFixture(SimSandbox::holding(Holds::default())), lies: None } };
        futures::executor::block_on(run("Sandbox", &cases::<Holding>(), make)).into_result().map(|_| ())
    }

    /// CP-T4.1: the suite isn't vacuous there: a sandbox that says it holds nothing but takes limits
    /// and grants anyway fails each clause about them, and only those.
    #[test]
    fn a_sandbox_that_takes_what_it_says_it_does_not_hold_fails_those_clauses() {
        let make = || async { Holding { sim: SimSandboxFixture::default(), lies: Some(Liar(SimSandbox::new())) } };
        let report = futures::executor::block_on(run("Sandbox", &cases::<Holding>(), make));
        let failed: Vec<&str> = report.failed.iter().map(|(id, _)| *id).collect();
        assert_eq!(failed, ["SANDBOX-5", "SANDBOX-6", "SANDBOX-7", "SANDBOX-10", "SANDBOX-11"], "{:?}", report.failed);
        assert!(report.failed.iter().all(|(_, why)| why.contains("doesn't hold it")), "{:?}", report.failed);
    }

    /// CP-T4.1: what each kind of launch is refused by a sandbox that holds only part of it.
    #[test]
    fn holds_refuse_what_they_do_not_hold_by_name() {
        let only_memory = Holds { memory: true, ..Holds::default() };
        let l = |limits: Limits, egress: Egress| Launch { limits, egress, ..Launch::of("say", &["x"]) };
        assert_eq!(only_memory.check(&l(Limits { processes: None, ..Limits::default() }, Egress::Any)), Ok(()));
        assert_eq!(only_memory.check(&l(Limits::default(), Egress::Any)), Err(Refusal::Unsupported("a process limit".into())));
        assert_eq!(only_memory.check(&l(Limits::NONE, Egress::default())), Err(Refusal::Unsupported("a run with no egress".into())));
        assert_eq!(only_memory.check(&l(Limits::NONE, Egress::Only(vec!["h".into()]))), Err(Refusal::Unsupported("egress to named hosts alone".into())));
        let weighted = Limits { cpu_weight: WEIGHT_MIN, ..Limits::NONE };
        assert!(matches!(only_memory.check(&l(weighted, Egress::Any)), Err(Refusal::Unsupported(_))));
        assert!(matches!(only_memory.check(&l(Limits { cpu_weight: 0, ..Limits::NONE }, Egress::Any)), Err(Refusal::Invalid { .. })), "a weight off the scale is invalid before it's unheld");
        assert_eq!(Holds::ALL.check(&l(Limits { cpu_weight: WEIGHT_MAX, ..Limits::default() }, Egress::Only(vec!["h".into()]))), Ok(()));
        // A folder at its own path, writable, is what a machine's own process sees anyway; at
        // another path, or read-only, only where shares are held.
        let shared = |s: Share| Launch { shares: vec![s], ..l(Limits::NONE, Egress::Any) };
        assert_eq!(only_memory.check(&shared(Share::at_its_path("/p"))), Ok(()));
        let moved = Share { at: "/workspace".into(), ..Share::at_its_path("/p") };
        assert_eq!(only_memory.check(&shared(moved.clone())), Err(Refusal::Unsupported("a folder shared at another path or read-only".into())));
        let read_only = Share { writable: false, ..Share::at_its_path("/p") };
        assert_eq!(only_memory.check(&shared(read_only)), Err(Refusal::Unsupported("a folder shared at another path or read-only".into())));
        assert_eq!(Holds::ALL.check(&shared(moved)), Ok(()));
    }

    /// Step 17: a signal ends a simulated run with the code a shell says of it, said once; once
    /// it has ended, a signal is refused `Ended`.
    #[test]
    fn a_signal_ends_a_run_with_its_shells_code() -> Result<(), Refusal> {
        let s = SimSandbox::new();
        for signal in [Signal::Interrupt, Signal::Terminate] {
            let run = futures::executor::block_on(s.launch(Launch::of("wait", &[])))?;
            run.signal(signal)?;
            let outs = futures::executor::block_on(drain(run.as_ref()));
            assert_eq!(end(&outs), Some(Exit::Code(128 + signal.number())), "{signal:?}");
            assert_eq!(run.signal(signal), Err(Refusal::Ended));
        }
        Ok(())
    }

    /// CP-M2, AC-M2.2: the simulator, bounded, refusing typed past its bounds.
    #[test]
    fn a_microvm_is_refused_as_unsupported_and_runs_are_bounded() {
        let s = SimSandbox::new();
        let vm = Launch { boundary: Boundary::MicroVm { image: "x".into() }, ..Launch::of("say", &["x"]) };
        assert!(matches!(futures::executor::block_on(s.launch(vm)), Err(Refusal::Unsupported(_))));
        let container = Launch { boundary: Boundary::Container { image: "x".into() }, ..Launch::of("say", &["x"]) };
        assert!(matches!(futures::executor::block_on(s.launch(container)), Err(Refusal::Unsupported(_))));
        let runs: Vec<_> = (0..SIM_RUNS).filter_map(|_| futures::executor::block_on(s.launch(Launch::of("wait", &[]))).ok()).collect();
        assert_eq!(runs.len(), SIM_RUNS);
        assert!(matches!(futures::executor::block_on(s.launch(Launch::of("wait", &[]))), Err(Refusal::Full { .. })));
        drop(runs);
        assert!(futures::executor::block_on(s.launch(Launch::of("wait", &[]))).is_ok(), "an ended run frees its place");
    }
}
