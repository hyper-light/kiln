//! The bare-metal engine's workspaces end to end (kiln goal 01a1ec34599974448aed2672bafaf9a2; docs/agentic.md W2–W6):
//! agents started through kiln on a hub of its own, with a Slates instance of the test's own run by a kiln executable
//! in Slates' roles (`KILN_TEST_KILN`: no Slates binary anywhere), on any machine that can mount a volume (FUSE on
//! Linux, NFS on loopback on macOS, WinFsp at a drive letter on Windows); skipped, saying so, elsewhere. No harness ever runs (each agent's program doesn't exist), so each session stays as
//! kiln made it, its folder its workspace.
//!
//! - Every harness kiln runs, started asking for a volume, works in a volume of its own, mounted, its folder there.
//! - A child started in its parent's folder works in its parent's volume; the volume outlives the parent while the
//!   child works in it, and goes with the child.
//! - Review: a change request from a volume holds what changed there.
//! - Drift: a file the agent changed, changed on disk after, is drift.
//! - Landing, as each agent's mode lets it (the person's rule, 2026-11-18): the plan leaves out what git ignores and
//!   git's own directory; an agent whose mode asks reads its plan and its writing waits on its person's click, then the
//!   person writes it; one whose mode edits unasked writes its own at once; each into the folder, its repository's
//!   HEAD as it was, approved with kiln's launcher secret on every system (on Linux the anchor's systemd scope keeps
//!   it on its standard input, Slates' contract 14); each volume's write policy is its agent's mode, as kiln set it.
//!
//! The test owns the instance it starts: it ends every agent it started, then stops the anchor kiln started for it (by
//! the pid kiln kept, `super::slates::started_anchor`) only once none of its volumes is mounted. A mount whose server is
//! gone can hang whatever looks at it, and a loopback NFS mount's can take macOS down with it, so a mount that won't go
//! leaves the anchor running, said loudly, and a test that runs out of time leaves it too.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::harness::session::{Info, Spec};
use crate::hub::Hub;
use crate::ops::{Cx, call};

use super::place;

/// How long the whole test may take: ten volumes made and mounted, a landing and every end, on a loaded machine.
const WITHIN: Duration = Duration::from_secs(900);
/// How long a volume may take to go once its last agent ends (its unmount and destroy run apart: `super::release`).
const GONE_WITHIN: Duration = Duration::from_secs(60);
/// How long an ended parent's volume is watched to stay, while its child works in it: longer than its unmount and
/// destroy take when they do run (seconds), so a release made too early shows.
const STAYS_FOR: Duration = Duration::from_secs(5);
/// How often a mount is looked at while waited on.
const LOOK_EVERY: Duration = Duration::from_millis(100);

/// Git, in `dir`, with no person's configuration: its output, or what it refused.
fn git(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("git").args(args).current_dir(dir).env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_TERMINAL_PROMPT", "0").output()?;
    anyhow::ensure!(out.status.success(), "git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr));
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn get<'a>(v: &'a Value, pointer: &str) -> &'a Value {
    v.pointer(pointer).unwrap_or(&Value::Null)
}

/// Waits until `done` holds, looking every [`LOOK_EVERY`], for at most `within`: whether it came to hold.
async fn until(within: Duration, mut done: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + within;
    loop {
        if done() {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        crate::rt::sleep(LOOK_EVERY).await;
    }
}

/// The volume and mount an agent works in.
fn made_of(info: &Info) -> anyhow::Result<super::slates::Made> {
    info.workspace.as_ref().and_then(|w| w.made.clone()).ok_or_else(|| anyhow::anyhow!("{} works in no volume: {:?}", info.name, info.workspace))
}

/// Whether a volume is mounted at `mount` (a folder, or a drive letter on Windows): on Unix the kernel's mount table's
/// word (`place::listed`), so a mount left behind, whose point a stat can no longer read, still counts as there.
fn mounted(mount: &str) -> bool {
    #[cfg(unix)]
    {
        place::listed(Path::new(mount)).unwrap_or(true)
    }
    #[cfg(not(unix))]
    {
        place::is_mount(Path::new(mount))
    }
}

/// A repository with a remote, a tracked file, a file git ignores and the ignored file's folder: (the bare remote, the
/// working folder, its top as git names it).
fn project(bare: &Path, work: &Path) -> anyhow::Result<String> {
    git(bare, &["init", "-q", "--bare", "-b", "main"])?;
    git(work, &["init", "-q", "-b", "main"])?;
    for (key, value) in [("user.name", "Ada"), ("user.email", "ada@example.com"), ("commit.gpgsign", "false"), ("tag.gpgsign", "false")] {
        git(work, &["config", key, value])?;
    }
    std::fs::create_dir_all(work.join("src"))?;
    std::fs::create_dir_all(work.join("target"))?;
    std::fs::write(work.join(".gitignore"), "target/\n")?;
    std::fs::write(work.join("src").join("a.rs"), "a\n")?;
    git(work, &["add", "."])?;
    git(work, &["commit", "-qm", "init"])?;
    git(work, &["remote", "add", "origin", &bare.to_string_lossy()])?;
    git(work, &["push", "-q", "origin", "main"])?;
    std::fs::write(work.join("target").join("out"), "built\n")?;
    git(work, &["rev-parse", "--show-toplevel"])
}

/// Kiln goal 01a1ec34599974448aed2672bafaf9a2 (W2–W6): every harness kiln runs works on a Slates volume of its own when
/// it names none, a child works in its parent's, and their work is reviewed, drifts and lands as each agent's mode lets
/// it, each volume going with the last agent working in it.
#[test]
fn w2_every_harness_and_agent_works_on_a_volume_reviews_drifts_and_lands_under_a_grant() -> anyhow::Result<()> {
    // The kiln executable whose `slates-role` runs the test's instance: a unit test's own executable isn't kiln's.
    let Some(program) = std::env::var_os("KILN_TEST_KILN") else {
        eprintln!("skipped: KILN_TEST_KILN names no kiln executable (cargo build -p kiln, then KILN_TEST_KILN=<target>/debug/kiln)");
        return Ok(());
    };
    if let Err(fault) = slates_api::mount::available() {
        eprintln!("skipped: {fault}");
        return Ok(());
    }
    let instance = format!("kilntest{}", std::process::id());
    let o = super::slates::Override { instance, program: Some(program.into()), quick: true };
    anyhow::ensure!(super::slates::set_override(o), "another test pointed workspaces elsewhere");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(crate::rt::block_on(run()));
    });
    match rx.recv_timeout(WITHIN) {
        Ok(r) => r,
        // Its instance is left running: nothing here knows its mounts are gone.
        Err(_) => anyhow::bail!("the workspaces hung (its Slates instance is left running)"),
    }
}

/// The agents a test started, ended at its end whatever happened, its instance stopped once their volumes are gone.
struct Started {
    hub: Arc<Hub>,
    agents: Vec<Info>,
}

impl Started {
    async fn open(&mut self, spec: Spec) -> anyhow::Result<Info> {
        let info = self.hub.harness.open(&self.hub, "local", spec, 1).await?.into_iter().next().ok_or_else(|| anyhow::anyhow!("no agent"))?;
        self.agents.push(info.clone());
        Ok(info)
    }

    /// Ends every agent still open, waits for their volumes' mounts to go, then stops the instance's anchor where none
    /// is left.
    async fn end(self) {
        let mounts: HashSet<String> = self.agents.iter().filter_map(|i| i.workspace.as_ref().and_then(|w| w.made.as_ref()).map(|m| m.mount.clone())).collect();
        for info in &self.agents {
            let _ = self.hub.harness.close(&self.hub, &info.id).await;
        }
        let gone = until(GONE_WITHIN, || mounts.iter().all(|m| !mounted(m))).await;
        let Some(pid) = super::slates::started_anchor() else { return };
        if !gone {
            let left: Vec<&String> = mounts.iter().filter(|m| mounted(m)).collect();
            eprintln!("LEFT RUNNING: Slates' anchor (pid {pid}) still serves mounts that didn't go: {left:?}");
            return;
        }
        stop(pid);
    }
}

/// Stops the anchor `pid` kiln started for this test (its daemon goes with it).
fn stop(pid: u32) {
    let said = if cfg!(windows) { Command::new("taskkill").args(["/PID", &pid.to_string(), "/T", "/F"]).output() } else { Command::new("kill").args(["-TERM", &pid.to_string()]).output() };
    if let Err(e) = said {
        eprintln!("Slates' anchor (pid {pid}) couldn't be stopped: {e}");
    }
}

async fn run() -> anyhow::Result<()> {
    // Slates workspaces on (`[workspaces] enabled`, off by default).
    let hub = crate::hub::tests::scratch_hub_with(|c| {
        c.terminal.enabled = true;
        c.workspaces.enabled = true;
    })
    .await?;
    let h = Arc::clone(&hub);
    crate::rt::spawn(async move {
        let _ = crate::harness::registry::Registry::run_local(h).await;
    });
    let mut started = Started { hub: Arc::clone(&hub), agents: Vec::new() };
    let r = scenario(&hub, &mut started).await;
    started.end().await;
    r
}

async fn scenario(hub: &Arc<Hub>, started: &mut Started) -> anyhow::Result<()> {
    let (bare, work) = (tempfile::tempdir()?, tempfile::tempdir()?);
    let top = project(bare.path(), work.path())?;
    let src = PathBuf::from(&top).join("src");
    let missing = work.path().join("no-harness").to_string_lossy().into_owned();
    let cx = Cx { hub: Arc::clone(hub), caller: crate::authz::Caller::owner() };
    let fail = |(c, m): crate::ops::Fail| anyhow::anyhow!("{c}: {m}");

    // Every harness, asking for a volume: a volume of its own each, mounted, its folder there.
    let mut volumes = HashSet::new();
    let mut each = Vec::new();
    for kind in crate::harness::kinds::Kind::all().iter().filter(|k| k.structured()) {
        let spec = Spec { kind: kind.id.into(), cwd: Some(src.to_string_lossy().into_owned()), name: Some(format!("{} on a volume", kind.name)), program: Some(missing.clone()), workspace: Some(super::Workspace { kind: "volume".into(), ..Default::default() }), ..Spec::default() };
        let info = started.open(spec).await?;
        let w = info.workspace.clone().ok_or_else(|| anyhow::anyhow!("{}: no workspace", kind.id))?;
        anyhow::ensure!(w.kind == "volume" && w.owner.is_none(), "{}: {w:?}", kind.id);
        anyhow::ensure!(w.over.as_deref() == Some(top.as_str()), "{}: laid over {:?}, not {top}", kind.id, w.over);
        let made = made_of(&info)?;
        anyhow::ensure!(mounted(&made.mount), "{}: nothing mounted at {}", kind.id, made.mount);
        anyhow::ensure!(super::within(&info.cwd, &made.mount) && info.cwd.ends_with("src"), "{}: works in {}, not in {}", kind.id, info.cwd, made.mount);
        anyhow::ensure!(volumes.insert(made.volume.clone()), "{}: shares volume {} with another", kind.id, made.volume);
        // What it hasn't touched is the folder's, build output too.
        anyhow::ensure!(std::fs::read_to_string(Path::new(&info.cwd).join("a.rs"))? == "a\n", "{}: a.rs", kind.id);
        anyhow::ensure!(std::fs::read_to_string(place::top(&made.mount).join("target").join("out"))? == "built\n", "{}: target/out", kind.id);
        each.push(info);
    }
    let Some(parent) = each.first().cloned() else { anyhow::bail!("no harness") };
    let parent_made = made_of(&parent)?;

    // A child in its parent's folder: its parent's volume, no new one.
    let child = started.open(Spec { kind: "claude".into(), cwd: Some(parent.cwd.clone()), parent: Some(parent.id.clone()), name: Some("A child".into()), program: Some(missing.clone()), ..Spec::default() }).await?;
    let cw = child.workspace.clone().unwrap_or_default();
    anyhow::ensure!(cw.made.as_ref() == Some(&parent_made) && cw.owner.as_deref() == Some(parent.id.as_str()), "the child works in {cw:?}, not its parent's {parent_made:?}");
    anyhow::ensure!(child.cwd == parent.cwd, "the child works in {}, not {}", child.cwd, parent.cwd);

    // What they change stays in the volume; the folder is as it was.
    std::fs::write(Path::new(&parent.cwd).join("a.rs"), "a2\n")?;
    std::fs::write(Path::new(&parent.cwd).join("new.rs"), "n\n")?;
    std::fs::write(Path::new(&child.cwd).join("child.rs"), "c\n")?;
    anyhow::ensure!(std::fs::read_to_string(src.join("a.rs"))? == "a\n", "the folder is as it was");
    anyhow::ensure!(!src.join("new.rs").exists() && !src.join("child.rs").exists(), "nothing new in the folder");

    // Review: the parent's change request holds everything that changed in its volume, its child's file too.
    let out = call(cx.clone(), "agents.change", json!({"agent": parent.id, "title": "Change a"})).await.map_err(fail)?;
    let branch = get(&out, "/change_request/branch").as_str().unwrap_or_default().to_owned();
    for (file, bytes) in [("src/a.rs", "a2"), ("src/new.rs", "n"), ("src/child.rs", "c")] {
        anyhow::ensure!(git(bare.path(), &["show", &format!("{branch}:{file}")])? == bytes, "{file} on {branch}");
    }

    // Drift: a file the parent changed, changed on disk too, for both of them.
    std::fs::write(src.join("a.rs"), "the person's\n")?;
    for who in [&parent, &child] {
        let ws = call(cx.clone(), "agents.workspace", json!({"agent": who.id})).await.map_err(fail)?;
        anyhow::ensure!(get(&ws, "/drift").as_array().is_some_and(|d| d.iter().any(|x| x.as_str().is_some_and(|s| s.contains("a.rs")))), "{}: {ws}", who.name);
    }

    // Landing, as each agent's mode lets it (docs/agentic.md W6, the person's rule of 2026-11-18), what git ignores and
    // git's own directory left out: one whose mode asks reads its plan, and its writing waits on its person's click (no
    // person's turn here: refused, nothing written), then the person writes it; one whose mode edits unasked writes its
    // own at once. Each approved with kiln's launcher secret, on every system.
    let now = |i: &Info| hub.harness.info(&i.id).unwrap_or_else(|| i.clone());
    let others: Vec<Info> = each.iter().skip(1).map(now).collect();
    let policy = |i: &Info| super::landing_policy(i, i);
    let asking = others.iter().find(|i| policy(i) == slates_api::WriteMode::Ask).cloned().ok_or_else(|| anyhow::anyhow!("no harness whose own default mode asks"))?;
    let writing = others.iter().find(|i| policy(i) == slates_api::WriteMode::Bypass).cloned().ok_or_else(|| anyhow::anyhow!("no harness whose own default mode edits unasked"))?;
    // Each volume's write policy is its agent's mode, as kiln, the launcher, set it when it made the volume.
    for (who, want) in [(&asking, slates_api::WriteMode::Ask), (&writing, slates_api::WriteMode::Bypass)] {
        let set = super::slates::policy(&made_of(who)?).await?;
        anyhow::ensure!(set.is_some_and(|p| p.land == want && p.merge == want), "{}: its volume's policy is {set:?}, not {want:?}", who.name);
    }
    let as_agent = |i: &Info| {
        let (hub, id) = (Arc::clone(hub), i.id.clone());
        async move { Ok::<_, anyhow::Error>(Cx { caller: crate::mcp::session_caller(&hub, &id, None).await.map_err(anyhow::Error::msg)?, hub }) }
    };
    let head = git(work.path(), &["rev-parse", "HEAD"])?;
    std::fs::write(Path::new(&asking.cwd).join("asked.rs"), "asked\n")?;
    let plan = call(as_agent(&asking).await?, "agents.land", json!({"agent": asking.id})).await.map_err(fail)?;
    anyhow::ensure!(get(&plan, "/outcome") == &json!("approval_required") && get(&plan, "/plan/grant_required") == &json!(true), "{}: {plan}", asking.name);
    // What git ignores (the folder's build output, seen through the volume) and git's own directory, left out.
    anyhow::ensure!(get(&plan, "/excludes") == &json!(["/target", "/.git"]), "{plan}");
    let manifest = get(&plan, "/approval/manifest").as_str().unwrap_or_default().to_owned();
    let refused = call(as_agent(&asking).await?, "agents.land", json!({"agent": asking.id, "manifest": manifest})).await.err().map(|(c, m)| format!("{c} {m}")).unwrap_or_default();
    anyhow::ensure!(refused.contains("403") && refused.contains("isn't serving a person's chat turn"), "{}'s own writing waits on its person: {refused}", asking.name);
    anyhow::ensure!(!src.join("asked.rs").exists(), "nothing written while it waits");
    // kiln approves on every system: on Linux its anchor's systemd scope keeps the launcher secret (Slates' contract 14).
    anyhow::ensure!(get(&plan, "/can_write") == &json!(true), "kiln can write a landing here: {plan}");
    std::fs::write(Path::new(&writing.cwd).join("wrote.rs"), "wrote\n")?;
    let landed = call(cx.clone(), "agents.land", json!({"agent": asking.id, "manifest": manifest})).await.map_err(fail)?;
    anyhow::ensure!(get(&landed, "/outcome") == &json!("written") && landed.get("landed").is_some(), "{landed}");
    anyhow::ensure!(std::fs::read_to_string(src.join("asked.rs"))? == "asked\n", "the folder holds the person's landing");
    let wrote = call(as_agent(&writing).await?, "agents.land", json!({"agent": writing.id})).await.map_err(fail)?;
    anyhow::ensure!(get(&wrote, "/outcome") == &json!("written"), "{}'s own landing, at once: {wrote}", writing.name);
    anyhow::ensure!(std::fs::read_to_string(src.join("wrote.rs"))? == "wrote\n", "the folder holds its own landing");
    anyhow::ensure!(git(work.path(), &["rev-parse", "HEAD"])? == head, "its HEAD as it was");

    // The parent ends: its volume stays while its child works in it, and goes with the child.
    anyhow::ensure!(hub.harness.close(hub, &parent.id).await, "the parent didn't end");
    let early = until(STAYS_FOR, || !mounted(&parent_made.mount)).await;
    anyhow::ensure!(!early, "the parent's volume went while its child worked in it");
    std::fs::write(Path::new(&child.cwd).join("after.rs"), "after\n")?;
    anyhow::ensure!(std::fs::read_to_string(Path::new(&child.cwd).join("after.rs"))? == "after\n", "the child still works in it");
    anyhow::ensure!(hub.harness.close(hub, &child.id).await, "the child didn't end");
    anyhow::ensure!(until(GONE_WITHIN, || !mounted(&parent_made.mount)).await, "the volume stayed after its last agent ended");
    Ok(())
}

