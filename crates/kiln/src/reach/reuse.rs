//! `kiln run --reuse` (docs/ci.md W24-R24): a command that opts in gives back the result recorded
//! for an identical run instead of running again.
//!
//! Identical means the same command, in the same folder of the tree, with the same environment
//! given to it (`-e`), on the same machine, over an identical tree: what sync sends (tracked files
//! and the untracked ones git doesn't ignore, `.git` aside), by content. The decision is the work
//! ledger's (`crate::ci::work`, W24-R30): a run is a unit of kind `run` keyed by
//! `kiln_ci::work::key`, its tree's digest one of its inputs and the machine its platform (its
//! toolchains are whatever that machine has). Only a success is recorded (W24-R07), its output
//! kept in this runner's content store, and a hit replays that output and exits 0: nothing runs.
//!
//! A run that can't be keyed runs as `kiln run` does, saying why: one that reads input (`-i`), runs
//! in a named shell (whose state carries between runs), or has no tree (`--no-sync` elsewhere,
//! outside a git repository). Results are the machine user's own (the reach socket is theirs
//! alone): kept under `kiln-run|person:<user>|`, never trusted (W24-R21).

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

use kiln_ci::work::key::{self, Key, Kind, PathSet, Platform, Unit};
use serde_json::json;

use super::directory::RunnerInfo;
use super::local::{Ev, Req, RunReq};
use super::node::Handle;
use super::wire::EntryKind;
use crate::ci::work::{Provenance, Recorded};
use crate::runner::work::service::Service;
use crate::runner::work::store::Scope;

/// The ledger's repository for `kiln run`'s results.
pub const REPO: &str = "kiln-run";

/// The most output a recorded run keeps: one that says more runs again next time.
pub const MAX_LOG: usize = 16 << 20;

/// This machine's user, as the trust domain its runs' results are kept in.
pub fn person() -> String {
    let user = ["USER", "LOGNAME", "USERNAME"].iter().find_map(|k| std::env::var(k).ok().filter(|u| !u.trim().is_empty()));
    format!("person:{}", user.unwrap_or_else(|| format!("uid-{}", crate::os::uid())))
}

/// Why `req` can't be keyed, when it can't (before its tree is looked at).
pub fn unkeyable(req: &RunReq) -> Option<&'static str> {
    if req.stdin {
        return Some("it reads input, which no key holds");
    }
    if req.shell.is_some() {
        return Some("it runs in a named shell, whose folder and variables carry from run to run");
    }
    if req.detach {
        return Some("it's detached, so nothing here sees how it ends");
    }
    None
}

/// The digest of the tree at `root` as sync sends it: every entry by path and kind, a file by its
/// content and whether it's executable, a symlink by its target (`.git` aside).
pub fn tree(root: &Path) -> anyhow::Result<String> {
    use sha2::Digest as _;
    let listed = super::sync::scan(root)?;
    let mut h = sha2::Sha256::new();
    h.update(b"kiln/run-tree/v1\0");
    let mut field = |s: &[u8]| {
        h.update((s.len() as u64).to_be_bytes());
        h.update(s);
    };
    for e in &listed.entries {
        if e.path == ".git" || e.path.starts_with(".git/") {
            continue;
        }
        field(e.path.as_bytes());
        match e.kind {
            EntryKind::Dir => field(b"d"),
            EntryKind::Symlink => {
                field(b"l");
                field(e.target.as_deref().unwrap_or_default().as_bytes());
            }
            EntryKind::File => {
                field(if e.mode & 0o111 != 0 { b"x" } else { b"f" });
                field(crate::sync::blobs::hash_file(&listed.source(&e.path))?.as_bytes());
            }
        }
    }
    Ok(hex::encode(h.finalize()))
}

/// The run as a unit of work (W24-R01): its command and folder in the tree, the environment given
/// it, the machine it runs on, the tree, and whose it is.
pub fn unit(req: &RunReq, rel: &str, tree: &str, runner: &RunnerInfo, person: &str) -> Unit {
    let env: BTreeMap<String, String> = req.env.iter().cloned().collect();
    Unit {
        kind: Kind::Run,
        definition: json!({ "command": req.command, "cwd": rel }).to_string(),
        secrets: Default::default(),
        env,
        platform: Platform { os: runner.os.clone(), arch: runner.arch.clone(), executor: "reach".to_owned(), toolchains: BTreeMap::from([("runner".to_owned(), runner.name.clone())]), ..Platform::default() },
        inputs: BTreeMap::from([("tree".to_owned(), tree.to_owned())]),
        upstream: BTreeMap::new(),
        trust: person.to_owned(),
    }
}

/// A run's output, as it's kept: each piece its stream (0 out, 1 err), its length, its bytes.
pub fn encode(pieces: &[(bool, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(pieces.iter().map(|(_, d)| d.len() + 5).sum());
    for (err, data) in pieces {
        out.push(u8::from(*err));
        out.extend_from_slice(&u32::try_from(data.len()).unwrap_or(u32::MAX).to_be_bytes());
        out.extend_from_slice(data);
    }
    out
}

/// Kept output read back; `None` when it doesn't read.
pub fn decode(mut b: &[u8]) -> Option<Vec<(bool, Vec<u8>)>> {
    let mut out = Vec::new();
    while let Some((&stream, rest)) = b.split_first() {
        let (len, rest) = rest.split_first_chunk::<4>()?;
        let n = usize::try_from(u32::from_be_bytes(*len)).ok()?;
        let data = rest.get(..n)?;
        out.push((stream == 1, data.to_vec()));
        b = rest.get(n..)?;
    }
    Some(out)
}

/// A run keyed: where it runs, its ledger scope and its weak key.
#[derive(Debug, Clone)]
pub struct Keyed {
    pub runner: RunnerInfo,
    pub why: String,
    pub scope: String,
    pub weak: Key,
}

/// `req` keyed, or why it can't be.
async fn key_for(handle: &Handle, me: &str, req: &RunReq) -> anyhow::Result<Result<Keyed, String>> {
    if let Some(why) = unkeyable(req) {
        return Ok(Err(why.to_owned()));
    }
    let choice = super::local::pick(handle, me, &req.target).await?;
    let here = choice.runner.name == me;
    let folder = if here { req.cwd.clone().or_else(|| req.remote_cwd.clone()) } else { req.cwd.clone().filter(|_| req.sync) };
    let Some(folder) = folder else { return Ok(Err("it runs elsewhere without its tree (--no-sync)".to_owned())) };
    let (root, rel) = crate::rt::blocking(move || super::sync::find_root(Path::new(&folder))).await?;
    if !root.join(".git").exists() {
        return Ok(Err(format!("{} isn't in a git repository, so its tree isn't known", root.display())));
    }
    let digest = crate::rt::blocking(move || tree(&root)).await??;
    let person = person();
    let u = unit(req, &rel.to_string_lossy(), &digest, &choice.runner, &person);
    Ok(Ok(Keyed { runner: choice.runner, why: choice.why, scope: crate::ci::work::scope(REPO, &person), weak: key::weak(&u) }))
}

/// The result recorded for `weak` in `scope`, with its output; `None` without one, or when its
/// output is no longer kept.
pub async fn lookup(svc: &Service, scope: &str, weak: Key) -> anyhow::Result<Option<(Recorded, Vec<(bool, Vec<u8>)>)>> {
    let sc = scope.to_owned();
    let found = svc.store().ledger().call(move |c| crate::ci::work::lookup(c, &sc, &weak, &|p: &PathSet| p.clone())).await?;
    let Some((_, recorded)) = found else { return Ok(None) };
    let Some(log) = recorded.logs.clone() else { return Ok(None) };
    let store = std::sync::Arc::clone(svc.store());
    let bytes = crate::rt::blocking(move || store.read(&log)).await??;
    Ok(bytes.and_then(|b| decode(&b)).map(|pieces| (recorded, pieces)))
}

/// Records a success: its output kept in the content store, its result in the ledger.
pub async fn record(svc: &Service, k: &Keyed, pieces: &[(bool, Vec<u8>)], ms: u64) -> anyhow::Result<()> {
    let log = encode(pieces);
    let store = std::sync::Arc::clone(svc.store());
    let (digest, _) = crate::rt::blocking(move || store.put_bytes(&log)).await??;
    let person = person();
    svc.store().hold(&Scope::new(REPO, &person), &digest)?;
    let result = Recorded {
        outcome: "success".to_owned(),
        exit: Some(0),
        logs: Some(digest),
        provenance: Provenance { repo: REPO.to_owned(), runner: k.runner.name.clone(), ms, finished_at: crate::util::now_unix(), ..Provenance::default() },
        domain: person,
        ..Recorded::default()
    };
    let (scope, weak) = (k.scope.clone(), k.weak);
    svc.store().ledger().call(move |c| crate::ci::work::record(c, &scope, &weak, &PathSet::new(), &result).map(|_| ())).await
}

fn ago(secs: i64) -> String {
    match secs.max(0) {
        s if s < 90 => format!("{s} s ago"),
        s if s < 90 * 60 => format!("{} min ago", s / 60),
        s if s < 36 * 3600 => format!("{} h ago", s / 3600),
        s => format!("{} days ago", s / 86_400),
    }
}

/// What a run said, kept as it goes by: whether it can be recorded once it ends.
#[derive(Default)]
struct Kept {
    pieces: Vec<(bool, Vec<u8>)>,
    bytes: usize,
    over: bool,
}

impl Kept {
    fn see(&mut self, err: bool, data: &[u8]) {
        self.bytes = self.bytes.saturating_add(data.len());
        if self.bytes > MAX_LOG {
            self.over = true;
            self.pieces.clear();
        } else if !self.over {
            self.pieces.push((err, data.to_vec()));
        }
    }
}

/// `kiln run --reuse`, start to end: keyed and looked up first; a hit replayed, a miss run as
/// [`super::local::run`] runs it and recorded if it succeeds.
pub async fn run(handle: &Handle, me: &str, req: RunReq, ev: async_channel::Sender<Ev>, ctl: async_channel::Receiver<Req>) {
    run_with(crate::runner::work::service::ensure().await, handle, me, req, ev, ctl).await;
}

/// [`run`] against `svc`, this runner's cache service.
pub async fn run_with(svc: Option<&std::sync::Arc<Service>>, handle: &Handle, me: &str, mut req: RunReq, ev: async_channel::Sender<Ev>, ctl: async_channel::Receiver<Req>) {
    let Some(svc) = svc else {
        let _ = ev.send(Ev::Note("not reused: this runner serves no cache".to_owned())).await;
        return super::local::run(handle, me, req, ev, ctl).await;
    };
    let keyed = match key_for(handle, me, &req).await {
        Ok(Ok(k)) => k,
        Ok(Err(why)) => {
            let _ = ev.send(Ev::Note(format!("not reused: {why}"))).await;
            return super::local::run(handle, me, req, ev, ctl).await;
        }
        Err(e) => {
            let _ = ev.send(Ev::Error(format!("{e:#}"))).await;
            return;
        }
    };
    match lookup(svc, &keyed.scope, keyed.weak).await {
        Ok(Some((found, pieces))) => {
            let _ = ev.send(Ev::Picked { runner: keyed.runner.name.clone(), why: keyed.why.clone() }).await;
            let took = found.provenance.ms as f64 / 1000.0;
            let note = format!("reused: an identical run on {} {} (it took {took:.1}s); nothing ran", found.provenance.runner, ago(crate::util::now_unix() - found.provenance.finished_at));
            let _ = ev.send(Ev::Note(note)).await;
            for (err, data) in pieces {
                let _ = ev.send(Ev::Output { err, data }).await;
            }
            let _ = ev.send(Ev::Exit { code: Some(0), error: None, lost: 0 }).await;
            return;
        }
        Ok(None) => {}
        Err(e) => {
            let _ = ev.send(Ev::Note(format!("the work ledger couldn't be read ({e:#}); it runs"))).await;
        }
    }
    // Where it was keyed for, by name: the run goes there.
    req.target = keyed.runner.name.clone();
    let (tee, seen) = async_channel::unbounded::<Ev>();
    let started = Instant::now();
    let running = super::local::run(handle, me, req, tee, ctl);
    let forwarding = async {
        let mut kept = Kept::default();
        while let Ok(e) = seen.recv().await {
            match &e {
                Ev::Output { err, data } => kept.see(*err, data),
                Ev::Exit { code: Some(0), error: None, lost: 0 } if !kept.over => {
                    let ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                    let note = match record(svc, &keyed, &kept.pieces, ms).await {
                        Ok(()) => "recorded: an identical run gives this result back (--reuse)".to_owned(),
                        Err(e) => format!("not recorded: {e:#}"),
                    };
                    let _ = ev.send(Ev::Note(note)).await;
                }
                Ev::Exit { code: Some(0), .. } => {
                    let _ = ev.send(Ev::Note("not recorded: its output was more than a record keeps, or some of it was dropped".to_owned())).await;
                }
                _ => {}
            }
            let _ = ev.send(e).await;
        }
    };
    futures::join!(running, forwarding);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runner(name: &str) -> RunnerInfo {
        RunnerInfo { name: name.to_owned(), os: "linux".to_owned(), arch: "x86_64".to_owned(), connected: true, ..RunnerInfo::default() }
    }

    /// W24-R24, R01: a run's key takes its command, its folder in the tree, the environment given
    /// it, the tree, the machine and whose it is; nothing else (its timeout) moves it.
    #[test]
    fn w24_r24_a_runs_key_takes_what_makes_it_identical() {
        let req = RunReq { target: "linux".into(), command: "cargo test".into(), env: vec![("MODE".into(), "fast".into())], timeout_ms: Some(1000), ..RunReq::default() };
        let k = |r: &RunReq, rel: &str, tree: &str, on: &str, who: &str| key::weak(&unit(r, rel, tree, &runner(on), who));
        let base = k(&req, "crates/app", "t1", "fedora", "person:ada");
        assert_eq!(base, k(&RunReq { timeout_ms: None, target: String::new(), ..req.clone() }, "crates/app", "t1", "fedora", "person:ada"));
        let others = [
            k(&RunReq { command: "cargo test -q".into(), ..req.clone() }, "crates/app", "t1", "fedora", "person:ada"),
            k(&RunReq { env: vec![("MODE".into(), "slow".into())], ..req.clone() }, "crates/app", "t1", "fedora", "person:ada"),
            k(&req, "crates", "t1", "fedora", "person:ada"),
            k(&req, "crates/app", "t2", "fedora", "person:ada"),
            k(&req, "crates/app", "t1", "jupiter", "person:ada"),
            k(&req, "crates/app", "t1", "fedora", "person:bob"),
        ];
        for (i, o) in others.iter().enumerate() {
            assert_ne!(*o, base, "edit {i} changes the key");
        }
        assert!(unkeyable(&RunReq { stdin: true, ..req.clone() }).is_some() && unkeyable(&RunReq { shell: Some("s".into()), ..req.clone() }).is_some());
        assert!(unkeyable(&req).is_none());
        assert!(person().starts_with("person:") && crate::runner::work::token::is_person(&person()));
    }

    fn sh(dir: &Path, args: &[&str]) -> anyhow::Result<()> {
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "kiln")
            .env("GIT_AUTHOR_EMAIL", "kiln@localhost")
            .env("GIT_COMMITTER_NAME", "kiln")
            .env("GIT_COMMITTER_EMAIL", "kiln@localhost")
            .output()?
            .status
            .success();
        anyhow::ensure!(ok, "git {args:?}");
        Ok(())
    }

    /// W24-R24: the tree is what sync sends, by content: ignored output and git's own records
    /// don't move it; a changed byte, a new file or an executable bit does.
    #[test]
    fn w24_r24_a_trees_digest_is_what_sync_sends() -> anyhow::Result<()> {
        let t = tempfile::tempdir()?;
        let r = t.path();
        sh(r, &["init", "-q", "-b", "main"])?;
        std::fs::write(r.join(".gitignore"), "target/\n")?;
        std::fs::write(r.join("a.txt"), "one\n")?;
        sh(r, &["add", "-A"])?;
        sh(r, &["commit", "-qm", "one"])?;
        let first = tree(r)?;
        std::fs::create_dir_all(r.join("target"))?;
        std::fs::write(r.join("target/out"), "built")?;
        sh(r, &["commit", "-q", "--allow-empty", "-m", "nothing"])?;
        assert_eq!(tree(r)?, first, "ignored output and a new commit of the same tree");
        std::fs::write(r.join("a.txt"), "two\n")?;
        let edited = tree(r)?;
        assert_ne!(edited, first, "the same size, another byte");
        std::fs::write(r.join("a.txt"), "one\n")?;
        assert_eq!(tree(r)?, first, "by content, not by time");
        std::fs::write(r.join("new.txt"), "")?;
        let added = tree(r)?;
        assert_ne!(added, first, "an untracked file git doesn't ignore");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(r.join("new.txt"), std::fs::Permissions::from_mode(0o755))?;
            assert_ne!(tree(r)?, added, "an executable bit");
        }
        Ok(())
    }

    #[test]
    fn w24_r24_kept_output_reads_back() {
        let pieces = vec![(false, b"out\n".to_vec()), (true, b"err\n".to_vec()), (false, Vec::new())];
        assert_eq!(decode(&encode(&pieces)), Some(pieces.clone()));
        let mut cut = encode(&pieces);
        cut.truncate(cut.len().saturating_sub(2));
        assert_eq!(decode(&cut), None, "a record cut short doesn't read");
        let mut k = Kept::default();
        k.see(false, &vec![0u8; MAX_LOG]);
        assert!(!k.over && k.pieces.len() == 1);
        k.see(true, b"x");
        assert!(k.over && k.pieces.is_empty(), "more than a record keeps");
        assert_eq!(ago(30), "30 s ago");
        assert_eq!(ago(7200), "2 h ago");
    }
}
