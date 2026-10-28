//! Agents' workspaces of their own, on Slates (docs/agentic.md, W): an agent's folder is a Slates
//! volume laid over the real one, mounted where the machine's runner keeps workspaces.
//!
//! An overlay volume reads every file the agent hasn't touched from the folder itself, as it is
//! now, and keeps what the agent changes (its edits, its build's output, git's own writes) in
//! memory, in the volume alone: the folder, and every other agent's volume, never see it. What
//! leaves it goes as a change request (`super::change`) or, written back under a person's grant,
//! to the folder itself.
//!
//! The machine keeps one Slates instance (`kiln`) for all its runner processes. Its anchor is
//! started apart from the runner (its own systemd user unit on Linux, its own process group
//! elsewhere), so restarting or upgrading the runner never ends it: a volume lives in the
//! anchor's memory, and goes when it does.
//!
//! Slates as built today (c8d6adb4) can't clone a snapshot of an overlay volume
//! (`ContentUnavailable`), and a pinned base makes every pinned file part of a landing's plan, so
//! each agent gets an overlay of its own, unpinned: what it hasn't touched is the folder as it is
//! now. Files it has touched are witnessed when it first writes them, and a later change on disk
//! to one of them is reported as drift.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The Slates instance kiln keeps on a machine.
pub const INSTANCE: &str = "kiln";
/// How long one Slates verb may take.
const VERB_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a new anchor has to answer.
const UP_WAIT: Duration = Duration::from_secs(30);
/// Shards the instance runs (each is a core's worth of work when busy): agents' folders need few.
const SHARDS: u32 = 2;
/// The largest a workspace may grow to, at most (what it changes is held in memory): a quarter of
/// the machine's memory, up to this.
const MAX_VOLUME_GIB: u64 = 64;

struct Here {
    /// Where workspaces are mounted (`<state>/workspaces/<name>`).
    root: PathBuf,
    /// Where kiln keeps a Slates binary it installed (`<state>/bin`).
    bin_dir: PathBuf,
    /// The anchor's log, where it says how a person's grant reaches it (macOS, Windows).
    log: PathBuf,
}

static HERE: OnceLock<Here> = OnceLock::new();

/// Where this process keeps its machine's workspaces: under `state_dir` (a runner's, shared by
/// every runner process of the machine; the server's data directory for its own agents).
pub fn init(state_dir: &Path) {
    let _ = HERE.set(Here { root: state_dir.join("workspaces"), bin_dir: state_dir.join("bin"), log: state_dir.join("slates-anchor.log") });
}

fn here() -> anyhow::Result<&'static Here> {
    HERE.get().ok_or_else(|| anyhow::anyhow!("this process keeps no workspaces"))
}

/// What a test points this process's workspaces at instead: a Slates binary, an instance of its
/// own, its anchor started here. Set once, before anything here runs.
#[derive(Debug, Clone)]
pub struct Override {
    pub binary: PathBuf,
    pub instance: String,
}

static OVERRIDE: OnceLock<Override> = OnceLock::new();

/// Points this process's workspaces at `o` (a test's own Slates): false when something already did.
pub fn set_override(o: Override) -> bool {
    OVERRIDE.set(o).is_ok()
}

/// The instance kiln uses (`KILN_SLATES_INSTANCE` names another, for labs).
fn instance() -> String {
    if let Some(o) = OVERRIDE.get() {
        return o.instance.clone();
    }
    std::env::var("KILN_SLATES_INSTANCE").ok().filter(|i| !i.trim().is_empty()).unwrap_or_else(|| INSTANCE.to_owned())
}

/// The Slates binary: `KILN_SLATES`, else the one kiln installed, else `slates` on `PATH`.
pub fn binary() -> Option<PathBuf> {
    if let Some(o) = OVERRIDE.get() {
        return Some(o.binary.clone());
    }
    if let Some(p) = std::env::var_os("KILN_SLATES").map(PathBuf::from).filter(|p| p.is_file()) {
        return Some(p);
    }
    if let Some(h) = HERE.get() {
        let mine = h.bin_dir.join(if cfg!(windows) { "slates.exe" } else { "slates" });
        if mine.is_file() {
            return Some(mine);
        }
    }
    crate::os::which("slates")
}

/// How a volume is mounted here, when it can be: `fuse` (Linux, through `fusermount3`, no
/// privilege) or `nfs` (macOS, on loopback).
pub fn mount_kind() -> Option<&'static str> {
    if cfg!(target_os = "linux") {
        crate::os::which("fusermount3").map(|_| "fuse")
    } else if cfg!(target_os = "macos") {
        Some("nfs")
    } else {
        None
    }
}

/// What a machine says of its workspaces, in its facts.
pub type Facts = kiln_ci::facts::Workspaces;

/// This machine's workspace facts, looked at now.
pub async fn facts() -> Facts {
    let Some(bin) = binary() else {
        return Facts { why_not: Some("Slates isn't installed on this machine".to_owned()), ..Facts::default() };
    };
    let slates = Some(bin.to_string_lossy().into_owned());
    let Some(mount) = mount_kind() else {
        let why = if cfg!(target_os = "linux") { "fusermount3 isn't installed (the fuse3 package)" } else { "Slates can't mount a volume on this system yet" };
        return Facts { slates, why_not: Some(why.to_owned()), ..Facts::default() };
    };
    if HERE.get().is_none() {
        return Facts { slates, mount: Some(mount.to_owned()), why_not: Some("this kiln keeps no workspaces".to_owned()), ..Facts::default() };
    }
    Facts { available: true, slates, mount: Some(mount.to_owned()), running: up().await, why_not: None }
}

/// Runs `slates --instance <kiln's> <args> --json`: its JSON answer, or what it refused.
async fn verb(args: &[&str]) -> anyhow::Result<Value> {
    let bin = binary().ok_or_else(|| anyhow::anyhow!("Slates isn't installed on this machine"))?;
    let mut c = Command::new(&bin);
    c.arg("--instance").arg(instance()).args(args).arg("--json");
    let out = crate::proc::output(c, VERB_TIMEOUT).await?;
    let text = String::from_utf8_lossy(&out.stdout);
    if out.status.success() {
        return Ok(text.lines().rev().find_map(|l| serde_json::from_str(l).ok()).unwrap_or(Value::Null));
    }
    let err = String::from_utf8_lossy(&out.stderr);
    let said = err.lines().chain(text.lines()).find_map(|l| serde_json::from_str::<Value>(l).ok()).and_then(|v| v.pointer("/error/message").and_then(Value::as_str).map(str::to_owned));
    anyhow::bail!("slates {}: {}", args.first().copied().unwrap_or_default(), said.unwrap_or_else(|| err.trim().chars().take(400).collect()))
}

/// The instance answers.
async fn up() -> bool {
    let Some(bin) = binary() else { return false };
    let mut c = Command::new(bin);
    c.arg("--instance").arg(instance()).args(["status", "--json"]);
    crate::proc::output(c, Duration::from_secs(5)).await.is_ok_and(|o| o.status.success())
}

/// Starts the instance's anchor apart from this process: a systemd user unit of its own on Linux
/// where there is one (stopping or restarting the runner's unit never takes it along), its own
/// process group elsewhere.
async fn start_anchor(bin: &Path, log: &Path) -> anyhow::Result<()> {
    let shards = SHARDS.to_string();
    let args = ["--instance", &instance(), "anchor", "--shards", &shards];
    // Tests and labs start theirs here (`KILN_SLATES_DIRECT`): no unit outlives them.
    let direct = OVERRIDE.get().is_some() || std::env::var_os("KILN_SLATES_DIRECT").is_some();
    if !direct && cfg!(target_os = "linux") && std::env::var_os("XDG_RUNTIME_DIR").is_some() && crate::os::which("systemd-run").is_some() {
        let unit = format!("{}-slates", instance());
        let mut reset = Command::new("systemctl");
        reset.args(["--user", "reset-failed", &format!("{unit}.service")]);
        let _ = crate::proc::output(reset, Duration::from_secs(10)).await;
        let mut c = Command::new("systemd-run");
        c.args(["--user", "--quiet", "--collect", &format!("--unit={unit}"), "--"]).arg(bin).args(args);
        match crate::proc::output(c, Duration::from_secs(20)).await {
            Ok(o) if o.status.success() => return Ok(()),
            Ok(o) => tracing::info!(said = %String::from_utf8_lossy(&o.stderr).trim(), "Slates' anchor didn't start as a unit of its own; starting it here"),
            Err(e) => tracing::info!(error = %format!("{e:#}"), "Slates' anchor didn't start as a unit of its own; starting it here"),
        }
    }
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let out = std::fs::OpenOptions::new().create(true).append(true).open(log)?;
    let err = out.try_clone()?;
    let mut c = Command::new(bin);
    c.args(args).stdin(std::process::Stdio::null()).stdout(out).stderr(err);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        c.process_group(0);
    }
    let child = c.spawn()?;
    // Never waited on: it outlives this process.
    drop(child);
    Ok(())
}

/// One start of the anchor at a time: agents starting together find it started by the first.
fn starting() -> &'static futures::lock::Mutex<()> {
    static STARTING: OnceLock<futures::lock::Mutex<()>> = OnceLock::new();
    STARTING.get_or_init(|| futures::lock::Mutex::new(()))
}

/// The instance answers, its anchor started (and its one-node group made) when it wasn't running.
pub async fn ensure_up() -> anyhow::Result<()> {
    if up().await {
        return Ok(());
    }
    let _one = starting().lock().await;
    if up().await {
        return Ok(());
    }
    let h = here()?;
    let bin = binary().ok_or_else(|| anyhow::anyhow!("Slates isn't installed on this machine"))?;
    start_anchor(&bin, &h.log).await?;
    let until = std::time::Instant::now() + UP_WAIT;
    while !up().await {
        if std::time::Instant::now() > until {
            anyhow::bail!("Slates' anchor didn't answer within {}s (its log: {})", UP_WAIT.as_secs(), h.log.display());
        }
        crate::rt::sleep(Duration::from_millis(200)).await;
    }
    // A new instance holds no volumes until its group exists; an existing one says so and is left as it is.
    if let Err(e) = verb(&["bootstrap", "root"]).await {
        tracing::debug!(error = %format!("{e:#}"), "Slates' group was there already");
    }
    Ok(())
}

/// The most a workspace may hold, in Slates' units: a quarter of this machine's memory, at most
/// [`MAX_VOLUME_GIB`].
fn volume_max() -> String {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    let quarter = (sys.total_memory() / 4) >> 30;
    format!("{}GiB", quarter.clamp(1, MAX_VOLUME_GIB))
}

/// A workspace as it stands: the volume, its name and where it's mounted.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Made {
    pub volume: String,
    pub name: String,
    pub mount: String,
}

/// Where workspace `name` is mounted.
pub fn mount_of(name: &str) -> anyhow::Result<PathBuf> {
    Ok(here()?.root.join(name))
}

/// A new workspace `name` laid over `over`, mounted.
pub async fn make(name: &str, over: &Path) -> anyhow::Result<Made> {
    ensure_up().await?;
    let over_s = over.to_string_lossy().into_owned();
    let max = volume_max();
    let made = verb(&["volume", "create", name, "--dynamic", &max, "--base", &over_s]).await?;
    let volume = made.get("id").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("Slates made no volume: {made}"))?.to_owned();
    let mount = mount_of(name)?;
    let m = Made { volume, name: name.to_owned(), mount: mount.to_string_lossy().into_owned() };
    if let Err(e) = mounted(&m).await {
        let _ = verb(&["volume", "destroy", &m.volume]).await;
        return Err(e);
    }
    Ok(m)
}

/// Whether something is mounted at `dir` (its device differs from its parent's).
#[cfg(unix)]
fn is_mount(dir: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    let (Ok(d), Some(Ok(p))) = (std::fs::metadata(dir), dir.parent().map(std::fs::metadata)) else { return false };
    d.dev() != p.dev()
}

#[cfg(not(unix))]
fn is_mount(_dir: &Path) -> bool {
    false
}

/// Workspace `m` is mounted where it says (again, after the machine's mount went away while its
/// volume stayed).
pub async fn mounted(m: &Made) -> anyhow::Result<()> {
    let dir = PathBuf::from(&m.mount);
    if is_mount(&dir) {
        return Ok(());
    }
    std::fs::create_dir_all(&dir)?;
    let mount = m.mount.clone();
    verb_plain(&["mount", &m.volume, &mount]).await?;
    anyhow::ensure!(is_mount(&dir), "Slates said it mounted {} at {mount}, but nothing is mounted there", m.name);
    Ok(())
}

/// A verb whose answer is text (`mount`, `unmount`).
async fn verb_plain(args: &[&str]) -> anyhow::Result<String> {
    let bin = binary().ok_or_else(|| anyhow::anyhow!("Slates isn't installed on this machine"))?;
    let mut c = Command::new(bin);
    c.arg("--instance").arg(instance()).args(args);
    let out = crate::proc::output(c, VERB_TIMEOUT).await?;
    let said = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    anyhow::ensure!(out.status.success(), "slates {}: {}", args.first().copied().unwrap_or_default(), said.trim());
    Ok(said)
}

/// Workspace `m` gone: unmounted, its volume destroyed and its mount point removed. What it held
/// is lost. A mount still in use (something the agent left running works there) is let go lazily
/// on Linux, and its volume destroyed all the same: nothing is kept for an agent that has ended.
pub async fn remove(m: &Made) -> anyhow::Result<()> {
    let dir = PathBuf::from(&m.mount);
    if is_mount(&dir)
        && let Err(e) = verb_plain(&["unmount", &m.mount]).await
    {
        tracing::info!(mount = %m.mount, error = %format!("{e:#}"), "an ended agent's workspace is still in use: let go lazily");
        if cfg!(target_os = "linux") {
            let mut c = Command::new("fusermount3");
            c.args(["-u", "-z"]).arg(&dir);
            let _ = crate::proc::output(c, VERB_TIMEOUT).await;
        }
    }
    verb(&["volume", "destroy", &m.volume]).await?;
    let _ = std::fs::remove_dir(&dir);
    Ok(())
}

/// The files of the folder below workspace `m` that changed on disk after the agent changed them
/// in its workspace (each as Slates says it: `/README.md (Modified)`).
pub async fn drift(m: &Made) -> anyhow::Result<Vec<String>> {
    let st = verb(&["status", &m.volume, "--drift"]).await?;
    Ok(st.get("drifted").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect())
}

/// What a landing leaves out: git's own directory. Slates' command line takes one `--exclude`
/// (each flag keeps its last value, though its wire format takes several).
pub const EXCLUDES: &[&str] = &["/.git"];

fn land_args<'a>(m: &'a Made, over: &'a str) -> Vec<&'a str> {
    vec!["land", m.volume.as_str(), over, "--exclude", "/.git"]
}

/// How many paths git sees changed in workspace `m`: tracked files changed or deleted, and new
/// files git doesn't ignore, each one (`--untracked-files=all`).
async fn git_changed(m: &Made) -> anyhow::Result<usize> {
    let mut c = Command::new("git");
    c.arg("-C").arg(&m.mount).args(["status", "--porcelain=v1", "-z", "--untracked-files=all"]).env("GIT_TERMINAL_PROMPT", "0");
    let o = crate::proc::output(c, VERB_TIMEOUT).await?;
    anyhow::ensure!(o.status.success(), "git status: {}", String::from_utf8_lossy(&o.stderr).trim());
    Ok(count_changed(&String::from_utf8_lossy(&o.stdout)))
}

/// The paths of `git status --porcelain=v1 -z`'s records: a rename or copy carries its source as
/// one more field, which is no path of its own.
fn count_changed(text: &str) -> usize {
    let mut n = 0usize;
    let mut fields = text.split('\0').filter(|f| !f.is_empty());
    while let Some(record) = fields.next() {
        n = n.saturating_add(1);
        if record.starts_with('R') || record.starts_with('C') {
            let _ = fields.next();
        }
    }
    n
}

/// The files a landing's plan would write, create or delete (not counting directories).
fn planned_files(plan: &Value) -> usize {
    plan.pointer("/summary/by_action")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|a| !matches!(a.get("action").and_then(Value::as_str), Some("mkdir" | "rmdir")))
        .filter_map(|a| a.get("count").and_then(Value::as_u64))
        .map(|n| usize::try_from(n).unwrap_or(usize::MAX))
        .fold(0usize, usize::saturating_add)
}

/// Whether `plan` writes only what git sees changed in workspace `m`: with `.git` the one path
/// left out, anything more is something git ignores (build output, dependencies), which kiln
/// won't write into a person's folder.
pub async fn writable(m: &Made, plan: &Value) -> Result<(), String> {
    let planned = planned_files(plan);
    let changed = git_changed(m).await.map_err(|e| format!("{e:#}"))?;
    if planned <= changed {
        return Ok(());
    }
    Err(format!(
        "it would also write {} file{} git ignores (build output, dependencies) into the folder: Slates' command line leaves out only .git, so put the work up for review instead",
        planned.saturating_sub(changed),
        if planned.saturating_sub(changed) == 1 { "" } else { "s" }
    ))
}

/// What writing workspace `m` back to `over` would do: Slates' presentation (`landing`,
/// `manifest`, `summary`, `conflicts`, `grant_required`), or how it went if nothing needed a grant.
pub async fn plan_landing(m: &Made, over: &str) -> anyhow::Result<Value> {
    verb(&land_args(m, over)).await
}

/// How a person's grant reaches this machine's Slates: the variables its anchor said to export
/// (macOS and Windows, where the anchor's segment has a name). Linux has none yet: there the
/// segment is a descriptor only the anchor's own children hold (Slates' GAP-A9-10).
fn issuer_env() -> anyhow::Result<Vec<(String, String)>> {
    if cfg!(target_os = "linux") {
        anyhow::bail!("Slates on Linux can't take a person's go-ahead from kiln yet (Slates' GAP-A9-10): put the work up for review instead, or approve it with `slates grant` in the session that started Slates");
    }
    let log = std::fs::read_to_string(&here()?.log).unwrap_or_default();
    let line = log.lines().rev().find_map(|l| l.split_once("issuer surface: export ").map(|(_, rest)| rest.to_owned())).ok_or_else(|| anyhow::anyhow!("Slates' anchor here wasn't started by kiln, so kiln can't pass on a person's approval to it"))?;
    Ok(line.split_whitespace().filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_owned(), v.to_owned())).collect())
}

/// Whether this machine's Slates takes a person's go-ahead from kiln, and why not.
pub fn can_take_grants() -> anyhow::Result<()> {
    issuer_env().map(|_| ())
}

/// What came of writing a workspace back: written (Slates' outcome), or not, because the plan is no
/// longer the one the person saw (the new plan, to show them).
#[derive(Debug)]
pub enum Landed {
    Written(Value),
    Changed(Value),
}

/// Writes workspace `m` back to `over` exactly as the plan whose manifest is `manifest` (the one
/// the person saw) says. Slates holds a plan only while the command that made it runs, so the plan
/// is made again here: when its manifest is the one the person saw, the grant for it is given at
/// once and the landing made under it; when anything changed, nothing is written and the new plan
/// comes back. Either way the grant binds exactly what the person said yes to.
pub async fn land(m: &Made, over: &str, manifest: &str) -> anyhow::Result<Landed> {
    let env = issuer_env()?;
    let plan = plan_landing(m, over).await?;
    if plan.get("grant_required").and_then(Value::as_bool) != Some(true) {
        // Nothing waits on a grant (nothing to write, or it landed as it was).
        return Ok(Landed::Written(plan));
    }
    let (Some(landing), Some(now)) = (plan.get("landing").and_then(Value::as_u64), plan.get("manifest").and_then(Value::as_str)) else {
        anyhow::bail!("Slates' plan named no landing: {plan}");
    };
    if now != manifest {
        return Ok(Landed::Changed(plan));
    }
    writable(m, &plan).await.map_err(anyhow::Error::msg)?;
    let bin = binary().ok_or_else(|| anyhow::anyhow!("Slates isn't installed on this machine"))?;
    let mut c = Command::new(&bin);
    c.arg("--instance").arg(instance()).args(["grant", &landing.to_string(), manifest, "--json"]);
    for (k, v) in &env {
        c.env(k, v);
    }
    let out = crate::proc::output(c, VERB_TIMEOUT).await?;
    let said = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let why = err.lines().chain(said.lines()).find_map(|l| serde_json::from_str::<Value>(l).ok()).and_then(|v| v.pointer("/error/message").and_then(Value::as_str).map(str::to_owned));
        anyhow::bail!("Slates refused the approval: {}", why.unwrap_or_else(|| err.trim().to_owned()));
    }
    // `{"grant": N}` (Slates' `grant --json`).
    let grant = said
        .lines()
        .rev()
        .find_map(|l| serde_json::from_str::<Value>(l).ok())
        .and_then(|v| v.get("grant").or_else(|| v.get("id")).and_then(Value::as_u64))
        .ok_or_else(|| anyhow::anyhow!("Slates gave no grant: {said}"))?;
    let grant = grant.to_string();
    let mut args = land_args(m, over);
    args.push("--grant");
    args.push(&grant);
    Ok(Landed::Written(verb(&args).await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_workspace_never_holds_more_than_its_share_of_memory() {
        let max = volume_max();
        let gib: u64 = max.trim_end_matches("GiB").parse().unwrap_or(0);
        assert!((1..=MAX_VOLUME_GIB).contains(&gib), "{max}");
    }

    #[test]
    fn a_landing_is_weighed_against_what_git_sees_changed() {
        // ` M a`, `?? n`, a rename (its source a field of its own), a deletion.
        assert_eq!(count_changed(" M src/a.rs\0?? src/n.rs\0R  new.rs\0old.rs\0 D gone.rs\0"), 4);
        assert_eq!(count_changed(""), 0);
        let plan = serde_json::json!({"summary": {"by_action": [{"action": "replace", "count": 2}, {"action": "create", "count": 3}, {"action": "mkdir", "count": 4}]}});
        assert_eq!(planned_files(&plan), 5, "directories don't count");
        assert_eq!(planned_files(&serde_json::json!({})), 0);
    }
}

/// Workspaces end to end with a real Slates (`KILN_TEST_SLATES`: its binary; skipped, saying so,
/// without it), on a hub of its own, under a timeout: an agent kiln runs on the server, in a
/// volume of its own over a git folder. No harness ever runs (its program doesn't exist).
#[cfg(all(test, target_os = "linux"))]
mod end_to_end {
    use std::path::Path;
    use std::process::Command;
    use std::sync::Arc;
    use std::time::Duration;

    use serde_json::{Value, json};

    use crate::harness::session::Spec;
    use crate::ops::{Cx, call};

    fn sh(dir: &Path, script: &str) -> anyhow::Result<String> {
        let out = Command::new("sh").arg("-c").arg(script).current_dir(dir).env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_NOSYSTEM", "1").output()?;
        anyhow::ensure!(out.status.success(), "{script}: {}", String::from_utf8_lossy(&out.stderr));
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    fn get<'a>(v: &'a Value, pointer: &str) -> &'a Value {
        v.pointer(pointer).unwrap_or(&Value::Null)
    }

    /// Stops the test's own Slates instance, whatever happened.
    struct Instance(String);

    impl Drop for Instance {
        fn drop(&mut self) {
            let pattern = format!("[s]lates --instance {} ", self.0);
            let _ = Command::new("pkill").arg("-f").arg(&pattern).status();
        }
    }

    #[test]
    fn an_agent_works_in_a_volume_of_its_own_and_its_folder_stays_as_it_was() -> anyhow::Result<()> {
        let Some(bin) = std::env::var_os("KILN_TEST_SLATES") else {
            eprintln!("skipped: KILN_TEST_SLATES names no Slates binary");
            return Ok(());
        };
        if crate::os::which("fusermount3").is_none() {
            eprintln!("skipped: no fusermount3 to mount a volume with");
            return Ok(());
        }
        let instance = format!("kilntest{}", std::process::id());
        let _stop = Instance(instance.clone());
        anyhow::ensure!(super::set_override(super::Override { binary: bin.into(), instance }), "another test pointed workspaces elsewhere");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(crate::rt::block_on(run()));
        });
        match rx.recv_timeout(Duration::from_secs(120)) {
            Ok(r) => r,
            Err(_) => anyhow::bail!("the workspace hung"),
        }
    }

    async fn run() -> anyhow::Result<()> {
        let hub = crate::hub::tests::scratch_hub_with(|c| c.terminal.enabled = true).await?;
        let h = Arc::clone(&hub);
        crate::rt::spawn(async move {
            let _ = crate::harness::registry::Registry::run_local(h).await;
        });
        let bare = tempfile::tempdir()?;
        sh(bare.path(), "git init -q --bare -b main")?;
        let work = tempfile::tempdir()?;
        let remote = bare.path().to_string_lossy().into_owned();
        sh(work.path(), &format!("git init -q -b main && git config user.name Ada && git config user.email ada@example.com && git config commit.gpgsign false && git config tag.gpgsign false && printf 'target/\\n' > .gitignore && mkdir -p src target && echo a > src/a.rs && git add . && git commit -qm init && git remote add origin {remote} && git push -q origin main && echo built > target/out"))?;
        let cwd = work.path().join("src").to_string_lossy().into_owned();
        let spec = Spec {
            kind: "claude".into(),
            cwd: Some(cwd.clone()),
            name: Some("Change a".into()),
            program: Some(work.path().join("no-claude").to_string_lossy().into_owned()),
            workspace: Some(crate::workspace::Workspace { kind: "volume".into(), ..Default::default() }),
            ..Spec::default()
        };
        let infos = hub.harness.open(&hub, "local", spec, 1).await?;
        let info = infos.first().cloned().ok_or_else(|| anyhow::anyhow!("no agent"))?;
        let made = info.workspace.as_ref().and_then(|w| w.made.clone()).ok_or_else(|| anyhow::anyhow!("no workspace made: {:?}", info.workspace))?;
        // It works in its volume, in the folder it was asked to: what it hasn't touched is the folder's.
        assert!(info.cwd.starts_with(&made.mount) && info.cwd.ends_with("/src"), "{} in {}", info.cwd, made.mount);
        assert_eq!(std::fs::read_to_string(Path::new(&made.mount).join("src/a.rs"))?, "a\n");
        assert_eq!(std::fs::read_to_string(Path::new(&made.mount).join("target/out"))?, "built\n", "build output comes from the folder");
        // What it changes stays in its volume.
        std::fs::write(Path::new(&made.mount).join("src/a.rs"), "a2\n")?;
        std::fs::write(Path::new(&made.mount).join("src/new.rs"), "n\n")?;
        assert_eq!(std::fs::read_to_string(work.path().join("src/a.rs"))?, "a\n", "the folder is as it was");
        assert!(!work.path().join("src/new.rs").exists());
        let cx = Cx { hub: Arc::clone(&hub), caller: crate::authz::Caller::owner() };
        let fail = |(c, m): crate::ops::Fail| anyhow::anyhow!("{c}: {m}");
        // Its change request comes from its volume: everything that changed there.
        let out = call(cx.clone(), "agents.change", json!({"agent": info.id, "title": "Change a"})).await.map_err(fail)?;
        let branch = get(&out, "/change_request/branch").as_str().unwrap_or_default().to_owned();
        assert_eq!(sh(bare.path(), &format!("git show {branch}:src/a.rs"))?, "a2");
        assert_eq!(sh(bare.path(), &format!("git show {branch}:src/new.rs"))?, "n");
        // A file it changed, changed on disk too: drift.
        std::fs::write(work.path().join("src/a.rs"), "the person's\n")?;
        let ws = call(cx.clone(), "agents.workspace", json!({"agent": info.id})).await.map_err(fail)?;
        assert!(get(&ws, "/drift").as_array().is_some_and(|d| d.iter().any(|x| x.as_str().is_some_and(|s| s.contains("a.rs")))), "{ws}");
        // Writing it back: the plan, with the person's change to a.rs as a conflict, and git's own files left out.
        let plan = call(cx.clone(), "agents.land", json!({"agent": info.id})).await.map_err(fail)?;
        assert_eq!(get(&plan, "/plan/grant_required"), &json!(true), "{plan}");
        assert_eq!(get(&plan, "/excludes"), &json!(["/.git"]), "{plan}");
        // A person's go-ahead can't reach Slates from kiln on Linux yet, and the plan and the write say so.
        assert_eq!(get(&plan, "/can_write"), &json!(false), "{plan}");
        assert!(get(&plan, "/why_not").as_str().is_some_and(|w| w.contains("Linux")), "{plan}");
        let manifest = get(&plan, "/plan/manifest").as_str().unwrap_or_default().to_owned();
        let refused = call(cx.clone(), "agents.land", json!({"agent": info.id, "manifest": manifest})).await.err().map(|(_, m)| m).unwrap_or_default();
        assert!(refused.contains("Linux"), "{refused}");
        // An agent can't write it, whatever it asks.
        let as_agent = Cx { hub: Arc::clone(&hub), caller: crate::mcp::session_caller(&hub, &info.id, None).await.map_err(anyhow::Error::msg)? };
        let refused = call(as_agent, "agents.land", json!({"agent": info.id, "manifest": "00"})).await.err().map(|(c, m)| format!("{c} {m}")).unwrap_or_default();
        assert!(refused.contains("403") && refused.contains("a person's to say"), "{refused}");
        // Ended for good: its volume goes with it.
        assert!(hub.harness.close(&hub, &info.id).await);
        for _ in 0..100 {
            if !super::is_mount(Path::new(&made.mount)) {
                break;
            }
            crate::rt::sleep(Duration::from_millis(50)).await;
        }
        assert!(!super::is_mount(Path::new(&made.mount)), "unmounted");
        Ok(())
    }
}
