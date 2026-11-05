//! The local side of reach (`docs/reach.md` §6): the socket `kiln run`, `kiln runners` and the
//! agents' MCP tools talk to on a runner's machine, and what it does for them: pick a runner,
//! copy the working tree over, run, stream the output back.
//!
//! The socket is the owner's alone (0600 in a 0700 folder), next to `kiln spawn`'s. Each
//! connection is one request, answered by a stream of [`Ev`]s; a run's connection also carries its
//! input and signals, and a client that goes away stops its run (unless it said `detach`).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::directory::{self, RunnerInfo, Want};
use super::node::Handle;
use super::sync;
use super::wire::{self, Ask, RunSpec, Say, Signal, Stream, SyncAsk, SyncSay};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Req {
    Status,
    /// The runners, ranked for `want` when given (`linux/amd64`, a name ...).
    Runners { want: Option<String> },
    Run(Box<RunReq>),
    Input(Vec<u8>),
    InputEnd,
    Signal(Signal),
    /// Forget a runner's pinned key (it was reinstalled).
    Forget { runner: String },
    /// A question for the kiln server (JSON), from an agent's MCP server: kiln's operations,
    /// asked for its session (`crate::mcp::runner_ask`). Answered with [`Ev::Answer`].
    Ask { json: String },
    /// Something for the machine's preview browser (JSON `{"op": "preview", ...}`, see
    /// `crate::browser::ask`), from an agent's MCP server. Answered with [`Ev::Answer`]. Appended
    /// last: postcard encodes variants by index.
    Browser { json: String },
    /// A run as [`Req::Run`], unless an identical one succeeded before: then its recorded result
    /// instead (`kiln run --reuse`, `super::reuse`, docs/ci.md W24-R24). Appended last.
    RunReuse(Box<RunReq>),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RunReq {
    /// Where: a runner's name and what the work needs, as one spelling (`linux/amd64`,
    /// `cpus=8,mem=32g`, `adacachy,mem=32g`: [`directory::target`]), or empty for the best.
    pub target: String,
    pub command: String,
    /// This machine's folder the command is about: its tree is copied over when `sync`, and the
    /// command runs in the matching folder of the copy.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Without `sync`: where to run on the other machine (its home without one).
    #[serde(default)]
    pub remote_cwd: Option<String>,
    #[serde(default)]
    pub sync: bool,
    #[serde(default)]
    pub env: Vec<(String, String)>,
    #[serde(default)]
    pub stdin: bool,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// A named shell (folder and exports carry over between runs).
    #[serde(default)]
    pub shell: Option<String>,
    /// Keep running if this client goes away.
    #[serde(default)]
    pub detach: bool,
    /// The agent session asking, as this runner tells from the client's process (never on the
    /// wire): a run on another machine is its act, which kiln must take first (docs/ci.md W27-R08).
    #[serde(skip)]
    pub asker: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Ev {
    Status(String),
    /// JSON: `{"runners": [...ranked], "passed": [[name, why]...]}`.
    Runners(String),
    Picked { runner: String, why: String },
    Syncing { files: u64, bytes: u64 },
    Synced { root: String, sent: u32, removed: u32, bytes: u64, ms: u64 },
    Started { runner: String, pid: u32, cwd: String },
    Output { err: bool, data: Vec<u8> },
    Exit { code: Option<i32>, error: Option<String>, lost: u64 },
    Error(String),
    Forgot(bool),
    /// The server's answer to [`Req::Ask`] (JSON).
    Answer(String),
    /// A line about how a run went that isn't its output: whether it was reused, or recorded
    /// (`super::reuse`). Appended last: postcard encodes variants by index.
    Note(String),
    /// Placement's answer for the run (docs/needs.md E5; JSON `kiln_ci::place::Answer`): at once
    /// (starting, queued with its place, expected start and reasons, or never with why), then each
    /// change as it happens, pushed, never polled. Appended last: postcard encodes variants by
    /// index; a client from before skips it.
    Placed { json: String },
}

static SOCKET: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// This runner's reach socket, once it serves one: agents it starts get it as `KILN_REACH_SOCK`.
pub fn set_socket(path: String) {
    let _ = SOCKET.set(path);
}

pub fn socket() -> Option<&'static str> {
    SOCKET.get().map(String::as_str)
}

/// Where this process's reach socket lives: beside the harness socket, named alike.
pub fn path(name: &str, home: &Path) -> PathBuf {
    let h = crate::harness::socket::path(name, home);
    let file = h.file_name().map(|f| f.to_string_lossy().replacen("harness-", "reach-", 1)).unwrap_or_else(|| "reach.sock".into());
    h.with_file_name(file)
}

/// The reach socket of a runner on this machine, for a shell outside kiln's terminals: the one
/// named in `KILN_REACH_SOCK`, else the only one a runner answers on, else the machine's installed
/// runner's among several ([`choose_socket`]).
pub fn find_socket() -> anyhow::Result<PathBuf> {
    if let Some(p) = std::env::var_os("KILN_REACH_SOCK").map(PathBuf::from) {
        return Ok(p);
    }
    let mut dirs = vec![std::env::temp_dir().join(format!("kiln-{}", crate::os::uid()))];
    if let Some(r) = std::env::var_os("XDG_RUNTIME_DIR") {
        dirs.insert(0, PathBuf::from(r).join("kiln"));
    }
    let found: Vec<PathBuf> = dirs
        .iter()
        .filter_map(|d| std::fs::read_dir(d).ok())
        .flat_map(|rd| rd.flatten().map(|e| e.path()))
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("reach-") && n.to_string_lossy().ends_with(".sock")))
        .collect();
    let installed = crate::harness::socket::tag_of(&crate::config::default_data_dir().join("agent"));
    choose_socket(found, &installed)
}

/// Of the reach sockets `found`, the one a runner answers on: a file nothing answers on is one a
/// runner left as it ended (`crate::harness::socket::sweep` removes them in time). Among several,
/// the machine's installed runner's, whose state directory is the default (its name ends in
/// `-<installed>.sock`), before a test's or a lab's.
fn choose_socket(mut found: Vec<PathBuf>, installed: &str) -> anyhow::Result<PathBuf> {
    #[cfg(unix)]
    found.retain(|p| crate::harness::socket::answers(p));
    found.sort();
    if found.len() > 1 {
        let suffix = format!("-{installed}.sock");
        let mut theirs: Vec<&PathBuf> = found.iter().filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().ends_with(&suffix))).collect();
        if theirs.len() == 1
            && let Some(one) = theirs.pop()
        {
            return Ok(one.clone());
        }
    }
    match found.len() {
        0 => anyhow::bail!("no kiln runner on this machine takes reach requests (is `kiln agent` running here, and new enough?)"),
        1 => Ok(found.remove(0)),
        _ => anyhow::bail!("several kiln runners run here; pick one with KILN_REACH_SOCK ({})", found.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")),
    }
}

/// Pick the runner for `target` from the directory, or say why none suits.
pub async fn pick(handle: &Handle, me: &str, target: &str) -> anyhow::Result<directory::Choice> {
    let runners = handle.directory().await?;
    let want = Want::parse(target)?;
    directory::pick(&runners, &want, me)
}

/// The directory ranked for `want` (everyone when None), as JSON for tools.
pub async fn ranked(handle: &Handle, me: &str, want: Option<&str>) -> anyhow::Result<serde_json::Value> {
    let runners: Vec<RunnerInfo> = handle.directory().await?;
    let want = want.map(Want::parse).transpose()?.unwrap_or_default();
    let (ok, passed) = directory::rank(&runners, &want, me);
    Ok(serde_json::json!({ "me": me, "want": want.describe(), "runners": ok, "passed": passed }))
}

/// Copy the tree at `root` to `peer` (as `key`), reporting progress to `ev`. Started over (it then
/// sends only what still differs) when a link drops in the middle.
pub async fn sync_tree(handle: &Handle, peer: &str, root: &Path, key: &str, ev: &async_channel::Sender<Ev>) -> anyhow::Result<(String, u32, u32, u64)> {
    let mut last_err = None;
    for attempt in 0..4 {
        if attempt > 0 {
            crate::rt::sleep(Duration::from_millis(500 * attempt)).await;
        }
        match sync_once(handle, peer, root, key, ev).await {
            Ok(done) => return Ok(done),
            Err(e) if format!("{e:#}").contains("interrupted") => last_err = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("sync failed")))
}

async fn sync_once(handle: &Handle, peer: &str, root: &Path, key: &str, ev: &async_channel::Sender<Ev>) -> anyhow::Result<(String, u32, u32, u64)> {
    let (tx, rx) = async_channel::unbounded::<SyncSay>();
    handle.sync(peer, key, tx).await;
    let leave = || handle.leave(peer, None, Some(key.to_owned()));
    let tree = std::sync::Arc::new(
        crate::rt::blocking({
            let root = root.to_path_buf();
            move || sync::scan(&root)
        })
        .await??,
    );
    let entries = &tree.entries;
    let total: u64 = entries.iter().map(|e| e.size).sum();
    let _ = ev.send(Ev::Syncing { files: entries.len() as u64, bytes: total }).await;
    let chunks: Vec<Vec<wire::Entry>> = entries.chunks(2000).map(<[wire::Entry]>::to_vec).collect();
    let n = chunks.len().max(1);
    if chunks.is_empty() {
        handle.sync_ask(peer, SyncAsk::Plan { key: key.to_owned(), entries: Vec::new(), start: true, more: false }).await;
    }
    for (i, c) in chunks.into_iter().enumerate() {
        handle.sync_ask(peer, SyncAsk::Plan { key: key.to_owned(), entries: c, start: i == 0, more: i + 1 < n }).await;
    }
    let wait = |rx: &async_channel::Receiver<SyncSay>, d: Duration| {
        let rx = rx.clone();
        async move {
            match crate::rt::timeout(d, rx.recv()).await {
                Ok(Ok(s)) => Ok(s),
                Ok(Err(_)) => Err(anyhow::anyhow!("interrupted: the sync's link closed")),
                Err(_) => Err(anyhow::anyhow!("{peer} did not answer the sync in {}s", d.as_secs())),
            }
        }
    };
    let paths = loop {
        match wait(&rx, Duration::from_secs(120)).await {
            Ok(SyncSay::Need { paths, .. }) => break paths,
            Ok(SyncSay::Failed { error, .. }) => {
                leave().await;
                anyhow::bail!("{error}");
            }
            Ok(_) => {}
            Err(e) => {
                leave().await;
                return Err(e);
            }
        }
    };
    let mut sent: u64 = 0;
    let mut got: u64 = 0;
    for p in paths {
        let mut offset = 0u64;
        loop {
            let piece = crate::rt::blocking({
                let src = tree.source(&p);
                move || sync::read_piece(&src, offset)
            })
            .await;
            let (data, last) = match piece {
                Ok(Ok(x)) => x,
                // Gone or unreadable since the list was made: sent empty (the next sync fixes it).
                _ => (Vec::new(), true),
            };
            let len = data.len() as u64;
            handle.sync_ask(peer, SyncAsk::File { key: key.to_owned(), path: p.clone(), offset, data, last }).await;
            sent += len;
            offset += len;
            // Keep at most a window of file bytes in flight.
            while sent.saturating_sub(got) > wire::WINDOW {
                match wait(&rx, Duration::from_secs(60)).await {
                    Ok(SyncSay::Got { bytes, .. }) => got = bytes,
                    Ok(SyncSay::Failed { error, .. }) => {
                        leave().await;
                        anyhow::bail!("{error}");
                    }
                    Ok(_) => {}
                    Err(e) => {
                        leave().await;
                        return Err(e);
                    }
                }
            }
            if last {
                break;
            }
        }
    }
    handle.sync_ask(peer, SyncAsk::Commit { key: key.to_owned() }).await;
    loop {
        match wait(&rx, Duration::from_secs(300)).await {
            Ok(SyncSay::Done { root, written, removed, bytes, .. }) => return Ok((root, written, removed, bytes)),
            Ok(SyncSay::Failed { error, .. }) => anyhow::bail!("{error}"),
            Ok(_) => {}
            Err(e) => {
                leave().await;
                return Err(e);
            }
        }
    }
}

/// Run what `req` says, start to end: pick, sync, run, stream. Control (input, signals) comes on
/// `ctl`; when it closes before the run ends, the run is stopped (unless `detach`).
pub async fn run(handle: &Handle, me: &str, req: RunReq, ev: async_channel::Sender<Ev>, ctl: async_channel::Receiver<Req>) {
    if let Err(e) = super::history::run(handle, me, req, &ev, ctl).await {
        let _ = ev.send(Ev::Error(format!("{e:#}"))).await;
    }
}

/// The command `req` asks for, as a runner is asked to run it. A detached one asks its runner to
/// wait for its asker for ever ([`wire::DETACHED`], docs/reach.md §4.2).
fn spec_of(req: &RunReq) -> RunSpec {
    RunSpec {
        command: req.command.clone(),
        env: req.env.clone(),
        stdin: req.stdin,
        timeout_ms: req.timeout_ms,
        linger_ms: req.detach.then_some(wire::DETACHED),
        shell: req.shell.clone(),
        ..RunSpec::default()
    }
}

/// One try at `req` (`super::history::run` tries again where `[reach] retry_oom` says), its size and
/// what it used kept in `t` (docs/reach.md, "Sizes from history").
pub(super) async fn run_try(handle: &Handle, me: &str, req: RunReq, ev: &async_channel::Sender<Ev>, ctl: async_channel::Receiver<Req>, t: &super::history::Try) -> anyhow::Result<()> {
    // An agent session's run on another machine is its act: it goes only once kiln takes it as
    // the session's holder's now (a copy left behind's is refused, and so is any while kiln can't
    // be asked), docs/ci.md W27-R08. One that names another machine is asked about first.
    let elsewhere = !req.target.is_empty() && req.target != me;
    let taken = |on: String| {
        let session = req.asker.clone();
        let what = format!("a run on {on}: {}", req.command);
        async move {
            match session {
                Some(s) => crate::runner::custody::act_taken(handle, &s, &what).await.map_err(|why| anyhow::anyhow!("refused: {why}")),
                None => Ok(()),
            }
        }
    };
    if elsewhere {
        taken(req.target.clone()).await?;
    }
    // Placed where the room is (docs/needs.md E5): this node owns the run, and the runner with room
    // admits it. A runner named, or placement not ready, picks as before: by the sizes history
    // predicts (`t`, docs/reach.md "Sizes from history").
    let placed = place(handle, me, &req, ev).await?;
    let (peer, claim, picked) = match placed {
        Some(p) => {
            let _ = ev.send(Ev::Picked { runner: p.runner.clone(), why: "its room was first".to_owned() }).await;
            (p.runner, Some((p.run, p.fence)), None)
        }
        None => {
            let choice = t.pick(handle, me, &req).await?;
            let _ = ev.send(Ev::Picked { runner: choice.runner.name.clone(), why: choice.why.clone() }).await;
            (choice.runner.name.clone(), None, Some(choice.runner))
        }
    };
    let mut spec = spec_of(&req);
    if peer == me {
        // This machine suits best: no copy, no link — the folder as it is.
        spec.cwd = req.cwd.clone().or(req.remote_cwd.clone());
        let local = claim.map(|(run, _)| (handle.clone(), run));
        return run_here(handle, spec, &req, ev, ctl, t, local).await;
    }
    if !elsewhere {
        taken(peer.clone()).await?;
    }
    handle.connect(&peer).await?;
    if req.sync {
        let cwd = req.cwd.clone().ok_or_else(|| anyhow::anyhow!("a sync needs the folder to copy"))?;
        let (root, rel) = crate::rt::blocking(move || sync::find_root(Path::new(&cwd))).await?;
        let key = sync::mirror_key(me, &root);
        let t = Instant::now();
        let (remote_root, sent, removed, bytes) = sync_tree(handle, &peer, &root, &key, ev).await?;
        let ms = u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX);
        let _ = ev.send(Ev::Synced { root: remote_root, sent, removed, bytes, ms }).await;
        spec.mirror = Some(key);
        spec.cwd = Some(rel.to_string_lossy().into_owned()).filter(|s| !s.is_empty());
    } else {
        spec.cwd = req.remote_cwd.clone();
    }
    let (says_tx, says) = async_channel::unbounded::<Say>();
    let id = match (&claim, &picked) {
        (Some((run, fence)), _) => handle.place_run(&peer, run, *fence, spec, says_tx).await?,
        (None, Some(runner)) => t.start(handle, runner, spec, says_tx).await?,
        (None, None) => handle.run(&peer, spec, says_tx).await?,
    };
    // Set once the client went away: by then the run should have ended, else it is killed.
    let mut stop_by: Option<Instant> = None;
    loop {
        use futures::FutureExt as _;
        let ctl_next = async {
            if stop_by.is_some() {
                futures::future::pending::<Result<Req, async_channel::RecvError>>().await
            } else {
                ctl.recv().await
            }
        };
        let deadline = async {
            match stop_by {
                Some(at) => crate::rt::sleep_until(at).await,
                None => futures::future::pending::<()>().await,
            }
        };
        futures::select_biased! {
            s = says.recv().fuse() => match s {
                Ok(Say::Started { pid, cwd, .. }) => {
                    let _ = ev.send(Ev::Started { runner: peer.clone(), pid, cwd }).await;
                }
                Ok(Say::Output { stream, data, .. }) => {
                    let _ = ev.send(Ev::Output { err: stream == Stream::Err, data }).await;
                }
                Ok(Say::Used { used, .. }) => t.used(&used),
                Ok(Say::Exit { code, error, lost, .. }) => return t.exit(ev, &req, stop_by.is_some(), code, error, lost).await,
                Ok(_) => {}
                Err(_) => anyhow::bail!("lost track of the run"),
            },
            c = ctl_next.fuse() => match c {
                Ok(Req::Input(data)) => { handle.send(&peer, Ask::Stdin { id, data }).await; }
                Ok(Req::InputEnd) => { handle.send(&peer, Ask::StdinEnd { id }).await; }
                Ok(Req::Signal(s)) => { handle.send(&peer, Ask::Signal { id, signal: s }).await; }
                Ok(_) => {}
                Err(_) => {
                    if req.detach {
                        // It keeps running over there, followed on alone: its output taken and
                        // let go, its link made again after a drop, until it ends.
                        handle.detach(&peer, id).await;
                        return Ok(());
                    }
                    handle.send(&peer, Ask::Signal { id, signal: Signal::Terminate }).await;
                    stop_by = Some(Instant::now() + Duration::from_secs(5));
                }
            },
            () = deadline.fuse() => {
                handle.send(&peer, Ask::Signal { id, signal: Signal::Kill }).await;
                handle.leave(&peer, Some(id), None).await;
                return Ok(());
            }
        }
    }
}

/// A run placement gave a runner: which, and its claim there.
struct Placing {
    runner: String,
    run: String,
    fence: kiln_ci::sched::Fence,
}

/// Places a run (docs/needs.md E5) when its target is what it needs (no runner named) and this
/// node's placement is ready: its asker is told each answer as it comes, at once and on every
/// change, until it's to start somewhere (the runner's room first), and after, as it starts and
/// ends. `None`: placed as before (a runner named, placement not ready, or only older runners
/// could take it).
async fn place(handle: &Handle, me: &str, req: &RunReq, ev: &async_channel::Sender<Ev>) -> anyhow::Result<Option<Placing>> {
    use super::place::Placed;
    let Ok(needs) = kiln_ci::sched::needs::parse(&req.target) else { return Ok(None) };
    let run = format!("{me}-{}", crate::util::new_id());
    let repo = req.cwd.as_deref().and_then(|c| Path::new(c).file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    // A person at a terminal, or an agent's turn, waits on it (docs/needs.md §7).
    let ask = kiln_ci::place::Ask { run: run.clone(), needs, class: kiln_ci::sched::Class::Interactive, repo, actor: req.asker.clone().unwrap_or_else(|| "person".to_owned()), timeout: req.timeout_ms, ..kiln_ci::place::Ask::default() };
    let (tx, rx) = async_channel::unbounded::<Placed>();
    if !handle.place(ask, tx).await? {
        return Ok(None);
    }
    let tell = |a: &kiln_ci::place::Answer| Ev::Placed { json: serde_json::to_string(a).unwrap_or_default() };
    loop {
        match rx.recv().await {
            Ok(Placed::Answer(a)) => {
                let _ = ev.send(tell(&a)).await;
                if let kiln_ci::place::Answer::Never { .. } = a {
                    anyhow::bail!("{}", a.words(crate::util::now_ms()));
                }
            }
            Ok(Placed::Go { runner, fence }) => {
                // Its claim may have lapsed meanwhile: then it waits on, at the runners probed.
                if !handle.place_starting(&run, &runner, fence).await? {
                    continue;
                }
                // What it hears after (started, ended) goes on to its asker.
                let ev = ev.clone();
                crate::rt::spawn(async move {
                    while let Ok(p) = rx.recv().await {
                        if let Placed::Answer(a) = p
                            && ev.send(tell(&a)).await.is_err()
                        {
                            return;
                        }
                    }
                });
                return Ok(Some(Placing { runner, run, fence }));
            }
            Ok(Placed::Fallback { .. }) => {
                handle.place_cancel(&run).await;
                return Ok(None);
            }
            Err(_) => anyhow::bail!("placement went away before the run started"),
        }
    }
}

/// Run on this machine (it suited best): the same events, no link. A detached run goes on after
/// its client, its events read to its end so that its output never makes it wait. What it used is
/// kept under its key (`t`, docs/reach.md "Sizes from history"). A run placement started here
/// (`placed`) says when it's up and when it ended, so its room is free at once.
async fn run_here(handle: &Handle, spec: RunSpec, req: &RunReq, ev: &async_channel::Sender<Ev>, ctl: async_channel::Receiver<Req>, t: &super::history::Try, placed: Option<(Handle, String)>) -> anyhow::Result<()> {
    let cwd = super::serve::resolve_cwd(&spec, None)?;
    let (tx, rx) = async_channel::bounded::<super::serve::Event>(16);
    let shell = spec.shell.as_deref().map(|s| crate::os::home_dir().join(".kiln").join("reach").join("shells").join(s));
    let stdin = super::serve::start(spec, cwd, shell, Vec::new(), tx);
    let mut pid = None;
    loop {
        use futures::FutureExt as _;
        futures::select_biased! {
            e = rx.recv().fuse() => match e {
                Ok(super::serve::Event::Started { pid: p, cwd }) => {
                    pid = Some(p);
                    if let Some((h, run)) = &placed {
                        h.place_local(run, None).await;
                    }
                    let _ = ev.send(Ev::Started { runner: String::new(), pid: p, cwd }).await;
                }
                Ok(super::serve::Event::Output { stream, data }) => { let _ = ev.send(Ev::Output { err: stream == Stream::Err, data }).await; }
                Ok(super::serve::Event::Exit { code, error, used }) => {
                    if let Some((h, run)) = &placed {
                        h.place_local(run, Some(error.is_none())).await;
                    }
                    return t.exit_here(handle, ev, req, code, error, used.as_deref()).await;
                }
                Err(_) => anyhow::bail!("lost track of the run"),
            },
            c = ctl.recv().fuse() => match c {
                Ok(Req::Input(d)) => { let _ = stdin.send(d).await; }
                Ok(Req::InputEnd) => { let _ = stdin.send(Vec::new()).await; }
                Ok(Req::Signal(s)) => if let Some(p) = pid { super::serve::signal_group(p, s); },
                Ok(_) => {}
                Err(_) if req.detach => break,
                Err(_) => {
                    if let Some(p) = pid {
                        super::serve::signal_group(p, Signal::Terminate);
                    }
                    return Ok(());
                }
            },
        }
    }
    // Its input ends with its client.
    drop(stdin);
    let (t, handle) = (t.clone(), handle.clone());
    crate::rt::spawn(async move {
        while let Ok(e) = rx.recv().await {
            if let super::serve::Event::Exit { error, used, .. } = e {
                if let Some((h, run)) = &placed {
                    h.place_local(run, Some(error.is_none())).await;
                }
                t.record(&handle, used.as_deref());
                return;
            }
        }
    });
    Ok(())
}

/// Serve the socket at `path` until this process exits. Unix only.
#[cfg(unix)]
pub fn serve(handle: Handle, me: String, path: PathBuf, take: bool) -> anyhow::Result<PathBuf> {
    use std::os::fd::{AsRawFd as _, OwnedFd};

    // Kept as `kiln spawn`'s is (crate::harness::socket::Kept): `take` when a new build takes the
    // machine over, else served once nothing else serves it, and served again if left dead.
    let mut kept = crate::harness::socket::Kept::new(path.clone(), take, "`kiln run`")?;
    crate::rt::spawn(async move {
        loop {
            let Some(listener) = kept.listener() else {
                crate::rt::sleep(Duration::from_secs(1)).await;
                kept.check();
                continue;
            };
            let fd = listener.as_raw_fd();
            match listener.accept() {
                Ok((stream, _)) => {
                    if stream.set_nonblocking(true).is_err() {
                        continue;
                    }
                    let (handle, me) = (handle.clone(), me.clone());
                    crate::rt::spawn(async move { connection(handle, me, std::sync::Arc::new(OwnedFd::from(stream))).await });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // Woken when a connection waits, and each second regardless, to look at its
                    // path while it's quiet.
                    if let Ok(Err(_)) = crate::rt::timeout(Duration::from_secs(1), hyper_rt::readiness::readable(fd)).await {
                        crate::rt::sleep(Duration::from_millis(200)).await;
                    }
                    kept.check();
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    tracing::warn!(error = %e, "reach socket accept failed");
                    crate::rt::sleep(Duration::from_millis(500)).await;
                }
            }
        }
    });
    Ok(path)
}

#[cfg(not(unix))]
pub fn serve(_handle: Handle, _me: String, _path: PathBuf, _take: bool) -> anyhow::Result<PathBuf> {
    anyhow::bail!("reach's local socket needs Unix sockets")
}

/// The process at the other end of a connection, where the system says.
#[cfg(target_os = "linux")]
fn peer_pid(fd: &std::os::fd::OwnedFd) -> Option<u32> {
    nix::sys::socket::getsockopt(fd, nix::sys::socket::sockopt::PeerCredentials).ok().and_then(|c| u32::try_from(c.pid()).ok())
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn peer_pid(fd: &std::os::fd::OwnedFd) -> Option<u32> {
    nix::sys::socket::getsockopt(fd, nix::sys::socket::sockopt::LocalPeerPid).ok().and_then(|p| u32::try_from(p).ok())
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos", target_os = "ios"))))]
fn peer_pid(_fd: &std::os::fd::OwnedFd) -> Option<u32> {
    None
}

/// One client: its first request decides what the connection is.
#[cfg(unix)]
async fn connection(handle: Handle, me: String, fd: std::sync::Arc<std::os::fd::OwnedFd>) {
    let (req_tx, req_rx) = async_channel::unbounded::<Req>();
    // Requests in (the first, then a run's input and signals), until the client closes.
    let reader = std::sync::Arc::clone(&fd);
    crate::rt::spawn(async move {
        let mut de = wire::Deframer::default();
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            match crate::os::proc::read_chunk(&reader, &mut chunk).await {
                Ok(Some(n)) if n > 0 => de.push(chunk.get(..n).unwrap_or_default()),
                _ => return,
            }
            loop {
                match de.take::<Req>() {
                    Ok(Some(r)) => {
                        if req_tx.send(r).await.is_err() {
                            return;
                        }
                    }
                    Ok(None) => break,
                    Err(_) => return,
                }
            }
        }
    });
    let (ev_tx, ev_rx) = async_channel::unbounded::<Ev>();
    // Events out, until the request is answered.
    let writer = std::sync::Arc::clone(&fd);
    let out = crate::rt::spawn(async move {
        while let Ok(e) = ev_rx.recv().await {
            let Ok(b) = wire::frame(&e) else { continue };
            if crate::os::proc::write_all(&writer, &b).await.is_err() {
                return;
            }
        }
    });
    let Ok(mut first) = crate::rt::timeout(Duration::from_secs(10), req_rx.recv()).await.unwrap_or(Err(async_channel::RecvError)) else { return };
    // A run's asker: the agent session the client's process belongs to, if any.
    if let Req::Run(r) | Req::RunReuse(r) = &mut first {
        let pid = peer_pid(&fd);
        r.asker = crate::rt::blocking(move || pid.and_then(crate::runner::custody::session_of_pid)).await.ok().flatten();
    }
    match first {
        Req::Status => {
            let s = handle.status().await.map(|v| v.to_string()).unwrap_or_else(|e| format!("{{\"error\": {:?}}}", format!("{e:#}")));
            let _ = ev_tx.send(Ev::Status(s)).await;
        }
        Req::Runners { want } => match ranked(&handle, &me, want.as_deref()).await {
            Ok(v) => {
                let _ = ev_tx.send(Ev::Runners(v.to_string())).await;
            }
            Err(e) => {
                let _ = ev_tx.send(Ev::Error(format!("{e:#}"))).await;
            }
        },
        Req::Forget { runner } => {
            // Named as people see it (its label, docs/machines.md): the runner kiln knows by its machine id.
            let named = handle.directory().await.ok().and_then(|all| all.into_iter().find(|r| r.label.as_deref() == Some(runner.as_str())).map(|r| r.name));
            let ok = handle.forget(named.as_deref().unwrap_or(&runner)).await.unwrap_or(false);
            let _ = ev_tx.send(Ev::Forgot(ok)).await;
        }
        Req::Ask { json } => {
            let ev = match super::history::for_agent(&handle, json).await {
                Ok(a) => Ev::Answer(a),
                Err(e) => Ev::Error(format!("{e:#}")),
            };
            let _ = ev_tx.send(ev).await;
        }
        Req::Browser { json } => {
            let ev = match crate::browser::ask(&json).await {
                Ok(a) => Ev::Answer(a),
                Err(e) => Ev::Error(format!("{e:#}")),
            };
            let _ = ev_tx.send(ev).await;
        }
        Req::Run(r) => run(&handle, &me, *r, ev_tx.clone(), req_rx).await,
        Req::RunReuse(r) => super::reuse::run(&handle, &me, *r, ev_tx.clone(), req_rx).await,
        Req::Input(_) | Req::InputEnd | Req::Signal(_) => {
            let _ = ev_tx.send(Ev::Error("nothing is running on this connection".into())).await;
        }
    }
    drop(ev_tx);
    let _ = out.await;
}

/// A client of the local socket (the CLI and the MCP server).
#[cfg(unix)]
pub struct Client {
    stream: std::os::unix::net::UnixStream,
    de: wire::Deframer,
}

#[cfg(unix)]
impl Client {
    pub fn connect(path: &Path) -> anyhow::Result<Self> {
        let stream = std::os::unix::net::UnixStream::connect(path).map_err(|e| anyhow::anyhow!("cannot reach the kiln runner at {}: {e}", path.display()))?;
        Ok(Self { stream, de: wire::Deframer::default() })
    }

    pub fn send(&mut self, r: &Req) -> anyhow::Result<()> {
        use std::io::Write as _;
        self.stream.write_all(&wire::frame(r)?)?;
        Ok(())
    }

    /// The next event (blocking); None when the runner closed the connection.
    pub fn event(&mut self) -> anyhow::Result<Option<Ev>> {
        use std::io::Read as _;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            if let Some(e) = self.de.take::<Ev>()? {
                return Ok(Some(e));
            }
            let n = self.stream.read(&mut buf)?;
            if n == 0 {
                return Ok(None);
            }
            self.de.push(buf.get(..n).unwrap_or_default());
        }
    }

    /// A second handle on the connection, to send input or signals while another thread reads.
    pub fn sender(&self) -> anyhow::Result<std::os::unix::net::UnixStream> {
        Ok(self.stream.try_clone()?)
    }
}

/// Send `r` on a cloned connection.
#[cfg(unix)]
pub fn send_on(stream: &mut std::os::unix::net::UnixStream, r: &Req) -> anyhow::Result<()> {
    use std::io::Write as _;
    stream.write_all(&wire::frame(r)?)?;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// Discovery counts only sockets a runner answers on, and among several takes the installed
    /// runner's: 1,532 left by ended test and lab runners kept `kiln reach mcp` from finding it.
    #[test]
    fn the_runner_s_socket_is_found_among_ended_ones_and_others() -> anyhow::Result<()> {
        use std::os::unix::net::UnixListener;
        let dir = tempfile::tempdir()?;
        let path = |n: &str| dir.path().join(n);
        drop(UnixListener::bind(path("reach-lab-0-deadbeef.sock"))?);
        let installed = path("reach-adacachy-4b48ed43.sock");
        let _runner = UnixListener::bind(&installed)?;
        let all = || std::fs::read_dir(dir.path()).map(|rd| rd.flatten().map(|e| e.path()).collect::<Vec<_>>());
        anyhow::ensure!(choose_socket(all()?, "00000000")? == installed, "the one live socket wasn't chosen");
        let _lab = UnixListener::bind(path("reach-lab-1-0badf00d.sock"))?;
        anyhow::ensure!(choose_socket(all()?, "4b48ed43")? == installed, "the installed runner's wasn't chosen among live ones");
        anyhow::ensure!(choose_socket(all()?, "00000000").is_err(), "two live runners, neither installed, were told apart");
        anyhow::ensure!(choose_socket(vec![path("reach-lab-0-deadbeef.sock")], "4b48ed43").is_err(), "an ended runner's socket was chosen");
        Ok(())
    }

    /// A detached run asks its runner to wait for its asker for ever, and only a detached one
    /// (docs/reach.md §4.2): a runner from before then never stops it for want of its asker.
    #[test]
    fn a_detached_run_asks_to_be_waited_for_for_ever() {
        let req = RunReq { command: "true".into(), detach: true, ..RunReq::default() };
        assert_eq!(spec_of(&req).linger_ms, Some(wire::DETACHED));
        assert_eq!(spec_of(&RunReq { detach: false, ..req }).linger_ms, None);
    }
}

