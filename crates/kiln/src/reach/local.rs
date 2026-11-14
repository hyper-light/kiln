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
use sha2::{Digest as _, Sha256};

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
    /// News of a run an agent session's MCP server follows for it (JSON `super::told::RunNews`),
    /// for its agent's news (docs/reach.md §7): the session is the one the client's process belongs
    /// to. Answered with [`Ev::Answer`] (`{"told": bool}`) or [`Ev::Error`]. Appended last.
    Told(String),
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
    /// The client's process, as this runner tells it (never on the wire): what its runs' news is
    /// told for (`super::told`, docs/reach.md §7).
    #[serde(skip)]
    pub client: Option<u32>,
}

/// A run refused before anything of it starts, by kind (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d,
/// step 32). It goes as the run's error ([`Ev::Error`]), its words led by its code, `refused
/// (<code>): …`, which `kiln run` prints as they are and `kiln reach mcp` gives agents beside them
/// as `refused` ([`Refused::code_of`]), its tool's and `kiln.machines.*`'s answer alike.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// Asked for by agent session `session`, which is held in a microVM of its own: its commands run
    /// in microVMs alone, on runners that start them, and no runner runs kiln's commands in a microVM
    /// yet (a microVM executor for them is owed), so it runs nowhere, never on a bare host.
    Isolated { session: String },
}

impl Refused {
    /// Every code there is.
    pub const CODES: &[&str] = &["isolated"];

    pub fn code(&self) -> &'static str {
        match self {
            Refused::Isolated { .. } => "isolated",
        }
    }

    /// The code of a run's error that says one: its words as [`Refused`] writes them.
    pub fn code_of(words: &str) -> Option<&'static str> {
        let (code, _) = words.strip_prefix("refused (")?.split_once("): ")?;
        Self::CODES.iter().find(|c| **c == code).copied()
    }
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::Isolated { session } => write!(
                f,
                "refused ({}): agent session {session} is held in a microVM of its own, so its commands run only in microVMs, on runners that start them, and no runner runs kiln's commands in a microVM yet: none of them runs on a bare host (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d step 32)",
                self.code()
            ),
        }
    }
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

/// The socket [`find_socket`] gives a test that serves a lab's node on one of its own (reach's
/// doors tests drive `kiln run` by it, docs/ci.md W05-R03): a test's `kiln run` never reaches this
/// machine's runner, whatever its environment names.
#[cfg(test)]
pub(super) static TEST_SOCKET: parking_lot::Mutex<Option<PathBuf>> = parking_lot::Mutex::new(None);

/// The reach socket of a runner on this machine, for a shell outside kiln's terminals: the one
/// named in `KILN_REACH_SOCK`, else the only one a runner answers on, else the machine's installed
/// runner's among several ([`choose_socket`]).
pub fn find_socket() -> anyhow::Result<PathBuf> {
    #[cfg(test)]
    if let Some(p) = TEST_SOCKET.lock().clone() {
        return Ok(p);
    }
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
/// runner left as it ended (`crate::harness::socket::sweep` removes them in time). One its probe
/// couldn't tell of (`crate::harness::socket::Probe::Unknown`) may be a runner's, and is counted:
/// never dropped as ended for a probe that failed. Among several, the machine's installed runner's,
/// whose state directory is the default (its name ends in `-<installed>.sock`), before a test's or
/// a lab's.
fn choose_socket(mut found: Vec<PathBuf>, installed: &str) -> anyhow::Result<PathBuf> {
    #[cfg(unix)]
    found.retain(|p| crate::harness::socket::probe(p) != crate::harness::socket::Probe::Dead);
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
    // The name people see of each, passed over too (docs/machines.md: a label, never an id, where there's one).
    let labels: serde_json::Map<String, serde_json::Value> = runners.iter().filter_map(|r| r.label.clone().map(|l| (r.name.clone(), serde_json::Value::String(l)))).collect();
    Ok(serde_json::json!({ "me": me, "want": want.describe(), "runners": ok, "passed": passed, "labels": labels }))
}

/// What a sync did: where the copy is over there, what it wrote and removed, and the tree it
/// handed on, by its digest (docs/ci.md W27-R08).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Synced {
    pub root: String,
    pub written: u32,
    pub removed: u32,
    pub bytes: u64,
    pub tree: String,
}

/// Copy the tree at `root` to `peer` (as `key`), reporting progress to `ev`, handed on by `me`
/// (`super::custody`). Started over (it then sends only what still differs) when a link drops in
/// the middle.
#[allow(clippy::disallowed_methods)] // (b) sync retry, linear backoff, picked
pub async fn sync_tree(handle: &Handle, peer: &str, root: &Path, key: &str, me: Option<&super::custody::Signer>, ev: &crate::memory::queue::Sender<Ev>) -> anyhow::Result<Synced> {
    let mut last_err = None;
    for attempt in 0..4 {
        if attempt > 0 {
            crate::rt::sleep(Duration::from_millis(500 * attempt)).await;
        }
        match sync_once(handle, peer, root, key, me, ev).await {
            Ok(done) => return Ok(done),
            Err(e) if format!("{e:#}").contains("interrupted") => last_err = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("sync failed")))
}

/// The digest of `tree` as this side handed it on: each file sent by the SHA-256 of the bytes it
/// sent (`sent`), the others by what its kept hashes (`hashes`) say, or hashed now. The hashes are
/// kept for the next sync.
fn handed_tree(tree: &sync::Tree, sent: &std::collections::HashMap<String, String>, mut hashes: sync::Hashes) -> anyhow::Result<String> {
    let digest = super::reuse::digest_of(&tree.entries, |e| match sent.get(&e.path) {
        Some(sha) => Ok(sha.clone()),
        None => hashes.hash(&e.path, &tree.source(&e.path)),
    })?;
    // What was sent is kept by the stat its file had in the list, if it still has it.
    for e in tree.entries.iter().filter(|e| e.kind == wire::EntryKind::File) {
        let source = tree.source(&e.path);
        if let Some(sha) = sent.get(&e.path)
            && sync::Stat::of_path(&source).is_some_and(|s| s.size == e.size && s.mtime_ns == e.mtime_ns)
        {
            hashes.put(&e.path, &source, sha.clone());
        }
    }
    let listed: std::collections::HashSet<&str> = tree.entries.iter().filter(|e| e.kind == wire::EntryKind::File).map(|e| e.path.as_str()).collect();
    hashes.save(&listed)?;
    Ok(digest)
}

async fn sync_once(handle: &Handle, peer: &str, root: &Path, key: &str, me: Option<&super::custody::Signer>, ev: &crate::memory::queue::Sender<Ev>) -> anyhow::Result<Synced> {
    let (tx, rx) = crate::memory::queue::unbounded::<SyncSay>(&crate::memory::owners::CHANNELS);
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
    let wait = |rx: &crate::memory::queue::Receiver<SyncSay>, d: Duration| {
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
    // What each file sent hashed to, as it went (docs/ci.md W27-R08), and a test build's
    // `sync_file_changed`: the first piece sent has a byte changed after it was hashed.
    let mut sent_sha: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut change = crate::runner::chaos::get().is_some_and(|c| c.sync_file_changed);
    for p in paths {
        let mut offset = 0u64;
        let mut sha = Sha256::new();
        loop {
            let piece = crate::rt::blocking({
                let src = tree.source(&p);
                move || sync::read_piece(&src, offset)
            })
            .await;
            let (mut data, last) = match piece {
                Ok(Ok(x)) => x,
                // Gone or unreadable since the list was made: sent empty (the next sync fixes it).
                _ => (Vec::new(), true),
            };
            sha.update(&data);
            if change
                && !p.starts_with(".git/")
                && let Some(b) = data.first_mut()
            {
                *b ^= 0x01;
                change = false;
            }
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
                sent_sha.insert(p.clone(), hex::encode(std::mem::take(&mut sha).finalize()));
                break;
            }
        }
    }
    // The tree as handed on, by its digest; its hand-on goes ahead of the commit on a link that
    // carries it, and to kiln either way (docs/ci.md W27-R08).
    let hashes_at = handle.hashes_of(key);
    let digest = {
        let t = std::sync::Arc::clone(&tree);
        crate::rt::blocking(move || handed_tree(&t, &sent_sha, sync::Hashes::open(hashes_at))).await??
    };
    let version = handle.custody(peer, key).await?.0.unwrap_or(wire::MIN_VERSION);
    let doer = match me {
        Some(_) => crate::runner::work::peers::custodian_of(peer).await,
        None => None,
    };
    let carried = version >= wire::CUSTODY;
    let handed = super::custody::workspace_hand_on(me, doer.as_deref(), key, &digest, (!carried).then_some(peer));
    if carried {
        handle.send(peer, Ask::Custody { json: handed.frame.clone() }).await;
    }
    handle.sync_ask(peer, SyncAsk::Commit { key: key.to_owned() }).await;
    loop {
        match wait(&rx, Duration::from_secs(300)).await {
            Ok(SyncSay::Done { root, written, removed, bytes, .. }) => {
                // Taken over there as handed on, by its receipt, before anything runs in it.
                if carried {
                    let receipt = handle.custody(peer, key).await?.1;
                    if let Err(why) = super::custody::check_receipt(handle.root(), peer, receipt.as_deref(), &handed, &digest) {
                        super::custody::refused(&format!("{peer}'s copy {key}"), peer, &why);
                        anyhow::bail!("refused: the copy on {peer} isn't the tree handed on: {why}");
                    }
                }
                return Ok(Synced { root, written, removed, bytes, tree: digest });
            }
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
/// `ctl`; when it closes before the run ends, the run is stopped (unless `detach`). What it says goes on
/// `ev`. Each a lock-free queue (`crate::memory::queue`; goal 5, plan 01a1a59c2aae): every piece of a run's
/// output passes them, a `kiln run`'s and MCP `run`'s.
pub async fn run(handle: &Handle, me: &str, req: RunReq, ev: crate::memory::queue::Sender<Ev>, ctl: crate::memory::queue::Receiver<Req>) {
    run_owned(handle, me, req, ev, ctl, None).await;
}

/// [`run`], its process tracked as its agent session's own work while it runs (`owned`; attention plan
/// 01a199e1b177752e84dad5dc990e6a15: a session isn't Finished while a run of its own goes on).
pub(crate) async fn run_owned(handle: &Handle, me: &str, req: RunReq, ev: crate::memory::queue::Sender<Ev>, ctl: crate::memory::queue::Receiver<Req>, owned: Option<std::sync::Arc<super::owned::Run>>) {
    if let Err(e) = super::history::run(handle, me, req, &ev, ctl, owned.as_ref()).await {
        let _ = ev.send(Ev::Error(format!("{e:#}"))).await;
    }
}

/// The command `req` asks for, as a runner is asked to run it. A detached one asks its runner to
/// wait for its asker for ever ([`wire::DETACHED`], docs/reach.md §4.2).
fn spec_of(req: &RunReq) -> RunSpec {
    RunSpec {
        command: req.command.clone(),
        env: super::told::strip(&req.env),
        stdin: req.stdin,
        timeout_ms: req.timeout_ms,
        linger_ms: req.detach.then_some(wire::DETACHED),
        shell: req.shell.clone(),
        ..RunSpec::default()
    }
}

/// One try at `req` (`super::history::run` tries again where `[reach] retry_oom` says), its size and
/// what it used kept in `t` (docs/reach.md, "Sizes from history").
pub(super) async fn run_try(handle: &Handle, me: &str, req: RunReq, ev: &crate::memory::queue::Sender<Ev>, ctl: crate::memory::queue::Receiver<Req>, t: &super::history::Try, owned: Option<&std::sync::Arc<super::owned::Run>>) -> anyhow::Result<()> {
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
    let (peer, claim, picked, answers) = match placed {
        Some(p) => {
            let _ = ev.send(Ev::Picked { runner: p.runner.clone(), why: "its room was first".to_owned() }).await;
            (p.runner, Some((p.run, p.fence)), None, p.answers)
        }
        None => {
            let choice = t.pick(handle, me, &req).await?;
            let _ = ev.send(Ev::Picked { runner: choice.runner.name.clone(), why: choice.why.clone() }).await;
            (choice.runner.name.clone(), None, Some(choice.runner), None)
        }
    };
    if let Some(note) = super::told::note(&req) { let _ = ev.send(note).await; }
    let mut spec = spec_of(&req);
    let mut told = super::told::Watch::new(&req);
    // A detached run's news is under the line its answers opened (AUDIT O5).
    if let Some(a) = &answers {
        told.keyed(a);
    }
    if peer == me {
        // This machine suits best: no copy, no link — the folder as it is.
        spec.cwd = req.cwd.clone().or(req.remote_cwd.clone());
        let local = claim.map(|(run, _)| (handle.clone(), run));
        return run_here(handle, spec, &req, ev, ctl, t, local, told, me, owned).await;
    }
    // Told to go on `peer`, its claim held there until its start is said there: given up before
    // (a refusal, a link or a copy that failed), the claim goes (W05-R03).
    let mut owed = Owed::start(claim.as_ref().map(|(run, _)| (handle.clone(), run.clone())));
    if !elsewhere {
        taken(peer.clone()).await?;
    }
    handle.connect(&peer).await?;
    // Who hands the workspace and the run on, and receives the output back (docs/ci.md W27-R08):
    // the agent session asking, when this process holds its key, else this runner process.
    let signer = super::custody::Signer::asker(req.asker.as_deref());
    let mut tree = String::new();
    if req.sync {
        let cwd = req.cwd.clone().ok_or_else(|| anyhow::anyhow!("a sync needs the folder to copy"))?;
        let (root, rel) = crate::rt::blocking(move || sync::find_root(Path::new(&cwd))).await?;
        let key = sync::mirror_key(me, &root);
        let t = Instant::now();
        let done = sync_tree(handle, &peer, &root, &key, signer.as_ref(), ev).await?;
        let ms = u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX);
        // Reach's own record of the sync, the pair's rate and the tree's next sync's size (LB08).
        handle.transferred(&peer, &key, done.bytes, u64::try_from(t.elapsed().as_micros()).unwrap_or(u64::MAX)).await;
        let _ = ev.send(Ev::Synced { root: done.root, sent: done.written, removed: done.removed, bytes: done.bytes, ms }).await;
        tree = done.tree;
        spec.mirror = Some(key);
        spec.cwd = Some(rel.to_string_lossy().into_owned()).filter(|s| !s.is_empty());
    } else {
        spec.cwd = req.remote_cwd.clone();
    }
    // The run's hand-on goes ahead of it on a link that carries custody, and to kiln either way.
    let (id, session) = handle.reserve(&peer).await?;
    spec.id = id;
    let version = handle.custody(&peer, "").await?.0.unwrap_or(wire::MIN_VERSION);
    let carried = version >= wire::CUSTODY;
    let doer = match &signer {
        Some(_) => crate::runner::work::peers::custodian_of(&peer).await,
        None => None,
    };
    let run_digest = super::custody::run_digest(&session, &spec, &tree);
    let handed = super::custody::run_hand_on(signer.as_ref(), doer.as_deref(), &session, &spec, &tree, (!carried).then_some(peer.as_str()));
    let frames = if carried { vec![handed.frame.clone()] } else { Vec::new() };
    let (says_tx, says) = crate::memory::queue::unbounded::<Say>(&crate::memory::owners::CHANNELS);
    // Whether its exit is the peer's own (true) or one this side synthesized on losing it (false):
    // only the peer's own ends the session's ownership of it.
    let (proof, confirmed) = crate::memory::queue::unbounded::<bool>(&crate::memory::owners::CHANNELS);
    if let Some(owned) = owned {
        owned.launching().map_err(anyhow::Error::msg)?;
    }
    let proof = owned.is_some().then_some(proof);
    let id = match (&claim, &picked) {
        (Some((run, fence)), _) => {
            let id = handle.place_run(&peer, run, *fence, spec, says_tx, frames, proof).await?;
            owed.paid();
            id
        }
        (None, Some(runner)) => t.start(handle, runner, spec, says_tx, frames, proof).await?,
        (None, None) => match proof {
            Some(proof) => handle.run_observed(&peer, spec, says_tx, frames, proof).await?,
            None => handle.run_with(&peer, spec, says_tx, frames).await?,
        },
    };
    // What came back over the link of its custody: the receipt of the run, the output's hand-on,
    // and what this side got of the output, by its SHA-256.
    let (mut receipt, mut out_hand_on, mut out_got) = (None::<String>, None::<String>, String::new());
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
                    told.started();
                    let _ = ev.send(Ev::Started { runner: peer.clone(), pid, cwd }).await;
                }
                Ok(Say::Output { stream, data, .. }) => {
                    told.output(&data);
                    let _ = ev.send(Ev::Output { err: stream == Stream::Err, data }).await;
                }
                Ok(Say::Used { used, .. }) => t.used(&used),
                Ok(Say::Custody { json }) => {
                    let v: serde_json::Value = serde_json::from_str(&json).unwrap_or_default();
                    if v.get("run").is_some() {
                        receipt = Some(json);
                    } else if v.get("output").is_some() {
                        out_hand_on = Some(json);
                    } else if let Some(sha) = v.get("sha256").and_then(serde_json::Value::as_str) {
                        sha.clone_into(&mut out_got);
                    }
                }
                Ok(Say::Exit { code, mut error, lost, .. }) => {
                    if let Some(owned) = owned
                        && confirmed.recv().await.unwrap_or(false)
                    {
                        owned.confirm_exit();
                    }
                    // A run it did, on a link that carries custody: taken as handed on, and its
                    // output received as it was handed back, or refused, and recorded.
                    if carried && error.is_none() && handed.hand_on.is_some() {
                        let checked = super::custody::check_receipt(handle.root(), &peer, receipt.as_deref(), &handed, &run_digest)
                            .and_then(|()| super::custody::take_output(signer.as_ref(), handle.root(), &peer, id, out_hand_on.as_deref(), &out_got));
                        if let Err(why) = checked {
                            super::custody::refused(&format!("run {id}'s output"), &peer, &why);
                            error = Some(format!("refused: {why}"));
                        }
                    }
                    // Its client saw it end: what its answers told its agent is closed.
                    told.seen();
                    return t.exit(ev, &req, stop_by.is_some(), code, error, lost).await;
                }
                Ok(_) => {}
                Err(_) => anyhow::bail!("lost track of the run"),
            },
            c = ctl_next.fuse() => match c {
                Ok(Req::Input(data)) => { handle.send(&peer, Ask::Stdin { id, data }).await; }
                Ok(Req::InputEnd) => { handle.send(&peer, Ask::StdinEnd { id }).await; }
                Ok(Req::Signal(s)) => { handle.send(&peer, Ask::Signal { id, signal: s }).await; }
                Ok(_) => {}
                // It keeps running over there, followed on alone: its output taken and let go, its
                // link made again after a drop, until it ends (below).
                Err(_) if req.detach => break,
                Err(_) => {
                    super::told::gone(req.client);
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
    // Detached, its client gone: the agent that asked for it is told how it ends (docs/reach.md §7).
    super::told::detached(handle, &peer, id, told, says).await
}

/// What a placed run's claim is owed should its driver leave before saying it (W05-R03): that its
/// start was never said at its runner ([`Handle::place_unsaid`]), or its end here
/// ([`Handle::place_local`]). Said as the driver leaves (an error, its client gone, its future
/// dropped) unless it was [`Owed::paid`]: unsaid, the claim stays held for good, and every run
/// asking room there waits behind it.
struct Owed(Option<(Handle, String, bool)>);

impl Owed {
    /// Its start's owed until it's said at `run`'s runner.
    fn start(placed: Option<(Handle, String)>) -> Owed {
        Owed(placed.map(|(h, run)| (h, run, false)))
    }

    /// Its end here's owed until it's said.
    fn end(placed: Option<(Handle, String)>) -> Owed {
        Owed(placed.map(|(h, run)| (h, run, true)))
    }

    /// Said: what follows the run says the rest.
    fn paid(&mut self) {
        self.0 = None;
    }
}

impl Drop for Owed {
    fn drop(&mut self) {
        if let Some((h, run, end)) = self.0.take() {
            crate::rt::spawn(async move {
                if end {
                    h.place_local(&run, Some(false)).await;
                } else {
                    h.place_unsaid(&run).await;
                }
            });
        }
    }
}

/// A run placement gave a runner: which, and its claim there; and its answers' teller, when its
/// agent is told them (AUDIT O5).
struct Placing {
    runner: String,
    run: String,
    fence: kiln_ci::sched::Fence,
    answers: Option<super::told::Answers>,
}

/// Places a run (docs/needs.md E5) when its target is what it needs (no runner named) and this
/// node's placement is ready: its asker is told each answer as it comes, at once and on every
/// change, until it's to start somewhere (the runner's room first), and after, as it starts and
/// ends; and the agent session that asked, once it's queued, through told (AUDIT O5,
/// [`super::told::Answers`]): pushed both ways, never polled, and no answer says to ask again.
/// `None`: placed as before (a runner named, placement not ready, or only older runners could take
/// it).
async fn place(handle: &Handle, me: &str, req: &RunReq, ev: &crate::memory::queue::Sender<Ev>) -> anyhow::Result<Option<Placing>> {
    use super::place::Placed;
    let Ok(needs) = kiln_ci::sched::needs::parse(&req.target) else { return Ok(None) };
    let run = format!("{me}-{}", crate::util::new_id());
    let repo = req.cwd.as_deref().and_then(|c| Path::new(c).file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    // A person at a terminal, or an agent's turn, waits on it (docs/needs.md §7).
    // Its kind of work, as its runner's health bars it (AUDIT L2): its command in its folder here.
    let key = req.cwd.as_deref().map(|c| crate::sense::run::command_key(&req.command, Path::new(c)));
    let ask = kiln_ci::place::Ask { run: run.clone(), needs, class: kiln_ci::sched::Class::Interactive, repo, actor: req.asker.clone().unwrap_or_else(|| "person".to_owned()), timeout: req.timeout_ms, key, ..kiln_ci::place::Ask::default() };
    let (tx, rx) = crate::memory::queue::unbounded::<Placed>(&crate::memory::owners::CHANNELS);
    let answers = super::told::Answers::of(req, &run);
    if !handle.place(ask, tx, answers.clone()).await? {
        return Ok(None);
    }
    let tell = |a: &kiln_ci::place::Answer| Ev::Placed { json: serde_json::to_string(a).unwrap_or_default() };
    loop {
        match rx.recv().await {
            Ok(Placed::Answer(a)) => {
                if let Some(t) = &answers {
                    t.heard(&a, crate::util::now_ms());
                }
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
                // What it hears after (started, ended) goes on to its asker, and its start by its
                // runner's word to the agent that asked (AUDIT O5), even once its client went.
                let (ev, told) = (ev.clone(), answers.clone());
                crate::rt::spawn(async move {
                    let mut asker = true;
                    while let Ok(p) = rx.recv().await {
                        if let Placed::Answer(a) = p {
                            if let Some(t) = &told {
                                t.heard(&a, crate::util::now_ms());
                            }
                            asker = asker && ev.send(tell(&a)).await.is_ok();
                            if !asker && told.is_none() {
                                return;
                            }
                        }
                    }
                });
                return Ok(Some(Placing { runner, run, fence, answers }));
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
/// What's told of it for its agent session (`told`, docs/reach.md §7) follows it as it goes, and
/// on once its client went.
// Each of the three streams merged into it brings what it follows a run with (history's `t`,
// placement's `placed`, told's `told`): kept apart, as their owners keep them.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_here(handle: &Handle, spec: RunSpec, req: &RunReq, ev: &crate::memory::queue::Sender<Ev>, ctl: crate::memory::queue::Receiver<Req>, t: &super::history::Try, placed: Option<(Handle, String)>, mut told: super::told::Watch, me: &str, owned: Option<&std::sync::Arc<super::owned::Run>>) -> anyhow::Result<()> {
    // Started here by placement, its room held until its end is said: left before (its folder or
    // its launch refused, its process lost, its client gone and its process stopped), it's said
    // then (W05-R03).
    let mut unended = Owed::end(placed.clone());
    let cwd = super::serve::resolve_cwd(&spec, None)?;
    // Its kind of work as this runner keys it (sense's), for its asker's key (AUDIT L2).
    let here_key = crate::sense::run::command_key(&spec.command, &cwd);
    let (tx, rx) = crate::memory::queue::places::<super::serve::Event>(&crate::memory::owners::CHANNELS, 16);
    let shell = spec.shell.as_deref().map(|s| crate::os::home_dir().join(".kiln").join("reach").join("shells").join(s));
    if let Some(owned)=owned{owned.launching().map_err(anyhow::Error::msg)?;}
    let stdin = super::serve::start(spec, cwd, shell, Vec::new(), tx);
    let mut pid = None;
    loop {
        use futures::FutureExt as _;
        futures::select_biased! {
            e = rx.recv().fuse() => match e {
                Ok(super::serve::Event::Started { pid: p, born, cwd }) => {
                    pid = Some(p);
                    if let Some((h, run)) = &placed {
                        h.place_local_up(run, p, born, here_key.clone()).await;
                    }
                    told.started();
                    let _ = ev.send(Ev::Started { runner: String::new(), pid: p, cwd }).await;
                }
                Ok(super::serve::Event::Output { stream, data }) => { told.output(&data); let _ = ev.send(Ev::Output { err: stream == Stream::Err, data }).await; }
                Ok(super::serve::Event::Exit { code, error, used }) => {
                    if let Some(owned) = owned { owned.confirm_exit(); }
                    if let Some((h, run)) = &placed {
                        h.place_local(run, Some(error.is_none())).await;
                    }
                    unended.paid();
                    told.seen();
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
                    super::told::gone(told.client());
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
    // What follows it once its client went: its room freed and what it used kept as it ends
    // (placement, history), every event on to what tells its agent of it (`told`).
    let (fwd, follow) = crate::memory::queue::places::<super::serve::Event>(&crate::memory::owners::CHANNELS, 16);
    crate::rt::spawn(super::told::follow_here(follow, told, me.to_owned()));
    let (t, handle) = (t.clone(), handle.clone());
    // Its end's owed by what follows it from here: said as it ends, or as this loses it.
    let mut unended = unended;
    crate::rt::spawn(async move {
        while let Ok(e) = rx.recv().await {
            let ended = if let super::serve::Event::Exit { error, used, .. } = &e {
                if let Some((h, run)) = &placed {
                    h.place_local(run, Some(error.is_none())).await;
                }
                unended.paid();
                t.record(&handle, used.as_deref());
                true
            } else {
                false
            };
            let _ = fwd.send(e).await;
            if ended {
                return;
            }
        }
    });
    Ok(())
}

/// Serve the socket at `path` until this process exits. Unix only.
#[cfg(unix)]
pub fn serve(handle: Handle, me: String, path: PathBuf, take: bool, host: Option<std::sync::Arc<crate::harness::host::Host>>) -> anyhow::Result<PathBuf> {
    use std::os::fd::{AsRawFd as _, OwnedFd};

    // Kept as `kiln spawn`'s is (crate::harness::socket::Kept): `take` when a new build takes the
    // machine over, else served once nothing else serves it, and served again if left dead.
    let mut kept = crate::harness::socket::Kept::new(path.clone(), take, "`kiln run`")?;
    crate::rt::spawn(async move {
        loop {
            let Some(listener) = kept.listener() else {
                // Another process serves it: looked at when its look is due (`Kept::due`; goal 9: it
                // looked each second), served once it's gone. (c) a path and a process kiln has no
                // word from, as `kiln spawn`'s socket's (crate::harness::socket::serve_with).
                crate::rt::sleep_until(kept.due()).await;
                kept.check();
                continue;
            };
            let fd = listener.as_raw_fd();
            match listener.accept() {
                Ok((stream, _)) => {
                    if stream.set_nonblocking(true).is_err() {
                        continue;
                    }
                    let (handle, me,host) = (handle.clone(), me.clone(),host.clone());
                    crate::rt::spawn(async move { connection(handle, me, std::sync::Arc::new(OwnedFd::from(stream)),host).await });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // Woken when a connection waits, on no clock for a wakeup missed (goal 9: it woke
                    // each second regardless): the driver arms the listener's interest as the wait is
                    // first polled, after this accept, and fires at once for a connection already
                    // waiting. Quiet, it looks at its path when that's due (`Kept::due`), as `kiln
                    // spawn`'s socket does.
                    let (ready, look) = (hyper_rt::readiness::readable(fd), crate::rt::sleep_until(kept.due()));
                    if let futures::future::Either::Left((Err(e), _)) = futures::future::select(std::pin::pin!(ready), std::pin::pin!(look)).await {
                        // (b) the driver refused the wait: asked again after a picked backoff.
                        tracing::debug!(error = %e, "reach socket's wait refused");
                        crate::rt::sleep_until(Instant::now() + Duration::from_millis(200)).await;
                    }
                    kept.check();
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    tracing::warn!(error = %e, "reach socket accept failed");
                    // (b) asked again after a picked backoff.
                    crate::rt::sleep_until(Instant::now() + Duration::from_millis(500)).await;
                }
            }
        }
    });
    Ok(path)
}

#[cfg(not(unix))]
pub fn serve(_handle: Handle, _me: String, _path: PathBuf, _take: bool,_host:Option<std::sync::Arc<crate::harness::host::Host>>) -> anyhow::Result<PathBuf> {
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
async fn connection(handle: Handle, me: String, fd: std::sync::Arc<std::os::fd::OwnedFd>, host: Option<std::sync::Arc<crate::harness::host::Host>>) {
    // What its client asks and what it's told, on lock-free queues, unbounded as the channels they were.
    let (req_tx, req_rx) = crate::memory::queue::unbounded::<Req>(&crate::memory::owners::CHANNELS);
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
    let (ev_tx, ev_rx) = crate::memory::queue::unbounded::<Ev>(&crate::memory::owners::CHANNELS);
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
    let Ok(first) = crate::rt::timeout(Duration::from_secs(10), req_rx.recv()).await.unwrap_or(Err(async_channel::RecvError)) else { return };
    answer(handle, me, first, req_rx, ev_tx, host, Peer::Process(peer_pid(&fd))).await;
    let _ = out.await;
}

/// Who a request comes from, as what carried it says: a process of this machine at the other end
/// of the reach socket (where the system says which), or an isolated session's own lane into this
/// process (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d step 18, Shards D254), which names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Peer {
    /// A process of this machine, by its pid where the system says.
    #[cfg(unix)]
    Process(Option<u32>),
    /// An agent session whose harness runs in a microVM of its own, asking over its lane.
    Session(String),
}

/// A request answered on `ev_tx`, whatever carried it ([`Peer`]): what follows it on `req_rx` is a
/// run's input and signals. Every platform has it; only Unix has the socket that carries it from a
/// process ([`serve`]).
pub(super) async fn answer(handle: Handle, me: String, mut first: Req, req_rx: crate::memory::queue::Receiver<Req>, ev_tx: crate::memory::queue::Sender<Ev>, host: Option<std::sync::Arc<crate::harness::host::Host>>, peer: Peer) {
    // A run's asker: the agent session its client belongs to, if any; a lane's is its session.
    if let Req::Run(r) | Req::RunReuse(r) = &mut first {
        match &peer {
            #[cfg(unix)]
            Peer::Process(pid) => {
                let pid = *pid;
                r.client = pid;
                r.asker = crate::rt::blocking(move || pid.and_then(crate::runner::custody::session_of_pid)).await.ok().flatten();
            }
            Peer::Session(session) => {
                r.client = None;
                r.asker = Some(session.clone());
            }
        }
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
        Req::Run(r) => owned_run(host, &handle, &me, *r, ev_tx.clone(), req_rx, false).await,
        Req::RunReuse(r) => owned_run(host, &handle, &me, *r, ev_tx.clone(), req_rx, true).await,
        Req::Told(json) => {
            let _ = ev_tx.send(super::told::heard(&json, &peer).await).await;
        }
        Req::Input(_) | Req::InputEnd | Req::Signal(_) => {
            let _ = ev_tx.send(Ev::Error("nothing is running on this connection".into())).await;
        }
    }
}

/// A run asked of this runner, as its owned launches run it (`super::owned::run`).
#[cfg(unix)]
async fn owned_run(host: Option<std::sync::Arc<crate::harness::host::Host>>, handle: &Handle, me: &str, req: RunReq, ev: crate::memory::queue::Sender<Ev>, ctl: crate::memory::queue::Receiver<Req>, reuse: bool) {
    super::owned::run(host, handle, me, req, ev, ctl, reuse).await;
}

/// Where this runner's owned launches don't run (W05-R07: only Unix's admit them), a run is refused.
#[cfg(not(unix))]
async fn owned_run(_host: Option<std::sync::Arc<crate::harness::host::Host>>, _handle: &Handle, _me: &str, _req: RunReq, ev: crate::memory::queue::Sender<Ev>, _ctl: crate::memory::queue::Receiver<Req>, _reuse: bool) {
    let _ = ev.send(Ev::Error(OWNED_UNIX.into())).await;
}

/// Why a run can't be asked of this runner where its owned launches don't run.
#[cfg(not(unix))]
const OWNED_UNIX: &str = "a run from an agent's MCP needs this runner's own launches, which only a Unix runner holds today (docs/ci.md W05-R07)";

/// This runner asked in its own process, as its reach socket is asked, with no socket: an isolated
/// session's MCP over its lane (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d step 18, Shards D254), on
/// every platform. Each [`Here::ask`] is one request, as a connection to the socket is.
#[derive(Clone)]
pub struct Here {
    pub handle: Handle,
    /// This runner's name.
    pub me: String,
    pub host: Option<std::sync::Arc<crate::harness::host::Host>>,
    /// The isolated session asking: a run's asker, and whom its runs' news is for.
    pub session: String,
}

impl Here {
    /// One request's connection.
    pub fn ask(&self) -> Asked {
        let (req_tx, req_rx) = crate::memory::queue::unbounded::<Req>(&crate::memory::owners::CHANNELS);
        let (ev_tx, ev_rx) = crate::memory::queue::unbounded::<Ev>(&crate::memory::owners::CHANNELS);
        Asked { here: self.clone(), req_tx, waiting: Some((req_rx, ev_tx)), ev_rx }
    }
}

/// One request of [`Here`]'s: the first sent is answered, as the socket answers it ([`answer`]),
/// and what's sent after it is a run's input and signals. For a thread of its own, as a socket's
/// client is: its sends and reads wait.
pub struct Asked {
    here: Here,
    req_tx: crate::memory::queue::Sender<Req>,
    /// Until the first request: what its answer takes.
    waiting: Option<(crate::memory::queue::Receiver<Req>, crate::memory::queue::Sender<Ev>)>,
    ev_rx: crate::memory::queue::Receiver<Ev>,
}

impl Asked {
    pub fn send(&mut self, r: &Req) -> anyhow::Result<()> {
        match self.waiting.take() {
            Some((req_rx, ev_tx)) => {
                let h = self.here.clone();
                let first = r.clone();
                crate::rt::spawn(async move { answer(h.handle, h.me, first, req_rx, ev_tx, h.host, Peer::Session(h.session)).await });
                Ok(())
            }
            None => self.req_tx.send_blocking(r.clone()).map_err(|_| anyhow::anyhow!("the runner has answered this request: nothing more goes on it")),
        }
    }

    /// The next event; None once its answer is done.
    pub fn event(&mut self) -> anyhow::Result<Option<Ev>> {
        Ok(self.ev_rx.recv_blocking().ok())
    }

    /// A second handle on it, to send a run's input or signals while another thread reads.
    pub fn sender(&self) -> crate::memory::queue::Sender<Req> {
        self.req_tx.clone()
    }
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
    /// runner's: 1,532 left by ended test and lab runners kept `kiln reach mcp` from finding it. The
    /// ended one is left as `crate::harness::socket::leave_ended` leaves it (one bound to listen and
    /// closed stays live in a program another test starts meanwhile).
    #[test]
    fn the_runner_s_socket_is_found_among_ended_ones_and_others() -> anyhow::Result<()> {
        use std::os::unix::net::UnixListener;
        let dir = tempfile::tempdir()?;
        let path = |n: &str| dir.path().join(n);
        crate::harness::socket::leave_ended(&path("reach-lab-0-deadbeef.sock"))?;
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

    /// A socket at a runner's path its probe couldn't tell of (`crate::harness::socket::Probe::Unknown`:
    /// here a live one of another kind, as a probe that can't be made on a loaded host) is counted,
    /// never dropped as an ended runner's: the only one, it's chosen, and whoever asks hears what
    /// reaching it says, not that no runner runs here.
    #[test]
    fn a_runner_s_socket_its_probe_couldnt_tell_of_is_still_chosen() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let untold = dir.path().join("reach-adacachy-4b48ed43.sock");
        let _there = std::os::unix::net::UnixDatagram::bind(&untold)?;
        anyhow::ensure!(choose_socket(vec![untold.clone()], "00000000")? == untold, "a socket its probe couldn't tell of was dropped as an ended runner's");
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

    /// Goal 9 (loops wake on what they wait for): the loop serving reach's socket waits on a
    /// connection, and on its path's look when that's due (`Kept::due`), as `kiln spawn`'s does; on
    /// no timeout around its wait and no sleep of its own (it woke every second regardless, for a
    /// wakeup missed: none is, `crate::harness::socket`'s
    /// g9_s5_a_connection_before_the_wait_is_armed_is_not_missed). Its connections, end to end:
    /// `super::super::tests::doors`. Expect: neither in it, and its look waited for.
    #[test]
    fn g9_the_sockets_loop_waits_on_a_connection_or_its_look_and_on_no_clock_of_its_own() {
        let code = include_str!("local.rs");
        let serving = code.split("pub fn serve(").nth(1).and_then(|rest| rest.split("#[cfg(not(unix))]").next()).unwrap_or_default();
        assert!(!serving.is_empty(), "the socket's loop wasn't found");
        for clock in ["rt::timeout(", "rt::sleep("] {
            assert!(!serving.contains(clock), "the socket's loop waits on a clock of its own: {clock}");
        }
        assert!(serving.contains("sleep_until(kept.due())"), "the socket's loop doesn't wait for its path's look");
    }
}

