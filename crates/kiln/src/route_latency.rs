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
//! Every histogram is atomics only ([`Histogram`]): no lock on any path that times or reads one.
//! Time a request waited in its socket while its shard was busy shows in `kiln_shard_delay_seconds`
//! (`crate::shard_health`): the runtime doesn't say when a task was woken.

use std::fmt::Write as _;
use std::sync::OnceLock;
use std::time::Duration;

use http::Method;

use crate::metrics::{Histogram, STALL_BUCKETS};

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

/// Each class's histograms, in [`Class::ALL`]'s order.
pub struct Routes {
    wait: [Histogram; 5],
    took: [Histogram; 5],
}

impl Routes {
    fn new() -> Self {
        Self { wait: std::array::from_fn(|_| Histogram::new(STALL_BUCKETS)), took: std::array::from_fn(|_| Histogram::new(STALL_BUCKETS)) }
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
            for _ in 0..n {
                h.observe(took.as_secs_f64());
            }
        }
    }

    /// `class`'s histograms: how long its work waited, and took.
    pub fn of(&self, class: Class) -> Option<(&Histogram, &Histogram)> {
        Some((self.wait.get(class.index())?, self.took.get(class.index())?))
    }
}

/// The server's.
pub fn routes() -> &'static Routes {
    static R: OnceLock<Routes> = OnceLock::new();
    R.get_or_init(Routes::new)
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
}

impl RunnerLeg {
    pub const ALL: [RunnerLeg; 4] = [RunnerLeg::ToLoop, RunnerLeg::InLoop, RunnerLeg::ToOwner, RunnerLeg::Held];

    pub fn name(self) -> &'static str {
        match self {
            RunnerLeg::ToLoop => "to_loop",
            RunnerLeg::InLoop => "in_loop",
            RunnerLeg::ToOwner => "to_owner",
            RunnerLeg::Held => "held",
        }
    }

    fn index(self) -> usize {
        match self {
            RunnerLeg::ToLoop => 0,
            RunnerLeg::InLoop => 1,
            RunnerLeg::ToOwner => 2,
            RunnerLeg::Held => 3,
        }
    }
}

/// Each leg's histogram, in [`RunnerLeg::ALL`]'s order, to the microsecond below a few milliseconds as the
/// shards' delay is (`crate::shard_health::DELAY_BUCKETS`): a leg's p99 sits about a millisecond.
pub struct Legs([Histogram; RunnerLeg::ALL.len()]);

impl Legs {
    fn new() -> Self {
        Self(std::array::from_fn(|_| Histogram::new(crate::shard_health::DELAY_BUCKETS)))
    }

    /// A runner's message spent `took` on `leg`.
    pub fn observe(&self, leg: RunnerLeg, took: Duration) {
        if let Some(h) = self.0.get(leg.index()) {
            h.observe(took.as_secs_f64());
        }
    }

    pub fn of(&self, leg: RunnerLeg) -> Option<&Histogram> {
        self.0.get(leg.index())
    }
}

/// The server's.
pub fn legs() -> &'static Legs {
    static L: OnceLock<Legs> = OnceLock::new();
    L.get_or_init(Legs::new)
}

/// A runner's message spent `took` on `leg`, on the server's histograms.
pub fn observe_leg(leg: RunnerLeg, took: Duration) {
    legs().observe(leg, took);
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
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
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

/// The `kiln_route_*` families, in the exposition.
pub fn render(o: &mut String) {
    let r = routes();
    let _ = writeln!(
        o,
        "# HELP kiln_route_wait_seconds How long the server's work waited before its handler started, by class (page, api_read, api_write, websocket, runner).\n# TYPE kiln_route_wait_seconds histogram"
    );
    for c in Class::ALL {
        if let Some((wait, _)) = r.of(c) {
            wait.render(o, "kiln_route_wait_seconds", &format!("class=\"{}\"", c.name()));
        }
    }
    let _ = writeln!(o, "# HELP kiln_route_seconds How long the server's work took once its handler started, by class.\n# TYPE kiln_route_seconds histogram");
    for c in Class::ALL {
        if let Some((_, took)) = r.of(c) {
            took.render(o, "kiln_route_seconds", &format!("class=\"{}\"", c.name()));
        }
    }
    let _ = writeln!(
        o,
        "# HELP kiln_runner_wait_leg_seconds A runner's message's wait, by leg: to_loop (to the hub's loop taking its frame, every frame's), in_loop (that loop handing an agent session's update to its owner's inbox), to_owner (that owner taking it); and held, the loop's own time with each event it takes, its waits included (what the frames behind it wait for), every event's.\n# TYPE kiln_runner_wait_leg_seconds histogram"
    );
    let l = legs();
    for leg in RunnerLeg::ALL {
        if let Some(h) = l.of(leg) {
            h.render(o, "kiln_runner_wait_leg_seconds", &format!("leg=\"{}\"", leg.name()));
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
        assert_eq!(name(&get, "/metrics"), "read /<kiln's own>");
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
        let counts: Vec<u64> = RunnerLeg::ALL.iter().map(|leg| l.of(*leg).map_or(0, Histogram::count)).collect();
        assert_eq!(counts, vec![0, 1, 2, 0]);
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
        assert_eq!(RunnerLeg::ALL.iter().map(|l| l.name()).collect::<Vec<_>>(), vec!["to_loop", "in_loop", "to_owner", "held"]);
        assert_eq!(RunnerLeg::ALL.iter().map(|l| l.index()).collect::<Vec<_>>(), vec![0, 1, 2, 3]);
        let l = Legs::new();
        l.observe(RunnerLeg::Held, Duration::from_micros(150));
        l.observe(RunnerLeg::Held, Duration::from_micros(6_000));
        let counts: Vec<u64> = RunnerLeg::ALL.iter().map(|leg| l.of(*leg).map_or(0, Histogram::count)).collect();
        assert_eq!(counts, vec![0, 0, 0, 2]);
        let under = |bound: f64| l.of(RunnerLeg::Held).and_then(|h| h.cumulative().into_iter().find(|(b, _)| (*b - bound).abs() < 1e-12).map(|(_, n)| n));
        assert_eq!((under(0.00025), under(0.001)), (Some(1), Some(1)), "150 µs under a quarter millisecond, 6 ms past a millisecond");
        let mut o = String::new();
        observe_leg(RunnerLeg::Held, Duration::from_micros(150));
        render(&mut o);
        assert!(o.contains("kiln_runner_wait_leg_seconds_count{leg=\"held\"}"), "held rendered");
    }
}
