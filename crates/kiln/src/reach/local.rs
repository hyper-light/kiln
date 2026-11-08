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
    if let Err(e) = run_inner(handle, me, req, &ev, ctl, owned.as_ref()).await {
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

async fn run_inner(handle: &Handle, me: &str, req: RunReq, ev: &crate::memory::queue::Sender<Ev>, ctl: crate::memory::queue::Receiver<Req>, owned: Option<&std::sync::Arc<super::owned::Run>>) -> anyhow::Result<()> {
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
    let choice = pick(handle, me, &req.target).await?;
    let peer = choice.runner.name.clone();
    let _ = ev.send(Ev::Picked { runner: peer.clone(), why: choice.why.clone() }).await;
    let mut spec = spec_of(&req);
    if peer == me {
        // This machine suits best: no copy, no link — the folder as it is.
        spec.cwd = req.cwd.clone().or(req.remote_cwd.clone());
        return run_here(spec, req.detach, ev, ctl,owned).await;
    }
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
    let (proof, confirmed) = async_channel::unbounded();
    if let Some(owned) = owned {
        owned.launching().map_err(anyhow::Error::msg)?;
    }
    let id = if owned.is_some() { handle.run_observed(&peer, spec, says_tx, frames, proof).await? } else { handle.run_with(&peer, spec, says_tx, frames).await? };
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
                    let _ = ev.send(Ev::Started { runner: peer.clone(), pid, cwd }).await;
                }
                Ok(Say::Output { stream, data, .. }) => {
                    let _ = ev.send(Ev::Output { err: stream == Stream::Err, data }).await;
                }
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
                    let _ = ev.send(Ev::Exit { code, error, lost }).await;
                    return Ok(());
                }
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

/// Run on this machine (it suited best): the same events, no link. A detached run goes on after
/// its client, its events read to its end so that its output never makes it wait.
pub(crate) async fn run_here(spec: RunSpec, detach: bool, ev: &crate::memory::queue::Sender<Ev>, ctl: crate::memory::queue::Receiver<Req>, owned: Option<&std::sync::Arc<super::owned::Run>>) -> anyhow::Result<()> {
    let cwd = super::serve::resolve_cwd(&spec, None)?;
    let (tx, rx) = crate::memory::queue::places::<super::serve::Event>(&crate::memory::owners::CHANNELS, 16);
    let shell = spec.shell.as_deref().map(|s| crate::os::home_dir().join(".kiln").join("reach").join("shells").join(s));
    if let Some(owned)=owned{owned.launching().map_err(anyhow::Error::msg)?;}
    let stdin = super::serve::start(spec, cwd, shell, Vec::new(), tx);
    let mut pid = None;
    loop {
        use futures::FutureExt as _;
        futures::select_biased! {
            e = rx.recv().fuse() => match e {
                Ok(super::serve::Event::Started { pid: p, cwd }) => {
                    pid = Some(p);
                    let _ = ev.send(Ev::Started { runner: String::new(), pid: p, cwd }).await;
                }
                Ok(super::serve::Event::Output { stream, data }) => { let _ = ev.send(Ev::Output { err: stream == Stream::Err, data }).await; }
                Ok(super::serve::Event::Exit { code, error }) => {
                    if let Some(owned)=owned{owned.confirm_exit();}
                    let _ = ev.send(Ev::Exit { code, error, lost: 0 }).await;
                    return Ok(());
                }
                Err(_) => anyhow::bail!("lost track of the run"),
            },
            c = ctl.recv().fuse() => match c {
                Ok(Req::Input(d)) => { let _ = stdin.send(d).await; }
                Ok(Req::InputEnd) => { let _ = stdin.send(Vec::new()).await; }
                Ok(Req::Signal(s)) => if let Some(p) = pid { super::serve::signal_group(p, s); },
                Ok(_) => {}
                Err(_) if detach => break,
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
    crate::rt::spawn(async move {
        while let Ok(e) = rx.recv().await {
            if matches!(e, super::serve::Event::Exit { .. }) {
                return;
            }
        }
    });
    Ok(())
}

/// Serve the socket at `path` until this process exits. Unix only.
#[cfg(unix)]
#[allow(clippy::disallowed_methods)] // (b) bind and accept retried, picked
pub fn serve(handle: Handle, me: String, path: PathBuf, take: bool, host: Option<std::sync::Arc<crate::harness::host::Host>>) -> anyhow::Result<PathBuf> {
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
                    let (handle, me,host) = (handle.clone(), me.clone(),host.clone());
                    crate::rt::spawn(async move { connection(handle, me, std::sync::Arc::new(OwnedFd::from(stream)),host).await });
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
            let ev = match handle.query(json).await {
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
        Req::Run(r) => super::owned::run(host,&handle, &me, *r, ev_tx.clone(), req_rx,false).await,
        Req::RunReuse(r) => super::owned::run(host,&handle, &me, *r, ev_tx.clone(), req_rx,true).await,
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

