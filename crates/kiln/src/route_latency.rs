//! How long each class of the server's work waits and takes, on `/metrics` (goal 1, plan
//! 01a1a59c2a68 step 2): a histogram for each route class of how long its work waited before its
//! handler started (`kiln_route_wait_seconds`) and how long the handler then took
//! (`kiln_route_seconds`).
//!
//! The classes, and where each is timed:
//! - `page`: the UI's pages and files (a GET or HEAD outside `/api`), and `api_read` and `api_write`:
//!   the API's reads (GET, HEAD) and changes (the rest, the MCP endpoint's calls among them). From the
//!   request's head arriving off its socket (kiln-http's [`kiln_http::Received`]) to its handler's
//!   start, then to its response's head. A WebSocket's opening isn't one: its frames are.
//! - `websocket`: the frames of an agent session's live feed going out to a viewer's socket. From the
//!   frame's publish on the bus to the viewer's task reading it, then to its write on the socket.
//! - `runner`: what runners send. A message off a runner's link, from the network thread handing it
//!   over to the hub starting on it, then to the hub having applied it; and runners' and their jobs'
//!   own HTTP routes (enrolling, sync's contents, artifacts, CI's runtime services), as requests are.
//!
//! Every histogram is atomics only ([`crate::metrics::Histogram`], a copy for each shard: [`PerShard`]): no lock on any
//! path that times or reads one.
//! Time a request waited in its socket while its shard was busy shows in `kiln_shard_delay_seconds`
//! (`crate::shard_health`): the runtime doesn't say when a task was woken.

use std::fmt::Write as _;
use std::sync::OnceLock;
use std::time::Duration;

use http::Method;

use crate::metrics::{PerShard, STALL_BUCKETS};

/// What the server's work is, as timed here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Page,
    ApiRead,
    ApiWrite,
    Websocket,
    Runner,
}

impl Class {
    pub const ALL: [Class; 5] = [Class::Page, Class::ApiRead, Class::ApiWrite, Class::Websocket, Class::Runner];

    pub fn name(self) -> &'static str {
        match self {
            Class::Page => "page",
            Class::ApiRead => "api_read",
            Class::ApiWrite => "api_write",
            Class::Websocket => "websocket",
            Class::Runner => "runner",
        }
    }

    fn index(self) -> usize {
        match self {
            Class::Page => 0,
            Class::ApiRead => 1,
            Class::ApiWrite => 2,
            Class::Websocket => 3,
            Class::Runner => 4,
        }
    }
}

/// Each class's histograms, in [`Class::ALL`]'s order: each shard times its work into copies of its own, summed as
/// they're read ([`PerShard`]; goal 9, plan 01a1a87b2cec step 5).
pub struct Routes {
    wait: [PerShard; 5],
    took: [PerShard; 5],
}

impl Routes {
    fn new() -> Self {
        Self { wait: std::array::from_fn(|_| PerShard::new(STALL_BUCKETS)), took: std::array::from_fn(|_| PerShard::new(STALL_BUCKETS)) }
    }

    /// Work of `class` that waited `wait` (where it's known) and took `took`.
    pub fn observe(&self, class: Class, wait: Option<Duration>, took: Duration) {
        let i = class.index();
        if let (Some(w), Some(h)) = (wait, self.wait.get(i)) {
            h.observe(w.as_secs_f64());
        }
        if let Some(h) = self.took.get(i) {
            h.observe(took.as_secs_f64());
        }
    }

    /// [`Routes::observe`]'s wait alone, for work whose time taken is told once it and others are done
    /// ([`Routes::observe_took`]): a live frame fed to a viewer's socket with the rest of its batch, all
    /// written at once (goal 9, plan 01a1a87b2cec step 5).
    pub fn observe_wait(&self, class: Class, wait: Duration) {
        if let Some(h) = self.wait.get(class.index()) {
            h.observe(wait.as_secs_f64());
        }
    }

    /// [`Routes::observe`]'s time taken alone, for `n` pieces of `class`'s work done at once, each taking
    /// `took`: a batch of frames written with one write.
    pub fn observe_took(&self, class: Class, took: Duration, n: u64) {
        if let Some(h) = self.took.get(class.index()) {
            h.observe_n(took.as_secs_f64(), n);
        }
    }

    /// `class`'s histograms: how long its work waited, and took.
    pub fn of(&self, class: Class) -> Option<(&PerShard, &PerShard)> {
        Some((self.wait.get(class.index())?, self.took.get(class.index())?))
    }
}

/// The server's route histograms, made by the first of its work they time (a request, a runner's message).
/// A process that serves none, a runner's, never makes them, so its registry renders none of them: they
/// were 220 series of zeros a runner, every 15 s into observe, each kept there as a series of its own
/// (goal 11, plan 01a1a87b2e56 step 5).
static ROUTES: OnceLock<Routes> = OnceLock::new();

/// The server's.
pub fn routes() -> &'static Routes {
    ROUTES.get_or_init(Routes::new)
}

/// The legs of a runner's message's wait, apart (goal 10, plan 01a1a87b2dae step 4: the runner class's wait
/// was 2.8 ms at p99 with each session's updates taken by the shard that owns its copy, and which leg it
/// spends that in decides what's cut). On `/metrics` as `kiln_runner_wait_leg_seconds`, beside
/// `kiln_route_wait_seconds{class="runner"}`, which is their sum for an agent session's update.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunnerLeg {
    /// From the network thread handing a frame over to the hub's loop taking it: every frame's.
    ToLoop,
    /// From the hub's loop taking an agent session's update to handing it to the inbox of the shard that
    /// owns the session's copy: its frame read, its channel item's place checked, what it carries read.
    InLoop,
    /// From that hand-over to the owner taking the update: the wait for room in its inbox, then for the
    /// owner, on its own shard.
    ToOwner,
    /// Not a message's wait but what makes the next one's: the hub's loop's time with each event it takes,
    /// from taking it to being done with it, whatever it waited on meanwhile (the compute pool, an owner's
    /// inbox, the net thread's queue, another task's answer), every event's. Goal 10, plan 01a1a87b2dae step 5:
    /// `to_loop` was 6.1 ms at p99 where a frame's own legs at the loop came to 1.3 ms at p99.9, and only this
    /// tells the loop held by its events from the loop waiting for its shard.
    Held,
    /// Not the server's wait but the runner's, as the runner says it (`crate::wire::stamp`): from the runner
    /// taking an agent session's update in to making its frame, a duration on the runner's own clock. Goal 10,
    /// plan 01a1a87b2dae step 5: in g9p7's fifth round frames lagged with every leg at the server short, and
    /// nothing said whether the runner or the wire held them. Only an update whose runner said it, at its owner's
    /// take.
    Runner,
    /// The server's time with the same updates, on the server's clock: from its frame's arrival off the wire to
    /// its owner done with it (its copy changed, its viewers' frames published). Set against `runner` and a
    /// frame's whole time, what's left is the wire's: the runner's way to its link, the network, the server's
    /// net thread before the arrival.
    Server,
    /// From a frame's loop taking it off its queue to its runner's earlier frames on the hub's other loops having
    /// been taken, which it may not overtake (goal 10, plan 01a1a87b2dae step 4: a session's updates go to the loop on
    /// their owner's shard and the rest to the control loop, `crate::wire::fan`): a barrier's wait (its runner's Caps,
    /// inventory, a hand-over) for the runner's session updates before it, an update's for the barrier before it.
    /// Observed only for a frame that had any to wait for, so its count against `to_loop`'s is how often one does;
    /// `to_loop` leaves it out.
    Order,
    /// `to_loop` of the frames the hub's control loop took (goal 10, plan 01a1a87b2dae step 4): all but agent sessions'
    /// updates, which their owners' shards' session loops take. Set against `to_loop`, every frame's, it tells the
    /// session loops' wait from the control loop's.
    ToControl,
    /// From the network thread taking a frame off its link to handing it to its loop's queue (`crate::wire::fan`): the
    /// net thread's own hold of it, the rest of its batch read and its turn's sends made first. `to_loop` includes it;
    /// what's past it there is the queue's, its loop's wake and its shard's (goal 10, plan 01a1a87b2dae step 4: rt9's
    /// and g9p16's `to_loop` was 8 to 10 ms at p99 where each event's hold at the loop was 0.08 to 0.2 ms, and the
    /// shards ran their ready work within 0.45 ms at p99.9: only this tells where the frames waited).
    Net,
}

impl RunnerLeg {
    pub const ALL: [RunnerLeg; 9] = [RunnerLeg::ToLoop, RunnerLeg::InLoop, RunnerLeg::ToOwner, RunnerLeg::Held, RunnerLeg::Runner, RunnerLeg::Server, RunnerLeg::Order, RunnerLeg::ToControl, RunnerLeg::Net];

    pub fn name(self) -> &'static str {
        match self {
            RunnerLeg::ToLoop => "to_loop",
            RunnerLeg::InLoop => "in_loop",
            RunnerLeg::ToOwner => "to_owner",
            RunnerLeg::Held => "held",
            RunnerLeg::Runner => "runner",
            RunnerLeg::Server => "server",
            RunnerLeg::Order => "order",
            RunnerLeg::ToControl => "to_control",
            RunnerLeg::Net => "net",
        }
    }

    fn index(self) -> usize {
        match self {
            RunnerLeg::ToLoop => 0,
            RunnerLeg::InLoop => 1,
            RunnerLeg::ToOwner => 2,
            RunnerLeg::Held => 3,
            RunnerLeg::Runner => 4,
            RunnerLeg::Server => 5,
            RunnerLeg::Order => 6,
            RunnerLeg::ToControl => 7,
            RunnerLeg::Net => 8,
        }
    }
}

/// Each leg's histogram, in [`RunnerLeg::ALL`]'s order, to the microsecond below a few milliseconds as the
/// shards' delay is (`crate::shard_health::DELAY_BUCKETS`): a leg's p99 sits about a millisecond.
pub struct Legs([PerShard; RunnerLeg::ALL.len()]);

impl Legs {
    fn new() -> Self {
        Self(std::array::from_fn(|_| PerShard::new(crate::shard_health::DELAY_BUCKETS)))
    }

    /// A runner's message spent `took` on `leg`.
    pub fn observe(&self, leg: RunnerLeg, took: Duration) {
        if let Some(h) = self.0.get(leg.index()) {
            h.observe(took.as_secs_f64());
        }
    }

    pub fn of(&self, leg: RunnerLeg) -> Option<&PerShard> {
        self.0.get(leg.index())
    }
}

/// The server's runner-wait legs, made by the first runner's message its hub times: as [`ROUTES`], none in a
/// runner's process (282 series of zeros it rendered; goal 11, plan 01a1a87b2e56 step 5).
static LEGS: OnceLock<Legs> = OnceLock::new();

/// The server's.
pub fn legs() -> &'static Legs {
    LEGS.get_or_init(Legs::new)
}

/// A runner's message spent `took` on `leg`, on the server's histograms.
pub fn observe_leg(leg: RunnerLeg, took: Duration) {
    legs().observe(leg, took);
}

/// The legs of an agent session's live frame at the server past its runner's wait ([`RunnerLeg`]), from its update's
/// take by the session's owner to its write on a viewer's socket, each to the microsecond (goal 10, plan 01a1a87b2dae
/// step 5: g9p16's calm frames took 0.59 ms at the median from the agent's write to the lab's read, where the bus's
/// delivery and the socket's write, the frame's legs past its owner, were timed only to 50 µs (the `websocket` class's
/// [`STALL_BUCKETS`]), the owner's own time with the update wasn't told from its wait, and nothing said whether a
/// viewer was woken on its owner's thread or across threads). On `/metrics` as `kiln_frame_leg_seconds`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameLeg {
    /// From a session's owner taking a runner's update to being done with it: its copy changed and its viewers' frames
    /// published, but for a frame long to write, which the compute pool writes after (`crate::harness::copies`).
    Apply,
    /// From a frame's publish to its viewer's task taking it up, the viewer on its session's owner's shard: no other
    /// thread between them, only the rest of the owner's turn and the shard's other ready work.
    Woken,
    /// The same, the viewer on another shard than its session's owner: a wake across threads, its shard kicked where it
    /// was parked. A wake onto a CPU that's busy waits up to a tick on a kernel that preempts lazily, as fedora's does
    /// at 1000 Hz (docs/runtime-lab.md, "Why: fedora's kernel, read").
    WokenOther,
    /// From a viewer's read of its batch off the bus to the batch written to its socket, each of its frames'.
    Write,
}

impl FrameLeg {
    pub const ALL: [FrameLeg; 4] = [FrameLeg::Apply, FrameLeg::Woken, FrameLeg::WokenOther, FrameLeg::Write];

    pub fn name(self) -> &'static str {
        match self {
            FrameLeg::Apply => "apply",
            FrameLeg::Woken => "woken",
            FrameLeg::WokenOther => "woken_other",
            FrameLeg::Write => "write",
        }
    }

    fn index(self) -> usize {
        match self {
            FrameLeg::Apply => 0,
            FrameLeg::Woken => 1,
            FrameLeg::WokenOther => 2,
            FrameLeg::Write => 3,
        }
    }
}

/// Each frame leg's histogram, in [`FrameLeg::ALL`]'s order, to the microsecond below a few milliseconds as the
/// runner's legs are ([`Legs`]).
pub struct FrameLegs([PerShard; FrameLeg::ALL.len()]);

impl FrameLegs {
    fn new() -> Self {
        Self(std::array::from_fn(|_| PerShard::new(crate::shard_health::DELAY_BUCKETS)))
    }

    /// `n` frames each spent `took` on `leg`.
    pub fn observe_n(&self, leg: FrameLeg, took: Duration, n: u64) {
        if let Some(h) = self.0.get(leg.index()) {
            h.observe_n(took.as_secs_f64(), n);
        }
    }

    pub fn of(&self, leg: FrameLeg) -> Option<&PerShard> {
        self.0.get(leg.index())
    }
}

/// The server's frame legs, made by the first frame it times: as [`LEGS`], none in a runner's process.
static FRAME_LEGS: OnceLock<FrameLegs> = OnceLock::new();

/// The server's.
pub fn frame_legs() -> &'static FrameLegs {
    FRAME_LEGS.get_or_init(FrameLegs::new)
}

/// A frame spent `took` on `leg`, on the server's histograms.
pub fn observe_frame(leg: FrameLeg, took: Duration) {
    frame_legs().observe_n(leg, took, 1);
}

/// `n` frames each spent `took` on `leg` (a batch written at once), on the server's histograms.
pub fn observe_frames(leg: FrameLeg, took: Duration, n: u64) {
    if n > 0 {
        frame_legs().observe_n(leg, took, n);
    }
}

/// Work of `class` that waited `wait` and took `took`, on the server's histograms.
pub fn observe(class: Class, wait: Option<Duration>, took: Duration) {
    routes().observe(class, wait, took);
}

/// Work of `class` that waited `wait`, its time taken told once it and others are done ([`observe_took`]).
pub fn observe_wait(class: Class, wait: Duration) {
    routes().observe_wait(class, wait);
}

/// `n` pieces of `class`'s work done at once, each having taken `took`, on the server's histograms.
pub fn observe_took(class: Class, took: Duration, n: u64) {
    routes().observe_took(class, took, n);
}

/// A request's class, by its method and path; `None` for a WebSocket's opening (`upgrade`), whose
/// frames are timed instead.
pub fn class_of(method: &Method, path: &str, upgrade: bool) -> Option<Class> {
    if upgrade {
        return None;
    }
    let read = *method == Method::GET || *method == Method::HEAD;
    let api = if read { Class::ApiRead } else { Class::ApiWrite };
    // On the stack, as many as a route names (goal 9, plan 01a1a87b2cec step 5: each request made a vector of them).
    let mut room = [""; crate::http::server::ROUTE_DEPTH];
    let segs = crate::http::server::split_path(path, &mut room);
    if runners(&segs) {
        return Some(Class::Runner);
    }
    let kiln_s_own = match segs.first().copied() {
        Some("api" | "metrics" | "healthz" | "readyz" | "livez" | "v1" | "v2" | "dl" | "install.sh" | "install.ps1" | "ca.pem" | "ca.crt" | "ca.mobileconfig") => true,
        // Git, CI's GitHub-shaped routes and the OIDC issuer, at the root as their clients ask.
        _ => crate::ci::api::is_open(&segs),
    };
    Some(if kiln_s_own || !read { api } else { Class::Page })
}

/// What a request's polls are labeled with for the shards' table of slow polls (`hyper_rt::slow`; goal 9,
/// plan 01a1a87b2cec step 2): whether it reads or changes, and which of the API's resources it is
/// (`/api/<resource>`, and the harness's own under `/api/harness/<resource>`), from a fixed set of labels, so
/// a label costs a match and nothing is kept per request; a WebSocket's opening, a page, kiln's own routes at
/// the root, and what matches none of them by their kind alone.
pub fn label_of(method: &Method, path: &str, upgrade: bool) -> &'static hyper_rt::slow::Label {
    use hyper_rt::slow::Label;
    static OPEN: Label = Label { name: "websocket open" };
    static PAGE: Label = Label { name: "page" };
    static ROOT: Label = Label { name: "read /<kiln's own>" };
    // A scrape of the server's figures apart from kiln's other own reads: the lab's own scrape at its window's start is
    // counted in that window (goal 10, plan 01a1a87b2dae step 5: g9p7's 2.5 ms `read /<kiln's own>`).
    static METRICS: Label = Label { name: "read /metrics" };
    static OTHER_READ: Label = Label { name: "read /api/<other>" };
    static OTHER_WRITE: Label = Label { name: "write /api/<other>" };
    static ROOT_WRITE: Label = Label { name: "write /<other>" };
    /// Each name's read and write labels, as statics of a `match` arm's own.
    macro_rules! resources {
        ($prefix:literal, $of:expr, $read:expr; $($name:literal),* $(,)?) => {
            match $of {
                $(Some($name) => {
                    static READ: Label = Label { name: concat!("read /api/", $prefix, $name) };
                    static WRITE: Label = Label { name: concat!("write /api/", $prefix, $name) };
                    Some(if $read { &READ } else { &WRITE })
                })*
                _ => None,
            }
        };
    }
    if upgrade {
        return &OPEN;
    }
    let read = *method == Method::GET || *method == Method::HEAD;
    let mut segs = path.split('/').filter(|s| !s.is_empty());
    let (first, second, third) = (segs.next(), segs.next(), segs.next());
    if first != Some("api") {
        if read && first == Some("metrics") && second.is_none() {
            return &METRICS;
        }
        return match (read, class_of(method, path, false)) {
            (true, Some(Class::Page)) => &PAGE,
            (true, _) => &ROOT,
            (false, _) => &ROOT_WRITE,
        };
    }
    let label = if second == Some("harness") && third.is_some() {
        resources!("harness/", third, read; "agents", "broadcast", "changes", "command", "conversations", "files", "history", "kinds", "machines", "sessions", "workspaces")
    } else {
        resources!("", second, read;
            "accounts", "agents", "ai-sessions", "attachments", "audit", "auth", "blobs", "cd", "chaos", "ci", "cluster", "conversations", "custody", "deploy", "diffs",
            "discovery", "events", "experiments", "front-doors", "harness", "headroom", "hooks", "identities", "incidents", "join", "join-requests", "jobs", "machines", "mcp",
            "me", "metrics", "observe", "ops", "outposts", "overview", "peers", "places", "plans", "programs", "push", "reach", "repos", "research", "runners", "runs",
            "settings", "shared", "ship", "sync", "system", "tailscale", "terminal", "trail", "transfers", "updates", "verify", "voice")
    };
    label.unwrap_or(if read { &OTHER_READ } else { &OTHER_WRITE })
}

/// Routes runners and their jobs call: enrolling and their link's tunnel, a job's source and
/// artifacts, sync's contents and bundles, a learner's copy of the store, Observe's snapshots, a
/// fleet build's own routes, the binaries runners fetch, and CI's runtime services (the cache and
/// artifacts actions ask of `_apis` and `twirp`, kiln's `_kiln`).
fn runners(segs: &[&str]) -> bool {
    matches!(
        segs,
        ["api", "agents", "enroll" | "instance" | "rekey" | "tunnel"]
            | ["api", "jobs", _, "source"]
            | ["api", "jobs", _, "artifacts", ..]
            | ["api", "sync", ..]
            | ["api", "cluster", "copy"]
            | ["api", "observe", "v1", "snapshot", ..]
            | ["api", "updates", "jobs", _, "source" | "binary"]
            | ["dl", ..]
            | ["_apis" | "twirp" | "_kiln", ..]
    )
}

/// The `kiln_route_*` families, in the exposition: those the process made ([`ROUTES`], [`LEGS`], [`FRAME_LEGS`]).
pub fn render(o: &mut String) {
    render_of(o, ROUTES.get(), LEGS.get(), FRAME_LEGS.get());
}

/// The families of `routes`, `legs` and `frames`: each said, with its TYPE, whether or not it's made (as every family
/// of the registry is, before anything happened), and its series where it's made, so a runner's process renders none.
fn render_of(o: &mut String, routes: Option<&Routes>, legs: Option<&Legs>, frames: Option<&FrameLegs>) {
    let _ = writeln!(
        o,
        "# HELP kiln_route_wait_seconds How long the server's work waited before its handler started, by class (page, api_read, api_write, websocket, runner).\n# TYPE kiln_route_wait_seconds histogram"
    );
    for c in Class::ALL {
        if let Some((wait, _)) = routes.and_then(|r| r.of(c)) {
            wait.render(o, "kiln_route_wait_seconds", &format!("class=\"{}\"", c.name()));
        }
    }
    let _ = writeln!(o, "# HELP kiln_route_seconds How long the server's work took once its handler started, by class.\n# TYPE kiln_route_seconds histogram");
    for c in Class::ALL {
        if let Some((_, took)) = routes.and_then(|r| r.of(c)) {
            took.render(o, "kiln_route_seconds", &format!("class=\"{}\"", c.name()));
        }
    }
    let _ = writeln!(
        o,
        "# HELP kiln_runner_wait_leg_seconds A runner's message's wait, by leg: to_loop (to the hub's loop taking its frame, every frame's), in_loop (that loop handing an agent session's update to its owner's inbox), to_owner (that owner taking it); held, the loop's own time with each event it takes, its waits included (what the frames behind it wait for), every event's; and of each agent session's update whose runner said how long it held it, runner (from the runner taking it in to making its frame, on the runner's clock) and server (from its frame's arrival to its owner done with it, on the server's).\n# TYPE kiln_runner_wait_leg_seconds histogram"
    );
    for leg in RunnerLeg::ALL {
        if let Some(h) = legs.and_then(|l| l.of(leg)) {
            h.render(o, "kiln_runner_wait_leg_seconds", &format!("leg=\"{}\"", leg.name()));
        }
    }
    let _ = writeln!(
        o,
        "# HELP kiln_frame_leg_seconds An agent session's live frame at the server past its runner's wait, by leg: apply (its session's owner taking the runner's update to being done with it, its frames published), woken (a frame's publish to its viewer's task taking it up, the viewer on its session's owner's shard), woken_other (the same, the viewer on another shard: a wake across threads), write (a viewer's read of its batch to the batch written to its socket).\n# TYPE kiln_frame_leg_seconds histogram"
    );
    for leg in FrameLeg::ALL {
        if let Some(h) = frames.and_then(|f| f.of(leg)) {
            h.render(o, "kiln_frame_leg_seconds", &format!("leg=\"{}\"", leg.name()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Goal 1, plan 01a1a59c2a68 step 2: each request is its own class's.
    #[test]
    fn g1_s2_each_request_is_its_own_class() {
        let get = Method::GET;
        let post = Method::POST;
        let class = |m: &Method, p: &str| class_of(m, p, false);
        // Pages: the UI's paths and files, and pages agents show.
        assert_eq!(class(&get, "/"), Some(Class::Page));
        assert_eq!(class(&get, "/agents/01a19ff6"), Some(Class::Page));
        assert_eq!(class(&Method::HEAD, "/assets/index-3f2a.js"), Some(Class::Page));
        assert_eq!(class(&get, "/v/abc/index.html"), Some(Class::Page));
        // The API's reads and changes; kiln's own reads at the root.
        assert_eq!(class(&get, "/api/harness/sessions"), Some(Class::ApiRead));
        assert_eq!(class(&get, "/metrics"), Some(Class::ApiRead));
        assert_eq!(class(&get, "/healthz"), Some(Class::ApiRead));
        assert_eq!(class(&post, "/api/harness/sessions/s1/command"), Some(Class::ApiWrite));
        assert_eq!(class(&post, "/api/mcp"), Some(Class::ApiWrite));
        assert_eq!(class(&Method::DELETE, "/api/harness/sessions/s1"), Some(Class::ApiWrite));
        // A change outside the API is a change, never a page.
        assert_eq!(class(&post, "/agents"), Some(Class::ApiWrite));
        // Git at the root reads as the API.
        assert_eq!(class(&get, "/ada/kiln.git/info/refs"), Some(Class::ApiRead));
        // Runners and their jobs.
        assert_eq!(class(&post, "/api/agents/enroll"), Some(Class::Runner));
        assert_eq!(class(&get, "/api/sync/blobs/ab12"), Some(Class::Runner));
        assert_eq!(class(&Method::PUT, "/api/jobs/j1/artifacts/upload"), Some(Class::Runner));
        assert_eq!(class(&post, "/twirp/github.actions.results.api.v1.ArtifactService/CreateArtifact"), Some(Class::Runner));
        assert_eq!(class(&get, "/_apis/artifactcache/cache"), Some(Class::Runner));
        assert_eq!(class(&get, "/dl/kiln/linux-x86_64"), Some(Class::Runner));
        // A WebSocket's opening is none: its frames are timed.
        assert_eq!(class_of(&get, "/api/harness/sessions/s1/ws", true), None);
    }

    /// Goal 9, plan 01a1a87b2cec step 2: a request's polls are labeled by whether it reads or changes and
    /// which of the API's resources it is; a page, kiln's own reads at the root, a WebSocket's opening and the
    /// API's other paths by their kind; the same route is the same label each time.
    #[test]
    fn g9_s2_a_request_is_labeled_by_its_route() {
        let (get, post) = (Method::GET, Method::POST);
        let name = |m: &Method, p: &str| label_of(m, p, false).name;
        assert_eq!(name(&get, "/api/harness/sessions/s1/events"), "read /api/harness/sessions");
        assert_eq!(name(&post, "/api/harness/sessions/s1/command"), "write /api/harness/sessions");
        assert_eq!(name(&get, "/api/harness"), "read /api/harness");
        assert_eq!(name(&get, "/api/runners"), "read /api/runners");
        assert_eq!(name(&post, "/api/mcp"), "write /api/mcp");
        assert_eq!(name(&get, "/api/nothing-here/x"), "read /api/<other>");
        assert_eq!(name(&Method::DELETE, "/api/nothing-here"), "write /api/<other>");
        assert_eq!(name(&get, "/agents/01a19ff6"), "page");
        assert_eq!(name(&get, "/metrics"), "read /metrics", "goal 10 step 5: a scrape apart from kiln's other own reads");
        assert_eq!(name(&get, "/healthz"), "read /<kiln's own>");
        assert_eq!(name(&post, "/agents"), "write /<other>");
        assert_eq!(label_of(&get, "/api/harness/sessions/s1/ws", true).name, "websocket open");
        assert!(std::ptr::eq(label_of(&get, "/api/runs", false), label_of(&Method::HEAD, "/api/runs/7", false)));
    }

    /// Goal 1, plan 01a1a59c2a68 step 2: what's observed moves its own class's histograms alone.
    #[test]
    fn g1_s2_an_observation_moves_its_own_class_alone() {
        let r = Routes::new();
        r.observe(Class::ApiWrite, Some(Duration::from_micros(80)), Duration::from_millis(3));
        r.observe(Class::Websocket, None, Duration::from_millis(1));
        for c in Class::ALL {
            let (wait, took) = r.of(c).map(|(w, t)| (w.count(), t.count())).unwrap_or_default();
            match c {
                Class::ApiWrite => assert_eq!((wait, took), (1, 1)),
                // A wait not known isn't one.
                Class::Websocket => assert_eq!((wait, took), (0, 1)),
                _ => assert_eq!((wait, took), (0, 0), "{}", c.name()),
            }
        }
        let mut o = String::new();
        routes().observe(Class::Page, Some(Duration::ZERO), Duration::from_micros(400));
        render(&mut o);
        for c in Class::ALL {
            assert!(o.contains(&format!("kiln_route_seconds_count{{class=\"{}\"}}", c.name())), "{} rendered", c.name());
            assert!(o.contains(&format!("kiln_route_wait_seconds_bucket{{class=\"{}\",le=\"0.00005\"}}", c.name())), "{} rendered", c.name());
        }
    }

    /// Goal 10, plan 01a1a87b2dae step 4: a runner's message's wait moves its own leg's histogram alone, to the
    /// microsecond, and each leg is rendered, its family beside the runner class's.
    #[test]
    fn g10_s4_a_runners_wait_is_told_apart_by_its_legs() {
        let l = Legs::new();
        l.observe(RunnerLeg::InLoop, Duration::from_micros(40));
        l.observe(RunnerLeg::ToOwner, Duration::from_micros(900));
        l.observe(RunnerLeg::ToOwner, Duration::from_micros(1100));
        // A frame's wait for its runner's earlier ones on the hub's other loops, and the control loop's own wait: each its
        // own leg, `to_loop`'s apart.
        l.observe(RunnerLeg::Order, Duration::from_micros(300));
        l.observe(RunnerLeg::ToControl, Duration::from_micros(70));
        l.observe(RunnerLeg::Net, Duration::from_micros(20));
        let counts: Vec<u64> = RunnerLeg::ALL.iter().map(|leg| l.of(*leg).map_or(0, PerShard::count)).collect();
        assert_eq!(counts, vec![0, 1, 2, 0, 0, 0, 1, 1, 1]);
        // Finer than a millisecond: 0.9 ms and 1.1 ms land in buckets apart.
        let at = |bound: f64| l.of(RunnerLeg::ToOwner).and_then(|h| h.cumulative().into_iter().find(|(b, _)| (*b - bound).abs() < 1e-12).map(|(_, n)| n));
        assert_eq!((at(0.001), at(0.00125)), (Some(1), Some(2)));
        let mut o = String::new();
        observe_leg(RunnerLeg::ToLoop, Duration::from_micros(10));
        render(&mut o);
        for leg in RunnerLeg::ALL {
            assert!(o.contains(&format!("kiln_runner_wait_leg_seconds_count{{leg=\"{}\"}}", leg.name())), "{} rendered", leg.name());
        }
        assert!(o.contains("kiln_runner_wait_leg_seconds_bucket{leg=\"to_loop\",le=\"0.00001\"}"), "the shards' delay buckets");
    }

    /// Goal 10, plan 01a1a87b2dae step 5: the hub's loop's hold of each event is a leg of its own, named `held`, the
    /// last of them (the legs before it keep their places), on its own histogram to the microsecond, and rendered
    /// with the others.
    #[test]
    fn g10_s5_the_loops_hold_of_each_event_is_a_leg_of_its_own() {
        assert_eq!(RunnerLeg::ALL.iter().map(|l| l.name()).collect::<Vec<_>>(), vec!["to_loop", "in_loop", "to_owner", "held", "runner", "server", "order", "to_control", "net"]);
        assert_eq!(RunnerLeg::ALL.iter().map(|l| l.index()).collect::<Vec<_>>(), vec![0, 1, 2, 3, 4, 5, 6, 7, 8]);
        let l = Legs::new();
        l.observe(RunnerLeg::Held, Duration::from_micros(150));
        l.observe(RunnerLeg::Held, Duration::from_micros(6_000));
        let counts: Vec<u64> = RunnerLeg::ALL.iter().map(|leg| l.of(*leg).map_or(0, PerShard::count)).collect();
        assert_eq!(counts, vec![0, 0, 0, 2, 0, 0, 0, 0, 0]);
        let under = |bound: f64| l.of(RunnerLeg::Held).and_then(|h| h.cumulative().into_iter().find(|(b, _)| (*b - bound).abs() < 1e-12).map(|(_, n)| n));
        assert_eq!((under(0.00025), under(0.001)), (Some(1), Some(1)), "150 µs under a quarter millisecond, 6 ms past a millisecond");
        let mut o = String::new();
        observe_leg(RunnerLeg::Held, Duration::from_micros(150));
        render(&mut o);
        assert!(o.contains("kiln_runner_wait_leg_seconds_count{leg=\"held\"}"), "held rendered");
    }

    /// Goal 10, plan 01a1a87b2dae step 5: the runner's hold of an update, as its runner says it, and the server's
    /// time with it are legs of their own, `runner` and `server`, appended after the legs before them (which keep
    /// their places), each on its own histogram to the microsecond, and rendered with the others.
    #[test]
    fn g10_s5_the_runners_hold_and_the_servers_time_are_legs_of_their_own() {
        let names: Vec<&str> = RunnerLeg::ALL.iter().map(|l| l.name()).collect();
        assert_eq!(names.get(RunnerLeg::Held.index() + 1..RunnerLeg::Held.index() + 3), Some(&["runner", "server"][..]), "appended after held");
        assert_eq!((RunnerLeg::Runner.index(), RunnerLeg::Server.index()), (RunnerLeg::Held.index() + 1, RunnerLeg::Held.index() + 2));
        let l = Legs::new();
        l.observe(RunnerLeg::Runner, Duration::from_micros(80));
        l.observe(RunnerLeg::Runner, Duration::from_micros(74_000));
        l.observe(RunnerLeg::Server, Duration::from_micros(900));
        let count = |leg| l.of(leg).map_or(0, PerShard::count);
        assert_eq!((count(RunnerLeg::Runner), count(RunnerLeg::Server), count(RunnerLeg::Held)), (2, 1, 0), "each its own");
        let under = |leg, bound: f64| l.of(leg).and_then(|h| h.cumulative().into_iter().find(|(b, _)| (*b - bound).abs() < 1e-12).map(|(_, n)| n));
        assert_eq!((under(RunnerLeg::Runner, 0.0001), under(RunnerLeg::Runner, 0.05)), (Some(1), Some(1)), "80 µs under a tenth of a millisecond, 74 ms past 50");
        assert_eq!(under(RunnerLeg::Server, 0.001), Some(1), "900 µs under a millisecond");
        let mut o = String::new();
        observe_leg(RunnerLeg::Runner, Duration::from_micros(80));
        observe_leg(RunnerLeg::Server, Duration::from_micros(900));
        render(&mut o);
        for leg in ["runner", "server"] {
            assert!(o.contains(&format!("kiln_runner_wait_leg_seconds_count{{leg=\"{leg}\"}}")), "{leg} rendered");
        }
    }

    /// Goal 9, plan 01a1a87b2cec step 5: a request's class is told from its path's segments split on the stack, with
    /// no allocation (each request made a vector of them), and is what it was: the API's reads and writes, runners'
    /// routes, pages, kiln's own at the root, and a path deeper than any route.
    #[test]
    fn g9_s5_a_requests_class_is_told_without_allocating() {
        let before = crate::os::counted::made().blocks;
        let told = [
            class_of(&Method::GET, "/api/harness/sessions/a/events", false),
            class_of(&Method::POST, "/api/harness/sessions/a/command", false),
            class_of(&Method::POST, "/api/agents/enroll", false),
            class_of(&Method::GET, "/agents/a1", false),
            class_of(&Method::GET, "/healthz", false),
            class_of(&Method::GET, "/api/harness/sessions/a/ws", true),
        ];
        let made = crate::os::counted::made().blocks.saturating_sub(before);
        assert_eq!(made, 0, "{made} allocations");
        assert_eq!(told, [Some(Class::ApiRead), Some(Class::ApiWrite), Some(Class::Runner), Some(Class::Page), Some(Class::ApiRead), None]);
        assert_eq!(class_of(&Method::GET, "/api/harness/sessions/a/files/src/kiln/lib/http/server.rs", false), Some(Class::ApiRead), "deeper than any route");
    }

    /// Goal 11, plan 01a1a87b2e56 step 5 (g11-heap): a process that times none of the server's work (a runner's)
    /// renders none of its route and leg histograms' series, where it rendered them all as zeros: 502 series a
    /// runner, each a series of its own in observe. Each family is still said with its TYPE; the server renders
    /// every series, every class and leg, once they're made.
    #[test]
    fn g11_s5_heap_a_process_that_times_none_of_the_servers_work_renders_none_of_it() {
        let mut none = String::new();
        render_of(&mut none, None, None, None);
        assert!(none.lines().all(|l| l.starts_with('#')), "series of nothing made: {none}");
        for family in ["kiln_route_wait_seconds", "kiln_route_seconds", "kiln_runner_wait_leg_seconds", "kiln_frame_leg_seconds"] {
            assert!(none.contains(&format!("# TYPE {family} histogram")), "{family} said with its TYPE");
        }
        let mut made = String::new();
        render_of(&mut made, Some(&Routes::new()), Some(&Legs::new()), Some(&FrameLegs::new()));
        // A histogram's series: a bucket each bound, `+Inf`, `_sum` and `_count`.
        let routes = Class::ALL.len() * 2 * (STALL_BUCKETS.len() + 3);
        let legs = RunnerLeg::ALL.len() * (crate::shard_health::DELAY_BUCKETS.len() + 3);
        let frames = FrameLeg::ALL.len() * (crate::shard_health::DELAY_BUCKETS.len() + 3);
        let series = made.lines().filter(|l| !l.starts_with('#')).count();
        // Goal 10, plan 01a1a87b2dae step 4: the legs `order`, `to_control` and `net` added 141 (3 legs of 47) to the 282
        // there were; step 5: the frame's four legs, 188 more.
        assert_eq!((series, routes, legs, frames), (routes + legs + frames, 220, 423, 188), "every class's and leg's, zeros and all");
        for c in Class::ALL {
            assert!(made.contains(&format!("kiln_route_wait_seconds_count{{class=\"{}\"}} 0", c.name())), "{}", c.name());
        }
    }

    /// Goal 10, plan 01a1a87b2dae step 5: a live frame's legs at the server past its runner's wait are each its own, to
    /// the microsecond: the owner's apply, the viewer's wake on its owner's shard and across shards, and the socket's
    /// write (a batch's frames each observed); each rendered, its family beside the runner's legs, in a set order the
    /// lab reads by name.
    #[test]
    fn g10_s5_a_frames_legs_past_its_owner_are_each_its_own() {
        assert_eq!(FrameLeg::ALL.iter().map(|l| l.name()).collect::<Vec<_>>(), vec!["apply", "woken", "woken_other", "write"]);
        assert_eq!(FrameLeg::ALL.iter().map(|l| l.index()).collect::<Vec<_>>(), vec![0, 1, 2, 3]);
        let f = FrameLegs::new();
        f.observe_n(FrameLeg::Apply, Duration::from_micros(40), 1);
        f.observe_n(FrameLeg::Woken, Duration::from_micros(8), 1);
        f.observe_n(FrameLeg::WokenOther, Duration::from_micros(180), 1);
        f.observe_n(FrameLeg::WokenOther, Duration::from_micros(1_100), 1);
        // A batch of three frames written at once: each frame's.
        f.observe_n(FrameLeg::Write, Duration::from_micros(30), 3);
        let counts: Vec<u64> = FrameLeg::ALL.iter().map(|leg| f.of(*leg).map_or(0, PerShard::count)).collect();
        assert_eq!(counts, vec![1, 1, 2, 3]);
        // Finer than the route classes' first bucket (50 µs): 8 µs and 40 µs apart from 180 µs, and a wake past a
        // millisecond (a tick behind a busy CPU) from one under it.
        let under = |leg: FrameLeg, bound: f64| f.of(leg).and_then(|h| h.cumulative().into_iter().find(|(b, _)| (*b - bound).abs() < 1e-12).map(|(_, n)| n));
        assert_eq!((under(FrameLeg::Woken, 0.00001), under(FrameLeg::Apply, 0.00005)), (Some(1), Some(1)));
        assert_eq!((under(FrameLeg::WokenOther, 0.0002), under(FrameLeg::WokenOther, 0.001)), (Some(1), Some(1)), "180 µs under 0.2 ms, 1.1 ms past a millisecond");
        let mut o = String::new();
        observe_frame(FrameLeg::Woken, Duration::from_micros(8));
        observe_frames(FrameLeg::Write, Duration::from_micros(30), 2);
        observe_frames(FrameLeg::Write, Duration::from_micros(30), 0);
        render(&mut o);
        for leg in FrameLeg::ALL {
            assert!(o.contains(&format!("kiln_frame_leg_seconds_count{{leg=\"{}\"}}", leg.name())), "{} rendered", leg.name());
        }
        assert!(o.contains("kiln_frame_leg_seconds_bucket{leg=\"woken\",le=\"0.00001\"}"), "the shards' delay buckets");
        assert!(o.find("# TYPE kiln_runner_wait_leg_seconds").is_some_and(|runner| o.find("# TYPE kiln_frame_leg_seconds").is_some_and(|frame| runner < frame)), "after the runner's legs");
    }
}
