//! A machine's side of installed skills (docs/agents.md, "Skills kiln installs"): on a runner, and
//! on the server for its own agent sessions. The server says what's installed and which of this
//! machine's sessions has which ([`Manifest`]); this keeps each file once by its digest, lays each
//! package out once per commit as a Claude Code plugin (`pkgs/<id>-<sha>/`), runs its setup there,
//! checks the tools it needs, and gives each session a folder of the plugins it has
//! (`sessions/<session>/<package>`), which its harness loads at its start (`--plugin-dir`) and
//! again when told (`reload_plugins`). What it says back is each package's state here.
//!
//! Blocking: it reads and writes files, and runs setup.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Seek as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::detect::{MAX_FILE, Setup};

/// The longest a package's setup may take here (a Go or Rust build of a skill's tool).
const SETUP_WITHIN: Duration = Duration::from_secs(30 * 60);
/// How much of a setup's output is kept to show.
const LOG_TAIL: usize = 4096;

/// A file of a package as the server names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRef {
    pub path: String,
    pub sha256: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub exec: bool,
}

/// One installed package, as a machine lays it out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pkg {
    pub id: String,
    pub name: String,
    pub sha: String,
    pub files: Vec<FileRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub setup: Vec<Setup>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub needs: Vec<String>,
}

impl Pkg {
    /// Its folder's name: one per package and commit.
    pub fn key(&self) -> String {
        format!("{}-{}", self.id, self.sha)
    }
}

/// What the server says of this machine: every package, and which of its sessions has which (by
/// package id, narrowest scope first).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub packages: Vec<Pkg>,
    #[serde(default)]
    pub sessions: BTreeMap<String, Vec<String>>,
}

/// A package's state on a machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub id: String,
    pub sha: String,
    /// "fetching", "setting-up", "ready", "needs" or "failed".
    pub state: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    /// Its setup's output, the last of it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub log: String,
}

/// Where a machine keeps it: beside kiln's own skills for its agents (`crate::mcp::plugin_dir`).
pub fn root() -> PathBuf {
    match crate::mcp::kit() {
        Some(k) => k.plugin_dir.parent().map_or_else(|| k.plugin_dir.clone(), Path::to_path_buf).join("skills"),
        None => {
            if cfg!(test) {
                std::env::temp_dir().join(format!("kiln-test-skills-{}", std::process::id()))
            } else {
                crate::config::default_data_dir().join("agents").join("skills")
            }
        }
    }
}

/// The folder of plugins session `id` loads, made if it isn't there.
pub fn session_folder(id: &str) -> Option<PathBuf> {
    let d = Place::new(root(), "").session_dir(id)?;
    std::fs::create_dir_all(&d).ok()?;
    Some(d)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}

/// A digest as a file's name: 64 hex digits, nothing else.
fn digest_ok(d: &str) -> bool {
    d.len() == 64 && d.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// A name as a path's one segment.
fn segment_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= 200 && s != "." && s != ".." && !s.contains(['/', '\\', '\0', ':'])
}

/// A package's path within its folder: relative, no `..`.
fn rel_ok(p: &str) -> bool {
    !p.is_empty() && !p.starts_with('/') && p.split('/').all(segment_ok)
}

/// What one keeper of a machine's skills holds (`owners/<owner>.json`): the server's own sessions'
/// and a runner's share the folder where a server runs a runner beside them, and each lets go only
/// of what neither still holds.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Held {
    pkgs: BTreeSet<String>,
    files: BTreeSet<String>,
    sessions: BTreeSet<String>,
}

pub struct Place {
    root: PathBuf,
    owner: String,
}

impl Place {
    /// The skills under `root` that `owner` keeps ("local": the server's own sessions; "runner").
    pub fn new(root: PathBuf, owner: &str) -> Self {
        Self { root, owner: owner.to_owned() }
    }

    fn held(&self, owner: &str) -> Held {
        std::fs::read(self.root.join("owners").join(format!("{owner}.json"))).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    fn others(&self) -> Vec<Held> {
        let Ok(rd) = std::fs::read_dir(self.root.join("owners")) else { return Vec::new() };
        rd.flatten().filter_map(|e| e.file_name().to_string_lossy().strip_suffix(".json").map(str::to_owned)).filter(|o| *o != self.owner).map(|o| self.held(&o)).collect()
    }

    fn file(&self, digest: &str) -> PathBuf {
        self.root.join("files").join(digest)
    }

    fn pkg_dir(&self, p: &Pkg) -> PathBuf {
        self.root.join("pkgs").join(p.key())
    }

    fn session_dir(&self, id: &str) -> Option<PathBuf> {
        segment_ok(id).then(|| self.root.join("sessions").join(id))
    }

    /// The files the manifest's packages need that this machine hasn't kept, each once.
    pub fn want(&self, m: &Manifest) -> Vec<String> {
        let mut out = BTreeSet::new();
        for p in &m.packages {
            if self.pkg_dir(p).join(".kiln-ready").exists() {
                continue;
            }
            for f in &p.files {
                if digest_ok(&f.sha256) && !self.file(&f.sha256).exists() {
                    out.insert(f.sha256.clone());
                }
            }
        }
        out.into_iter().collect()
    }

    /// A piece of a file the server sends, at `offset`; on the last, kept once its digest holds.
    pub fn receive(&self, digest: &str, offset: u64, data: &[u8], done: bool) -> std::io::Result<()> {
        if !digest_ok(digest) {
            return Err(std::io::Error::other("not a file's digest"));
        }
        if offset.saturating_add(data.len() as u64) > MAX_FILE {
            return Err(std::io::Error::other("past the most a file may be"));
        }
        let dir = self.root.join("files");
        std::fs::create_dir_all(&dir)?;
        let part = dir.join(format!("{digest}.part"));
        let mut f = std::fs::OpenOptions::new().create(true).write(true).truncate(offset == 0).open(&part)?;
        f.seek(std::io::SeekFrom::Start(offset))?;
        f.write_all(data)?;
        drop(f);
        if done {
            let got = hex(&Sha256::digest(std::fs::read(&part)?));
            if got != digest {
                let _ = std::fs::remove_file(&part);
                return Err(std::io::Error::other(format!("its digest is {got}, not {digest}")));
            }
            std::fs::rename(&part, self.file(digest))?;
        }
        Ok(())
    }

    /// Keeps `bytes` (the server's own machine, which reads them from its store), by their digest.
    pub fn keep(&self, digest: &str, bytes: &[u8]) -> std::io::Result<()> {
        self.receive(digest, 0, bytes, true)
    }

    /// Lays package `p` out once its files are all here: in a folder made beside it and renamed into
    /// place, so a harness never loads half of one.
    fn lay_out(&self, p: &Pkg) -> Result<PathBuf, String> {
        let dir = self.pkg_dir(p);
        if dir.join(".kiln-laid").exists() {
            return Ok(dir);
        }
        let tmp = self.root.join("pkgs").join(format!(".{}-{}", p.key(), crate::util::new_id()));
        let made = (|| -> Result<(), String> {
            for f in &p.files {
                if !rel_ok(&f.path) || !digest_ok(&f.sha256) {
                    return Err(format!("{} isn't a path a package may hold", f.path));
                }
                let to = tmp.join(&f.path);
                if let Some(d) = to.parent() {
                    std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
                }
                std::fs::copy(self.file(&f.sha256), &to).map_err(|e| format!("{}: {e}", f.path))?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    let mode = if f.exec { 0o755 } else { 0o644 };
                    std::fs::set_permissions(&to, std::fs::Permissions::from_mode(mode)).map_err(|e| e.to_string())?;
                }
            }
            std::fs::write(tmp.join(".kiln-laid"), &p.sha).map_err(|e| e.to_string())?;
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::rename(&tmp, &dir).map_err(|e| e.to_string())
        })();
        if let Err(e) = made {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(e);
        }
        Ok(dir)
    }

    /// Setup the manifest's packages still need here: each laid out, with the tools it needs, not
    /// yet set up. Each is (its folder, the skill's folder, the command), in the package's order.
    pub fn to_set_up(&self, m: &Manifest) -> Vec<(PathBuf, PathBuf, Setup)> {
        let mut out = Vec::new();
        for p in &m.packages {
            let dir = self.pkg_dir(p);
            if p.setup.is_empty() || !dir.join(".kiln-laid").exists() || dir.join(".kiln-ready").exists() || dir.join(".kiln-failed").exists() || p.needs.iter().any(|t| crate::os::which(t).is_none()) {
                continue;
            }
            for s in &p.setup {
                if segment_ok(&s.skill) {
                    out.push((dir.clone(), dir.join("skills").join(&s.skill), s.clone()));
                }
            }
        }
        out
    }

    /// What a package's setup came to: ready, or failed with why and its output.
    pub fn finish(&self, pkg_dir: &Path, result: &Result<String, String>) {
        let tail = |log: &str| log.char_indices().rev().nth(LOG_TAIL).map_or(log, |(i, _)| log.get(i..).unwrap_or_default()).to_owned();
        let _ = match result {
            Ok(log) => std::fs::write(pkg_dir.join(".kiln-ready"), tail(log)),
            Err(log) => {
                let note = log.lines().rev().find(|l| !l.trim().is_empty() && !l.starts_with("exited")).unwrap_or("setup failed").trim().to_owned();
                std::fs::write(pkg_dir.join(".kiln-failed"), serde_json::json!({"note": note, "log": tail(log)}).to_string())
            }
        };
    }

    /// The manifest's packages laid out and checked here, and each session's folder made to hold
    /// the plugins it has: each package's state, and the sessions whose plugins changed (to be
    /// told to load them again). Setup is [`Place::to_set_up`]'s, run apart. `live`: this
    /// machine's sessions, whose folders stay.
    pub fn apply(&self, m: &Manifest, live: &BTreeSet<String>) -> (Vec<State>, Vec<String>) {
        let mut states = Vec::new();
        let mut ready: BTreeMap<String, PathBuf> = BTreeMap::new();
        for p in &m.packages {
            let state = |state: &str, note: String, log: String| State { id: p.id.clone(), sha: p.sha.clone(), state: state.to_owned(), note, log };
            if p.files.iter().any(|f| !self.file(&f.sha256).exists()) && !self.pkg_dir(p).join(".kiln-laid").exists() {
                states.push(state("fetching", String::new(), String::new()));
                continue;
            }
            let dir = match self.lay_out(p) {
                Ok(d) => d,
                Err(e) => {
                    states.push(state("failed", e, String::new()));
                    continue;
                }
            };
            let missing: Vec<&str> = p.needs.iter().map(String::as_str).filter(|t| crate::os::which(t).is_none()).collect();
            if !missing.is_empty() {
                states.push(state("needs", format!("{} missing", missing.join(", ")), String::new()));
                // What it can do without them, it does: its words are there.
                ready.insert(p.id.clone(), dir);
                continue;
            }
            if p.setup.is_empty() && !dir.join(".kiln-ready").exists() {
                let _ = std::fs::write(dir.join(".kiln-ready"), "");
            }
            if let Ok(log) = std::fs::read_to_string(dir.join(".kiln-ready")) {
                states.push(state("ready", String::new(), log));
                ready.insert(p.id.clone(), dir);
            } else if let Ok(failed) = std::fs::read_to_string(dir.join(".kiln-failed")) {
                let v: serde_json::Value = serde_json::from_str(&failed).unwrap_or_default();
                let field = |k: &str| v.get(k).and_then(serde_json::Value::as_str).unwrap_or_default().to_owned();
                states.push(state("failed", field("note"), field("log")));
            } else {
                states.push(state("setting-up", String::new(), String::new()));
            }
        }
        // Each session's folder: a link to each plugin it has, by the plugin's name.
        let names: BTreeMap<&str, &str> = m.packages.iter().map(|p| (p.id.as_str(), p.name.as_str())).collect();
        let mut changed = Vec::new();
        for (session, ids) in &m.sessions {
            let Some(folder) = self.session_dir(session) else { continue };
            let mut want: BTreeMap<String, PathBuf> = BTreeMap::new();
            for id in ids {
                if let (Some(name), Some(dir)) = (names.get(id.as_str()), ready.get(id)) {
                    want.entry((*name).to_owned()).or_insert_with(|| dir.clone());
                }
            }
            if self.link(&folder, &want) {
                changed.push(session.clone());
            }
        }
        // A live session the manifest gives nothing has an empty folder; an ended one's goes.
        let before = self.held(&self.owner);
        for id in before.sessions.iter().filter(|id| !m.sessions.contains_key(*id)) {
            let Some(folder) = self.session_dir(id) else { continue };
            if live.contains(id) {
                if self.link(&folder, &BTreeMap::new()) {
                    changed.push(id.clone());
                }
            } else {
                let _ = std::fs::remove_dir_all(folder);
            }
        }
        let now = Held {
            pkgs: m.packages.iter().map(Pkg::key).collect(),
            files: m.packages.iter().flat_map(|p| p.files.iter().map(|f| f.sha256.clone())).collect(),
            sessions: m.sessions.keys().cloned().chain(before.sessions.into_iter().filter(|id| live.contains(id))).collect(),
        };
        let _ = std::fs::create_dir_all(self.root.join("owners"));
        let _ = std::fs::write(self.root.join("owners").join(format!("{}.json", self.owner)), serde_json::to_vec(&now).unwrap_or_default());
        // Packages none holds any more, and files none of them has, go.
        let others = self.others();
        let pkgs: BTreeSet<&String> = now.pkgs.iter().chain(others.iter().flat_map(|h| h.pkgs.iter())).collect();
        if let Ok(rd) = std::fs::read_dir(self.root.join("pkgs")) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().into_owned();
                if !pkgs.contains(&n) && !n.starts_with('.') {
                    let _ = std::fs::remove_dir_all(e.path());
                }
            }
        }
        let files: BTreeSet<&String> = now.files.iter().chain(others.iter().flat_map(|h| h.files.iter())).collect();
        if let Ok(rd) = std::fs::read_dir(self.root.join("files")) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().into_owned();
                if digest_ok(&n) && !files.contains(&n) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        (states, changed)
    }

    /// Makes `folder` hold exactly `want` (a link per plugin, by name); whether it changed.
    fn link(&self, folder: &Path, want: &BTreeMap<String, PathBuf>) -> bool {
        let _ = std::fs::create_dir_all(folder);
        let mut changed = false;
        let mut have: BTreeMap<String, PathBuf> = BTreeMap::new();
        if let Ok(rd) = std::fs::read_dir(folder) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let to = std::fs::read_link(e.path()).unwrap_or_else(|_| e.path().join(".kiln-copy"));
                have.insert(name, to);
            }
        }
        for (name, to) in &have {
            if want.get(name) != Some(to) {
                let at = folder.join(name);
                let _ = std::fs::remove_file(&at).or_else(|_| std::fs::remove_dir_all(&at));
                changed = true;
            }
        }
        for (name, dir) in want {
            if have.get(name) == Some(dir) || !segment_ok(name) {
                continue;
            }
            if place_link(dir, &folder.join(name)).is_ok() {
                changed = true;
            }
        }
        changed
    }
}

#[cfg(unix)]
fn place_link(dir: &Path, at: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(dir, at)
}

/// Windows: a directory junction needs no privilege a symbolic link does.
#[cfg(windows)]
fn place_link(dir: &Path, at: &Path) -> std::io::Result<()> {
    let out = std::process::Command::new("cmd").args(["/C", "mklink", "/J"]).arg(at).arg(dir).output()?;
    if out.status.success() { Ok(()) } else { Err(std::io::Error::other(String::from_utf8_lossy(&out.stderr).into_owned())) }
}

/// Who keeps a machine's skills, and how its sessions and the server hear of them: a runner (its
/// own sessions, its link to the server) or the server for its own sessions.
pub trait Keeper: Send + Sync + 'static {
    /// "runner" or "local" ([`Held`]).
    fn owner(&self) -> &'static str;
    /// This machine's sessions now.
    fn live(&self) -> BTreeSet<String>;
    /// Tells sessions whose plugins changed to load them again.
    fn reload(&self, sessions: &[String]);
    /// Each package's state here, to the server.
    fn report(&self, states: Vec<State>) -> futures::future::BoxFuture<'static, ()>;
}

/// A keeper's manifest: the last it settled, the next to, and whether it's at it.
#[derive(Default)]
struct Slot {
    last: Option<Manifest>,
    next: Option<Manifest>,
    busy: bool,
}

static SLOTS: parking_lot::Mutex<BTreeMap<&'static str, Slot>> = parking_lot::Mutex::new(BTreeMap::new());

/// The manifest `owner` last settled.
pub fn last(owner: &'static str) -> Option<Manifest> {
    SLOTS.lock().get(owner).and_then(|s| s.next.clone().or_else(|| s.last.clone()))
}

/// Settles manifest `m` on this machine for `k`: lays its packages out, links each session's,
/// reports, then runs the setup still to run, a package's commands in turn, and reports again. One
/// at a time per keeper: a manifest heard meanwhile is settled next, the latest only.
pub async fn settle(m: Manifest, k: std::sync::Arc<dyn Keeper>) {
    let owner = k.owner();
    {
        let mut slots = SLOTS.lock();
        let slot = slots.entry(owner).or_default();
        slot.next = Some(m);
        if slot.busy {
            return;
        }
        slot.busy = true;
    }
    loop {
        let m = {
            let mut slots = SLOTS.lock();
            let slot = slots.entry(owner).or_default();
            match slot.next.take() {
                Some(m) => {
                    slot.last = Some(m.clone());
                    m
                }
                None => {
                    slot.busy = false;
                    return;
                }
            }
        };
        let live = k.live();
        let (applied, jobs) = {
            let m = m.clone();
            crate::rt::blocking(move || {
                let place = Place::new(root(), owner);
                let applied = place.apply(&m, &live);
                (applied, place.to_set_up(&m))
            })
            .await
            .unwrap_or_default()
        };
        let (states, changed) = applied;
        k.reload(&changed);
        k.report(states).await;
        if jobs.is_empty() {
            continue;
        }
        // Each package's commands in turn; the first that fails ends its setup.
        let mut i = 0;
        while let Some((pkg, _, _)) = jobs.get(i) {
            let pkg = pkg.clone();
            let mut log = String::new();
            let mut result = Ok(());
            while let Some((p, dir, s)) = jobs.get(i) {
                if *p != pkg {
                    break;
                }
                i += 1;
                if result.is_err() {
                    continue;
                }
                match run_setup(dir.clone(), s.clone()).await {
                    Ok(out) => log.push_str(&out),
                    Err(e) => {
                        log.push_str(&e);
                        result = Err(());
                    }
                }
            }
            let done = if result.is_ok() { Ok(log) } else { Err(log) };
            let _ = crate::rt::blocking(move || Place::new(root(), owner).finish(&pkg, &done)).await;
        }
        // Settled again with what setup came to, unless a newer manifest came meanwhile.
        let mut slots = SLOTS.lock();
        let slot = slots.entry(owner).or_default();
        if slot.next.is_none() {
            slot.next = Some(m);
        }
    }
}

/// Runs a package's setup command in its skill's folder: through the platform's shell, at a lower
/// priority than what's at work here, its output captured.
pub async fn run_setup(dir: PathBuf, s: Setup) -> Result<String, String> {
    let mut cmd = if cfg!(windows) {
        let mut c = std::process::Command::new("cmd");
        c.arg("/C").arg(&s.command);
        c
    } else if crate::os::which("nice").is_some() {
        let mut c = std::process::Command::new("nice");
        c.args(["-n", "10", "sh", "-c", &s.command]);
        c
    } else {
        let mut c = std::process::Command::new("sh");
        c.arg("-c").arg(&s.command);
        c
    };
    cmd.current_dir(&dir);
    let out = crate::proc::output(cmd, SETUP_WITHIN).await.map_err(|e| format!("{e:#}"))?;
    let text = format!("$ {}\n{}{}", s.command, String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if out.status.success() { Ok(text) } else { Err(format!("{text}\nexited {}", out.status)) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(b: &[u8]) -> String {
        hex(&Sha256::digest(b))
    }

    #[test]
    fn files_once_packages_laid_out_sessions_linked() -> anyhow::Result<()> {
        let d = tempfile::tempdir()?;
        let place = Place::new(d.path().to_path_buf(), "runner");
        let skill = b"---\nname: how\n---\n".to_vec();
        let manifest_json = b"{\"name\":\"pstack\"}".to_vec();
        let p = Pkg {
            id: "p1".into(),
            name: "pstack".into(),
            sha: "8ecfb19".into(),
            files: vec![
                FileRef { path: ".claude-plugin/plugin.json".into(), sha256: digest(&manifest_json), size: 17, exec: false },
                FileRef { path: "skills/how/SKILL.md".into(), sha256: digest(&skill), size: 18, exec: false },
            ],
            setup: Vec::new(),
            needs: Vec::new(),
        };
        let m = Manifest { packages: vec![p.clone()], sessions: BTreeMap::from([("s1".to_owned(), vec!["p1".to_owned()])]) };
        assert_eq!(place.want(&m).len(), 2);
        let live = BTreeSet::from(["s1".to_owned()]);
        assert_eq!(place.apply(&m, &live).0.first().map(|s| s.state.as_str()), Some("fetching"));
        // A file in two pieces; one whose bytes aren't its digest's is refused.
        let half = skill.len() / 2;
        place.receive(&digest(&skill), 0, skill.get(..half).unwrap_or_default(), false)?;
        place.receive(&digest(&skill), half as u64, skill.get(half..).unwrap_or_default(), true)?;
        assert!(place.receive(&digest(b"other"), 0, b"not it", true).is_err());
        place.keep(&digest(&manifest_json), &manifest_json)?;
        assert!(place.want(&m).is_empty());
        let (states, changed) = place.apply(&m, &live);
        assert_eq!(states.first().map(|s| s.state.as_str()), Some("ready"));
        assert_eq!(changed, ["s1"]);
        let linked = d.path().join("sessions/s1/pstack/skills/how/SKILL.md");
        assert_eq!(std::fs::read(&linked)?, skill);
        // Again: nothing changes. Taken from the session: its folder empties; uninstalled: gone.
        assert!(place.apply(&m, &live).1.is_empty());
        let none = Manifest { packages: vec![p], sessions: BTreeMap::new() };
        assert_eq!(place.apply(&none, &live).1, ["s1"]);
        assert!(!linked.exists() && d.path().join("sessions/s1").is_dir());
        // Another keeper of this machine's skills (the server's own sessions beside its runner)
        // holding it: it stays until neither does.
        let local = Place::new(d.path().to_path_buf(), "local");
        local.apply(&Manifest { packages: none.packages.clone(), sessions: BTreeMap::new() }, &BTreeSet::new());
        place.apply(&Manifest::default(), &BTreeSet::new());
        assert!(!d.path().join("sessions/s1").exists() && d.path().join("pkgs/p1-8ecfb19").is_dir());
        local.apply(&Manifest::default(), &BTreeSet::new());
        assert!(std::fs::read_dir(d.path().join("pkgs"))?.next().is_none());
        assert!(std::fs::read_dir(d.path().join("files"))?.next().is_none());
        Ok(())
    }

    #[test]
    fn setup_runs_once_and_a_failure_says_why() -> anyhow::Result<()> {
        let d = tempfile::tempdir()?;
        let place = Place::new(d.path().to_path_buf(), "runner");
        let md = b"# s".to_vec();
        place.keep(&digest(&md), &md)?;
        let p = Pkg {
            id: "p".into(),
            name: "perf".into(),
            sha: "1".into(),
            files: vec![FileRef { path: "skills/bench/SKILL.md".into(), sha256: digest(&md), size: 3, exec: false }],
            setup: vec![Setup { skill: "bench".into(), stack: "Python".into(), command: "uv sync".into() }],
            needs: Vec::new(),
        };
        let m = Manifest { packages: vec![p.clone()], sessions: BTreeMap::new() };
        assert_eq!(place.apply(&m, &BTreeSet::new()).0.first().map(|s| s.state.clone()), Some("setting-up".into()));
        let jobs = place.to_set_up(&m);
        assert_eq!(jobs.len(), 1);
        let (pkg, at, _) = jobs.first().cloned().ok_or_else(|| anyhow::anyhow!("no setup"))?;
        assert!(at.ends_with("skills/bench"));
        place.finish(&pkg, &Err("$ uv sync\nerror: no Python\nexited 2".into()));
        assert_eq!(place.apply(&m, &BTreeSet::new()).0.first().map(|s| (s.state.clone(), s.note.clone())), Some(("failed".into(), "error: no Python".into())));
        assert!(place.to_set_up(&m).is_empty(), "a failed setup isn't run again on its own");
        // A new commit sets up anew, once.
        let next = Manifest { packages: vec![Pkg { sha: "2".into(), ..p.clone() }], sessions: BTreeMap::new() };
        place.apply(&next, &BTreeSet::new());
        let jobs = place.to_set_up(&next);
        let (pkg, _, _) = jobs.first().cloned().ok_or_else(|| anyhow::anyhow!("no setup"))?;
        place.finish(&pkg, &Ok("$ uv sync\nResolved 3 packages".into()));
        assert!(place.apply(&next, &BTreeSet::new()).0.first().is_some_and(|s| s.state == "ready" && s.log.contains("Resolved")));
        assert!(place.to_set_up(&next).is_empty());
        // A tool it needs that the machine hasn't: its words are there, and it says what's missing.
        let needy = Manifest { packages: vec![Pkg { needs: vec!["kiln-no-such-tool".into()], sha: "3".into(), ..p }], sessions: BTreeMap::new() };
        assert_eq!(place.apply(&needy, &BTreeSet::new()).0.first().map(|s| (s.state.clone(), s.note.clone())), Some(("needs".into(), "kiln-no-such-tool missing".into())));
        // A path out of its folder is refused.
        let bad = Pkg { id: "b".into(), name: "b".into(), sha: "1".into(), files: vec![FileRef { path: "../x".into(), sha256: digest(&md), size: 3, exec: false }], setup: Vec::new(), needs: Vec::new() };
        assert_eq!(place.apply(&Manifest { packages: vec![bad], sessions: BTreeMap::new() }, &BTreeSet::new()).0.first().map(|s| s.state.clone()), Some("failed".into()));
        Ok(())
    }
}
