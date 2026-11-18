//! Agents started through kiln work on Slates volumes on both of kiln's engines, and their work reaches the project's
//! folder on disk only through Slates, by the landing permission each agent's person gave it: its mode (Slates' goal
//! g266, kiln goal 28e73c34ff3d898696d8210ef3b9fa94; Ada 2026-11-18: permission follows the agent's mode, with no keys
//! or proofs; docs/agentic.md W1–W8; kiln plan T4, CP-T4.4).
//!
//! Each drive starts two agents through kiln's REST API, the stand-in harness behind each (`tests/fixtures/vm-stub/
//! harness/stub`: no model), each in a workspace of its own (`"workspace": {"kind": "volume"}`) over a git project of its
//! own with a bare remote: one in Claude Code's mode that asks (`default`), then one in its mode that asks nothing
//! (`bypassPermissions`) ([`Mode`]). Each one's first turn writes a new file, appends a line to a tracked one and removes
//! another, then asks kiln itself, as the agent, to write its work to disk (the stand-in's `LAND`). What each drive
//! reads, all of it what a person or an agent could observe, each step said (`PROVEN: ...`) as it holds, so a drive that
//! fails shows how far the rest went:
//!
//! 1. **Slates through its library alone, at kiln's pin.** The runner finds no `slates` executable (its PATH holds none
//!    and `KILN_SLATES` is empty), so a workspace made at all was made without Slates' command line; no `slates` process
//!    of the drive's own instance runs at any of its looks ([`cli_of_instance`]); and the runner's facts name the Slates
//!    it links as the revision this checkout's `vendor/MANIFEST.toml` pins ([`links_the_pin`]).
//! 2. **The volume.** kiln's word on the session names its volume over the project ([`workspace_of`]). On the host
//!    (`bare_metal`), its mount is in the host's table as Slates' (NFS on loopback on macOS, FUSE on Linux; WinFsp keeps
//!    no table to read) and the harness works in it. In a microVM (`microvm`), no host mount: kiln names none, the host's
//!    table gains no Slates mount of the session's at any look ([`session_mounts`]), and the harness runs in a guest's
//!    kernel at its project's guest path.
//! 3. **Isolation.** Nothing the harness did is in the project's folder before its landing writes it, byte for byte
//!    ([`tree`]).
//! 4. **Landing by the agent's mode** (the contract's draft 3). The agent that asks nothing: kiln answers its own landing
//!    `written` ([`check_turn`]), and its folder holds exactly its changes ([`expected`]) with no person's act. The agent
//!    that asks: kiln answers its own landing with Slates' typed approval request and refuses its own approval of it, its
//!    folder untouched; its change request (`agents.change`) holds exactly its changes; a person is shown the same
//!    request ([`approval_manifest`]) and approves it, and only then does its folder hold exactly its changes. git's HEAD
//!    and index stay as they were throughout.
//! 5. **The end.** Ended for good, the session's mount goes (bare metal).
//!
//! What it needs, and who owes it (`~/.cache/slates-scratch/fanout/g266-e2e-contract.md`, draft 3): Slates' library API
//! and its typed landing outcomes (g262) and the daemon's landing policy (A-285), vendored in kiln and driven by kiln's
//! workspace code instead of the CLI (g263), the bare-metal engine on it, landing by each agent's mode (g264), and the
//! microVM engine handing each guest its volume over Slates' own virtio-fs route by a descriptor (g265). Until a runner
//! gives workspaces with no Slates binary, the drives skip in its own words (`why_not`).
//!
//! - `checks`: the pure checks, against samples (any OS).
//! - `stand_in`: the stand-in's words for a project's files, for the mode it runs in and for its own landing, driven as a
//!   process against a kiln MCP server of the test's own (Unix: python3).
//! - `bare_metal`: the drive on the runner's host (Unix: the stand-in is a Python script run by its name), and drift.
//! - `microvm`: the drive in a Shards microVM of its own, booting the stand-in's Agentfile from kiln's repository at this
//!   checkout's commit (which must be on GitHub, as `vm_harnesses`' stand-in images' is).
//!
//! `SLATES_E2E=1` makes a runner without workspaces a failure rather than a skip; `VM_E2E=1` does so for a runner without
//! microVMs (tests/vm_e2e/README.md). Each drive owns a Slates instance of its own ([`Instance`]) and ends exactly its
//! processes, its mounts unmounted first.
//!
//!   SLATES_E2E=1 cargo test -p kiln --test slates_e2e -- --nocapture --test-threads 1

#![allow(dead_code, reason = "the drives are Unix's until the stand-in runs by its name on Windows' hosts here; the checks are every OS's")]

mod common;
mod vm_e2e;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use vm_e2e::HostProc;

/// The tracked file the stand-in appends a line to, and what it holds before.
pub const APPENDED: (&str, &str) = ("README.md", "the project\n");

/// The tracked file the stand-in removes, and what it holds before.
pub const REMOVED: (&str, &str) = ("gone.txt", "to be removed\n");

/// What the project's git ignores (build output), and the rule that says so.
pub const IGNORED: (&str, &str) = (".gitignore", "target/\n");

/// Slates' command line, by its program's name: what kiln never runs.
pub const CLI: &str = "slates";

/// The switch that makes a runner without workspaces a failure rather than a skip.
pub const REQUIRED: &str = "SLATES_E2E";

/// How long one of kiln's operations on an agent's workspace may take to answer: a minute past the server's own wait
/// on its runner's answer about a folder (`kiln::workspace::WAIT`; a push can be slow).
pub const OP_WITHIN: Duration = Duration::from_secs(kiln::workspace::WAIT.as_secs().saturating_add(60));

/// How long a drive's own Slates instance has to stop on its own when asked (SIGTERM) before it's killed: a Slates anchor
/// stops its daemon within the daemon's liveness budget, a few seconds on a loaded machine.
pub const STOP_GRACE: Duration = Duration::from_secs(30);

/// How often a drive's cleanup looks again at what it waits on (a mount going, its instance's processes ending).
pub const LOOK_EVERY: Duration = Duration::from_millis(200);

/// A name of the drive's own: its process and the time, so two drives, here or on another runner of the machine, never
/// share one.
pub fn nonce() -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("{}x{nanos:x}", std::process::id())
}

/// `path` with its links resolved where it can be (macOS's `/var` is `/private/var`), without the verbatim prefix
/// canonicalizing adds on Windows: as the host's mount table and a harness name it.
pub fn canonical(path: &Path) -> PathBuf {
    let full = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let text = full.to_string_lossy().into_owned();
    text.strip_prefix(r"\\?\").map_or(full, PathBuf::from)
}

// =================================================================================================
// The host's mount table: Slates' mounts, and a session's among them
// =================================================================================================

/// A mount of the host's table: where it is, its type, and its source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub at: String,
    pub kind: String,
    pub source: String,
}

/// A field of `/proc/self/mountinfo` as its words are: the kernel writes a space, a tab, a newline and a backslash in one
/// as octal escapes (proc(5)).
fn unescape(field: &str) -> String {
    field.replace("\\040", " ").replace("\\011", "\t").replace("\\012", "\n").replace("\\134", "\\")
}

/// The mounts a Linux `/proc/self/mountinfo` lists (proc(5)): each line's mount point, its fifth field, and after its
/// ` - ` separator its file system's type and its source.
pub fn mountinfo(text: &str) -> Vec<Mount> {
    text.lines()
        .filter_map(|line| {
            let (head, tail) = line.split_once(" - ")?;
            let at = head.split(' ').nth(4)?;
            let mut rest = tail.split(' ');
            let kind = rest.next()?;
            let source = rest.next()?;
            Some(Mount { at: unescape(at), kind: kind.to_owned(), source: unescape(source) })
        })
        .collect()
}

/// The mounts BSD's `mount` lists, macOS's among them: `<source> on <mount point> (<type>, <options>…)`.
pub fn bsd_mounts(text: &str) -> Vec<Mount> {
    text.lines()
        .filter_map(|line| {
            let (source, rest) = line.split_once(" on ")?;
            let (at, options) = rest.rsplit_once(" (")?;
            let kind = options.split([',', ')']).next()?.trim();
            Some(Mount { at: at.to_owned(), kind: kind.to_owned(), source: source.to_owned() })
        })
        .collect()
}

/// Whether `m` is a Slates mount: its type is `fuse.slates` (Linux: the `slates` subtype), or its source is one of
/// Slates' (`slates:/<name>`, the macOS loopback's; `slates:<attachment>`, a FUSE mount's).
pub fn is_slates(m: &Mount) -> bool {
    m.kind == "fuse.slates" || m.source.starts_with("slates:")
}

/// The host's mounts now: Linux's `/proc/self/mountinfo`, BSD `mount`'s list elsewhere on Unix. None on Windows, which
/// keeps no table of WinFsp's mounts to read.
pub fn host_mounts() -> anyhow::Result<Vec<Mount>> {
    if cfg!(target_os = "linux") {
        return Ok(mountinfo(&std::fs::read_to_string("/proc/self/mountinfo")?));
    }
    if cfg!(unix) {
        let out = std::process::Command::new("mount").output()?;
        anyhow::ensure!(out.status.success(), "mount: {}", String::from_utf8_lossy(&out.stderr));
        return Ok(bsd_mounts(&String::from_utf8_lossy(&out.stdout)));
    }
    Ok(Vec::new())
}

/// The Slates mounts among `mounts` that are a session's whose volume is `made`: under the drive's own folder `dir`
/// (where its runner keeps its state, and its workspaces with it), or naming the volume or its name in their source,
/// wherever they are. A mount of the person's own, or of another drive, is neither.
pub fn session_mounts(mounts: &[Mount], dir: &Path, made: &Made) -> Vec<Mount> {
    mounts
        .iter()
        .filter(|m| is_slates(m))
        .filter(|m| Path::new(&m.at).starts_with(dir) || (!made.volume.is_empty() && m.source.contains(&made.volume)) || (!made.name.is_empty() && m.source.contains(&made.name)))
        .cloned()
        .collect()
}

/// Unmounts the mount at `at`, as its user may: `umount` (and forced, where that's refused) on macOS, `fusermount3 -u`
/// (and lazily, where that's refused) on Linux. Whether it went is read from the table after, not from these.
fn unmount(at: &str) {
    let tries: Vec<Vec<&str>> = if cfg!(target_os = "linux") { vec![vec!["fusermount3", "-u"], vec!["fusermount3", "-u", "-z"]] } else { vec![vec!["umount"], vec!["umount", "-f"]] };
    for t in &tries {
        let Some((program, args)) = t.split_first() else { continue };
        if std::process::Command::new(program).args(args).arg(at).output().is_ok_and(|o| o.status.success()) {
            return;
        }
    }
}

/// Every Slates mount under `dir` unmounted ([`unmount`]): those still there after. Where the table can't be read, a
/// mount stands for it, so nothing below a mount is stopped unseen.
pub fn unmount_under(dir: &Path) -> Vec<Mount> {
    let ours = |all: Vec<Mount>| -> Vec<Mount> { all.into_iter().filter(|m| is_slates(m) && Path::new(&m.at).starts_with(dir)).collect() };
    let unread = |e: anyhow::Error| vec![Mount { at: format!("(the mount table couldn't be read: {e:#})"), kind: String::new(), source: String::new() }];
    match host_mounts() {
        Ok(all) => ours(all).iter().for_each(|m| unmount(&m.at)),
        Err(e) => return unread(e),
    }
    host_mounts().map(ours).unwrap_or_else(unread)
}

// =================================================================================================
// The drive's own Slates instance, its processes and their ends
// =================================================================================================

/// A program's name as a command line names it: its file's, lower-cased, without Windows' `.exe`.
fn program(word: &str) -> String {
    let file = word.rsplit(['/', '\\']).next().unwrap_or(word).to_ascii_lowercase();
    file.strip_suffix(".exe").map_or_else(|| file.clone(), str::to_owned)
}

/// Whether `argv` names Slates instance `name` (`--instance <name>`, or `--instance=<name>`).
fn names_instance(argv: &[String], name: &str) -> bool {
    argv.windows(2).any(|w| matches!(w, [flag, value] if flag == "--instance" && value == name)) || argv.iter().any(|a| a.strip_prefix("--instance=") == Some(name))
}

/// The processes among `procs` of Slates instance `name`, however kiln started them: each whose arguments name it.
pub fn of_instance(procs: &[HostProc], name: &str) -> Vec<HostProc> {
    procs.iter().filter(|p| names_instance(&p.argv, name)).cloned().collect()
}

/// Those of them that are Slates' command line itself: a program called [`CLI`]. kiln drives Slates through its library
/// alone, so none is ever one of the drive's instance.
pub fn cli_of_instance(procs: &[HostProc], name: &str) -> Vec<HostProc> {
    of_instance(procs, name).into_iter().filter(|p| p.argv.first().is_some_and(|w| program(w) == CLI)).collect()
}

/// The folders of `path` (a PATH's value) holding [`CLI`] as kiln finds a program (`kiln::os::which_in`).
pub fn holding_cli(path: &str) -> Vec<PathBuf> {
    std::env::split_paths(path).filter(|d| !d.as_os_str().is_empty() && kiln::os::which_in(CLI, d.as_os_str()).is_some()).collect()
}

/// A Slates instance of a drive's own, named for it alone, and the drive's folder. Dropped, every Slates mount under
/// that folder is unmounted, then, with none left, every process naming the instance is ended: asked to stop, then
/// killed after [`STOP_GRACE`]. Unmounted first: a hard NFS mount over a stopped server never gives up, and on macOS
/// a daemon killed under a live kernel NFS mount panicked the Mac twice (Slates' record of 2026-10-06), so with a mount
/// left nothing is ended, and it says so.
pub struct Instance {
    pub name: String,
    dir: PathBuf,
}

impl Instance {
    pub fn new(dir: &Path) -> Instance {
        Instance { name: format!("kilne2e{}", nonce()), dir: canonical(dir) }
    }

    /// The runner's environment that points kiln's workspaces at this instance, started by the runner itself rather
    /// than as a unit of the user's systemd, and leaves it no Slates binary of `KILN_SLATES`'s.
    pub fn env(&self) -> Vec<(&'static str, String)> {
        vec![("KILN_SLATES_INSTANCE", self.name.clone()), ("KILN_SLATES_DIRECT", "1".to_owned()), ("KILN_SLATES", String::new())]
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        let left = unmount_under(&self.dir);
        if !left.is_empty() {
            eprintln!("LEFT RUNNING: Slates instance {}: these of its mounts wouldn't go, so its server stays up beneath them: {left:?}", self.name);
            return;
        }
        end_instance(&self.name);
    }
}

/// Every process of instance `name` ended: asked to stop (SIGTERM), then killed after [`STOP_GRACE`]; on Windows
/// ended at once with its tree.
fn end_instance(name: &str) {
    let pids = |procs: &[HostProc]| -> Vec<u32> { of_instance(procs, name).iter().map(|p| p.pid).collect() };
    let Ok(first) = vm_e2e::host_processes().map(|p| pids(&p)) else {
        eprintln!("LEFT RUNNING: Slates instance {name}: the host's processes couldn't be listed");
        return;
    };
    #[cfg(unix)]
    {
        use nix::sys::signal::{Signal, kill};
        use nix::unistd::Pid;
        let send = |pid: u32, s: Signal| {
            if let Ok(raw) = i32::try_from(pid) {
                let _ = kill(Pid::from_raw(raw), s);
            }
        };
        first.iter().for_each(|p| send(*p, Signal::SIGTERM));
        let until = Instant::now() + STOP_GRACE;
        while first.iter().any(|p| kiln::os::pid_alive(*p)) && Instant::now() < until {
            std::thread::sleep(LOOK_EVERY);
        }
        first.iter().filter(|p| kiln::os::pid_alive(**p)).for_each(|p| send(*p, Signal::SIGKILL));
    }
    #[cfg(windows)]
    for pid in &first {
        let _ = std::process::Command::new("taskkill").args(["/PID", &pid.to_string(), "/T", "/F"]).output();
    }
}

// =================================================================================================
// The project: a git repository of the drive's own with a bare remote
// =================================================================================================

/// A git project of a drive's own: its folder (canonical, as a harness says its folder), its bare remote, and the empty
/// configuration its git commands read instead of a person's or the system's.
pub struct Project {
    pub dir: String,
    pub remote: PathBuf,
    config: PathBuf,
}

impl Project {
    /// A repository on `main` under `dir` holding [`APPENDED`], [`REMOVED`] and [`IGNORED`], committed and pushed to a
    /// bare remote of its own (`origin`).
    pub fn new(dir: &Path) -> anyhow::Result<Project> {
        let (folder, remote, config) = (dir.join("project"), dir.join("remote.git"), dir.join("gitconfig"));
        std::fs::create_dir_all(&folder)?;
        std::fs::create_dir_all(&remote)?;
        std::fs::write(&config, "")?;
        let p = Project { dir: canonical(&folder).to_string_lossy().into_owned(), remote, config };
        p.git(&p.remote, &["init", "-q", "--bare", "-b", "main"])?;
        let here = PathBuf::from(&p.dir);
        p.git(&here, &["init", "-q", "-b", "main"])?;
        for (k, v) in [("user.name", "Ada"), ("user.email", "ada@example.com"), ("commit.gpgsign", "false"), ("tag.gpgsign", "false"), ("core.autocrlf", "false")] {
            p.git(&here, &["config", k, v])?;
        }
        for (name, text) in [APPENDED, REMOVED, IGNORED] {
            std::fs::write(here.join(name), text)?;
        }
        p.git(&here, &["add", "."])?;
        p.git(&here, &["commit", "-q", "-m", "the project"])?;
        let remote = p.remote.to_string_lossy().into_owned();
        p.git(&here, &["remote", "add", "origin", &remote])?;
        p.git(&here, &["push", "-q", "origin", "main"])?;
        Ok(p)
    }

    /// `git <args>` in `dir`: what it printed, or why it failed.
    pub fn git(&self, dir: &Path, args: &[&str]) -> anyhow::Result<String> {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", &self.config)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()?;
        anyhow::ensure!(out.status.success(), "git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    pub fn path(&self) -> PathBuf {
        PathBuf::from(&self.dir)
    }

    /// Its repository's HEAD and its index's bytes: what a landing, which leaves git's own folder out, never changes.
    pub fn git_state(&self) -> anyhow::Result<(String, Vec<u8>)> {
        Ok((self.git(&self.path(), &["rev-parse", "HEAD"])?, std::fs::read(self.path().join(".git").join("index"))?))
    }

    /// The files of `rev` in its remote, by path, with their bytes.
    pub fn tree_at(&self, rev: &str) -> anyhow::Result<BTreeMap<String, Vec<u8>>> {
        let names = self.git(&self.remote, &["ls-tree", "-r", "-z", "--name-only", rev])?;
        let mut files = BTreeMap::new();
        for name in names.split('\0').filter(|n| !n.is_empty()) {
            let out = std::process::Command::new("git").arg("-C").arg(&self.remote).args(["cat-file", "blob", &format!("{rev}:{name}")]).env("GIT_CONFIG_GLOBAL", &self.config).env("GIT_CONFIG_NOSYSTEM", "1").output()?;
            anyhow::ensure!(out.status.success(), "{rev}:{name}: {}", String::from_utf8_lossy(&out.stderr).trim());
            files.insert(name.to_owned(), out.stdout);
        }
        Ok(files)
    }
}

/// Every file under `dir` but git's own folder (`.git`), by its path from `dir` (`/` between names), with its bytes.
pub fn tree(dir: &Path) -> anyhow::Result<BTreeMap<String, Vec<u8>>> {
    fn walk(base: &Path, at: &Path, out: &mut BTreeMap<String, Vec<u8>>) -> anyhow::Result<()> {
        for entry in std::fs::read_dir(at)? {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            if kind.is_dir() {
                if at == base && entry.file_name() == ".git" {
                    continue;
                }
                walk(base, &path, out)?;
            } else {
                let name = path.strip_prefix(base)?.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>().join("/");
                out.insert(name, std::fs::read(&path)?);
            }
        }
        Ok(())
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out)?;
    Ok(out)
}

/// What changed from `before` to `after`: each path whose bytes differ, with its bytes after (none: gone).
pub fn changes(before: &BTreeMap<String, Vec<u8>>, after: &BTreeMap<String, Vec<u8>>) -> BTreeMap<String, Option<Vec<u8>>> {
    let mut out = BTreeMap::new();
    for (name, bytes) in after {
        if before.get(name) != Some(bytes) {
            out.insert(name.clone(), Some(bytes.clone()));
        }
    }
    for name in before.keys().filter(|n| !after.contains_key(*n)) {
        out.insert(name.clone(), None);
    }
    out
}

/// The stand-in's first turn in `mode` ([`check_turn`] reads what it says): `probe` written in its project, a line
/// appended to [`APPENDED`], [`REMOVED`] removed, then its own write to disk asked of kiln; where its mode asks, its
/// own mode raised to one that asks nothing is asked of kiln first, which must leave its landing held all the same.
pub fn prompt(probe: &str, mode: Mode) -> String {
    let raise = if mode == Mode::Ask { "RAISE " } else { "" };
    format!("WRITE={probe} APPEND={} REMOVE={} {raise}LAND do the task", APPENDED.0, REMOVED.0)
}

/// The changes [`prompt`]'s turn makes, file by file: `probe` with its name as its words, [`APPENDED`] with the
/// stand-in's line after what it held, and [`REMOVED`] gone.
pub fn expected(probe: &str) -> BTreeMap<String, Option<Vec<u8>>> {
    BTreeMap::from([
        (probe.to_owned(), Some(format!("{probe}\n").into_bytes())),
        (APPENDED.0.to_owned(), Some(format!("{}APPENDED {}\n", APPENDED.1, APPENDED.0).into_bytes())),
        (REMOVED.0.to_owned(), None),
    ])
}

/// The landing permission a drive's agent runs with: the mode its person gave it, which kiln, its launcher, passes to
/// Slates as its session's landing policy, and which the agent can never raise (Ada 2026-11-18).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// A mode that asks nothing: its landing is written at once.
    Bypass,
    /// A mode that asks: its landing is Slates' typed approval request, written once a person approves it.
    Ask,
}

impl Mode {
    /// Both, in the order a drive starts them: the agent that asks first, since its review needs no landing, so a drive
    /// on a kiln whose landing falls short still shows how far the rest goes.
    pub const ALL: [Mode; 2] = [Mode::Ask, Mode::Bypass];

    /// The mode as Claude Code names it (`--permission-mode`), as a session's spec gives it to kiln.
    pub fn claude(self) -> &'static str {
        match self {
            Mode::Bypass => "bypassPermissions",
            Mode::Ask => "default",
        }
    }

    /// A word for it in a folder's name.
    pub fn word(self) -> &'static str {
        match self {
            Mode::Bypass => "bypass",
            Mode::Ask => "ask",
        }
    }
}

/// Whether `word` is a landing's manifest as kiln writes it: the 32 bytes of the BLAKE3 hash an approval binds, in
/// hexadecimal.
pub fn is_manifest(word: &str) -> bool {
    word.len() == 64 && word.chars().all(|c| c.is_ascii_hexdigit())
}

/// The paths kiln's word on a workspace (`agents.workspace`) lists as drifted: each entry of its `drift` as Slates
/// words it, a line (`/README.md (Modified)`) or a typed drift (`{"path": "/README.md", "kind": ...}`, slates-api's
/// `Drift`).
pub fn drifted(workspace: &Value) -> Vec<String> {
    let entries = workspace.get("drift").and_then(Value::as_array).into_iter().flatten();
    entries.filter_map(|d| d.as_str().or_else(|| d.get("path").and_then(Value::as_str))).map(str::to_owned).collect()
}

/// The manifest of the approval request kiln answered the agent's own landing with, as the stand-in said it
/// (`LAND_APPROVAL <manifest>`).
pub fn approval_said(lines: &[String]) -> Option<String> {
    lines.iter().find_map(|l| l.strip_prefix("LAND_APPROVAL ")).map(str::trim).filter(|m| is_manifest(m)).map(str::to_owned)
}

/// The stand-in's turn made the three changes [`prompt`] asked for, as it said: `probe` written, [`APPENDED`] appended
/// to, [`REMOVED`] removed.
pub fn check_edits(lines: &[String], probe: &str) -> Result<(), String> {
    for want in [format!("WROTE {probe}"), format!("APPENDED {}", APPENDED.0), format!("REMOVED {}", REMOVED.0)] {
        if !lines.contains(&want) {
            return Err(format!("the harness never said {want:?}: {lines:?}"));
        }
    }
    Ok(())
}

/// What the stand-in's turn said of its own landing, as kiln answered it by its `mode` (docs/agentic.md W6): written
/// at once where it asks nothing; where it asks, its ask to raise its own mode answered by kiln (refused, or taken
/// without effect on its landing), then its landing held as an approval request naming its manifest, and its own
/// approval of that refused. Never failed for want of a kiln to ask, never refused outright (neither mode forbids
/// writing).
pub fn check_landing(lines: &[String], mode: Mode) -> Result<(), String> {
    let said = |tag: &str| lines.iter().find(|l| l.starts_with(tag));
    for (tag, what) in [("LAND_FAIL ", "its own landing reached no answer"), ("LAND_REFUSED ", "its own landing was refused"), ("LAND_CONFLICTED ", "its own landing met conflicts")] {
        if let Some(l) = said(tag) {
            return Err(format!("{what} ({mode:?}): {l}"));
        }
    }
    match mode {
        Mode::Bypass => {
            if let Some(held) = said("LAND_APPROVAL ") {
                return Err(format!("an agent whose mode asks nothing was held for a person's approval: {held}"));
            }
            said("LAND_WRITTEN ").map(|_| ()).ok_or_else(|| format!("the harness never said its own landing was written: {lines:?}"))
        }
        Mode::Ask => {
            if let Some(l) = said("RAISE_FAIL ") {
                return Err(format!("its ask to raise its own mode reached no kiln: {l}"));
            }
            if said("RAISE_REFUSED ").or_else(|| said("RAISE_ASKED ")).is_none() {
                return Err(format!("the harness never said how its ask to raise its own mode went: {lines:?}"));
            }
            for (tag, what) in [("LAND_WRITTEN ", "an agent whose mode asks wrote its own work to disk"), ("LAND_SELF_WRITTEN ", "an agent whose mode asks approved its own landing")] {
                if let Some(l) = said(tag) {
                    return Err(format!("{what}: {l}"));
                }
            }
            approval_said(lines).ok_or_else(|| format!("its own landing wasn't held as an approval request naming its manifest: {lines:?}"))?;
            said("LAND_SELF_REFUSED ").map(|_| ()).ok_or_else(|| format!("its own approval of its landing wasn't refused: {lines:?}"))
        }
    }
}

/// The whole turn: its changes ([`check_edits`]), then its own landing ([`check_landing`]).
pub fn check_turn(lines: &[String], probe: &str, mode: Mode) -> Result<(), String> {
    check_edits(lines, probe)?;
    check_landing(lines, mode)
}

// =================================================================================================
// kiln's word: workspaces, a session's volume, a landing's plan
// =================================================================================================

/// Whether kiln says runner `machine` gives agents workspaces of their own (`GET /api/harness/workspaces`'s answer
/// `all`), and why not where it doesn't.
pub fn workspaces_said(all: &Value, machine: &str) -> Result<(), String> {
    let entry = all.get(format!("agent:{machine}")).ok_or_else(|| format!("kiln says nothing of runner {machine}'s workspaces: {all}"))?;
    if entry.get("available") == Some(&Value::Bool(true)) {
        return Ok(());
    }
    Err(entry.get("why_not").and_then(Value::as_str).unwrap_or("kiln says no, and not why").to_owned())
}

/// The Slates revision a kiln vendor manifest (`vendor/MANIFEST.toml`) pins: its `[projects.slates]`'s `commit`.
pub fn vendored_slates(manifest: &str) -> Option<String> {
    let table = toml::from_str::<toml::Table>(manifest).ok()?;
    table.get("projects")?.get("slates")?.get("commit")?.as_str().map(str::to_owned)
}

/// What runner `machine`'s facts, in kiln's list of its runners (`GET /api/agents`), say of the Slates it links
/// (`facts.workspaces.slates`).
pub fn linked_slates(runners: &Value, machine: &str) -> Option<String> {
    let list = runners.as_array().or_else(|| runners.get("agents").and_then(Value::as_array))?;
    let runner = list.iter().find(|r| r.get("name").and_then(Value::as_str) == Some(machine))?;
    runner.pointer("/facts/workspaces/slates").and_then(Value::as_str).map(str::to_owned)
}

/// Whether the Slates a runner says it links (`said`) is the one this checkout of kiln vendors at its pinned commit
/// (`vendored`), as kiln names it (`slates <commit>`): kiln links its vendored Slates, at the revision its manifest pins.
pub fn links_the_pin(said: Option<&str>, vendored: Option<&str>) -> Result<(), String> {
    let vendored = vendored.ok_or("this checkout's vendor/MANIFEST.toml pins no Slates: kiln vendors none")?;
    match said {
        Some(s) if s == format!("slates {vendored}") => Ok(()),
        Some(s) => Err(format!("the runner links {s:?}, not the Slates this kiln pins (slates {vendored})")),
        None => Err(format!("the runner names no Slates it links; this kiln pins slates {vendored}")),
    }
}

/// A session's volume, as kiln's word on it names it (`workspace.made`): the volume, its name, its mount where it has
/// one, and the folder it lies over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Made {
    pub volume: String,
    pub name: String,
    pub mount: Option<String>,
    pub over: String,
}

/// The volume kiln's word on a session (`info`) names: a workspace of kind `volume`, made, its volume named.
pub fn workspace_of(info: &Value) -> Result<Made, String> {
    let w = info.get("workspace").filter(|w| !w.is_null()).ok_or_else(|| format!("kiln names no workspace of the session's: {info}"))?;
    if w.get("kind").and_then(Value::as_str) != Some("volume") {
        return Err(format!("its workspace isn't a volume: {w}"));
    }
    let made = w.get("made").filter(|m| !m.is_null()).ok_or_else(|| format!("its volume was never made: {w}"))?;
    let word = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
    let volume = word(made, "volume").ok_or_else(|| format!("its workspace names no volume: {w}"))?;
    Ok(Made { volume, name: word(made, "name").unwrap_or_default(), mount: word(made, "mount"), over: word(w, "over").unwrap_or_default() })
}

/// A landing's outcome, as kiln answers `agents.land` (the contract's draft 3): one of Slates' typed outcomes
/// (slates-api's `LandOutcome`, g262: `Written`, `ApprovalRequired`, `Conflicted`, `Refused`), named by the answer's
/// `outcome` in snake case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Written to the folder: Slates' landing report rides in `written`.
    Written,
    /// Held for a person's approval: Slates' presentation in `approval`, naming the manifest an approval binds.
    ApprovalRequired { manifest: String },
    /// Its presentation met conflicts (a path changed on disk since the volume read it): each typed in `conflicts`.
    Conflicted { conflicts: Value },
    /// Refused, a closed reason in `refusal`.
    Refused { refusal: Value },
}

/// The outcome kiln's answer to `agents.land` names, or why it names none: an answer in no outcome's shape (draft 2's
/// plan, an approval request naming no manifest) is no answer a consumer can act on.
pub fn outcome_of(answer: &Value) -> Result<Outcome, String> {
    match answer.get("outcome").and_then(Value::as_str) {
        Some("written") => Ok(Outcome::Written),
        Some("approval_required") => {
            let manifest = answer.pointer("/approval/manifest").and_then(Value::as_str).filter(|m| is_manifest(m));
            manifest.map(|m| Outcome::ApprovalRequired { manifest: m.to_owned() }).ok_or_else(|| format!("its approval request names no manifest: {answer}"))
        }
        Some("conflicted") => Ok(Outcome::Conflicted { conflicts: answer.get("conflicts").cloned().unwrap_or(Value::Null) }),
        Some("refused") => Ok(Outcome::Refused { refusal: answer.get("refusal").cloned().unwrap_or(Value::Null) }),
        _ => Err(format!("kiln's answer names no landing outcome: {answer}")),
    }
}

/// The manifest of the approval request kiln answered a landing with: what a person approves.
pub fn approval_manifest(answer: &Value) -> Result<String, String> {
    match outcome_of(answer)? {
        Outcome::ApprovalRequired { manifest } => Ok(manifest),
        other => Err(format!("the landing wasn't held for a person's approval ({other:?}): {answer}")),
    }
}

/// Whether kiln's answer to an approval (`agents.land` with its manifest) says it was written, and not that what it
/// approved moved on in the meantime or was refused.
pub fn written(answer: &Value) -> Result<(), String> {
    match outcome_of(answer)? {
        Outcome::Written => Ok(()),
        other => Err(format!("the approval wrote nothing ({other:?}): {answer}")),
    }
}

// =================================================================================================
// The drive, through kiln's REST API
// =================================================================================================

/// Operation `name` of kiln's (`POST /api/ops/<name>`), asked as the fixture's admin, a person: its answer, or what kiln
/// said refusing it.
pub fn op(f: &vm_e2e::Fixture, name: &str, input: &Value) -> anyhow::Result<Value> {
    let (code, body) = common::http_within(f.port, "POST", &format!("/api/ops/{name}"), &f.admin, Some(input), OP_WITHIN)?;
    anyhow::ensure!(code == 200, "{name} {input}: {code} {body}\n{}\n{}", f.runner_log(), slates_log(&f.dir));
    Ok(serde_json::from_str(&body)?)
}

/// Whether the fixture's runner gives agents workspaces of their own: Ok(false) after saying why it skips; an error
/// where [`REQUIRED`] requires them.
pub fn workspaces_ready(f: &vm_e2e::Fixture) -> anyhow::Result<bool> {
    match workspaces_said(&f.get("/api/harness/workspaces")?, &f.machine) {
        Ok(()) => Ok(true),
        Err(why) => {
            let skip = format!("SKIP: runner {} gives agents no Slates volume of their own: {why}", f.machine);
            anyhow::ensure!(std::env::var_os(REQUIRED).is_none(), "{skip} ({REQUIRED}=1 requires one)");
            eprintln!("{skip}");
            Ok(false)
        }
    }
}

/// The runner's environment for a drive under `dir`: the stand-in under each harness's name
/// ([`vm_e2e::stub_host_env`]), no Slates binary within its reach, and `instance`'s ([`Instance::env`]). On Unix the
/// folder of the system's programs the stand-in's PATH links (`common::agents::path_without_harnesses`, `/usr/bin`'s and
/// `/bin`'s) loses its link to a `slates` there; elsewhere the folders holding one leave the PATH. Refused where one is
/// still found.
pub fn runner_env(dir: &Path, instance: &Instance) -> anyhow::Result<Vec<(&'static str, String)>> {
    let mut env = vm_e2e::stub_host_env(dir)?;
    #[cfg(unix)]
    {
        let link = dir.join("system-bin").join(CLI);
        if link.symlink_metadata().is_ok() {
            std::fs::remove_file(&link)?;
        }
    }
    #[cfg(windows)]
    for (k, v) in &mut env {
        if *k == "PATH" {
            let kept: Vec<PathBuf> = std::env::split_paths(v.as_str()).filter(|d| kiln::os::which_in(CLI, d.as_os_str()).is_none()).collect();
            *v = std::env::join_paths(kept)?.to_string_lossy().into_owned();
        }
    }
    let path = env.iter().find(|(k, _)| *k == "PATH").map(|(_, v)| v.clone()).unwrap_or_default();
    let found = holding_cli(&path);
    anyhow::ensure!(found.is_empty(), "the runner would find Slates' command line in {found:?}");
    env.extend(instance.env());
    Ok(env)
}

/// Which engine a drive runs its agent on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    /// A process on the runner's host, its volume mounted there.
    BareMetal,
    /// A Shards microVM of its own, booted from `image`, its volume served to its guest with no host mount.
    MicroVm,
}

/// What a drive saw at its looks at the host: its instance's `slates` processes ([`cli_of_instance`]), and in a
/// microVM's drive the session's Slates mounts ([`session_mounts`]), with how many looks it took.
#[derive(Debug, Default)]
pub struct Looks {
    pub looks: usize,
    pub cli: Vec<HostProc>,
    pub mounts: Vec<Mount>,
}

impl Looks {
    /// One look: at the host's processes, and where `mounts_of` is given (a microVM's session: its drive's folder and
    /// volume), at the host's mount table.
    pub fn look(&mut self, instance: &Instance, mounts_of: Option<(&Path, &Made)>) -> anyhow::Result<()> {
        self.looks = self.looks.saturating_add(1);
        self.cli.extend(cli_of_instance(&vm_e2e::host_processes()?, &instance.name));
        if let Some((dir, made)) = mounts_of {
            self.mounts.extend(session_mounts(&host_mounts()?, dir, made));
        }
        Ok(())
    }

    /// Nothing of Slates' command line ran at any look, and no host mount of a microVM's session was there.
    pub fn check(&self) -> Result<(), String> {
        if self.looks == 0 {
            return Err("the host was never looked at".to_owned());
        }
        if !self.cli.is_empty() {
            return Err(format!("Slates' command line ran for kiln: {:?}", self.cli));
        }
        if !self.mounts.is_empty() {
            return Err(format!("the microVM's volume was mounted on the host: {:?}", self.mounts));
        }
        Ok(())
    }
}

/// Where a drive runs: its fixture (its server and runner), its folder (where its runner keeps its state, and its
/// projects), its own Slates instance, the engine its agents run on, and in a microVM the image they boot.
pub struct Drive<'a> {
    pub f: &'a vm_e2e::Fixture,
    pub dir: &'a Path,
    pub instance: &'a Instance,
    pub engine: Engine,
    pub image: Option<&'a str>,
}

/// Drives both agents ([`Mode::ALL`]) on the drive's engine, each in its own volume over a project of its own under
/// the drive's folder, through kiln's REST API, and checks them end to end (the module's 1 to 5).
pub fn drive(d: &Drive<'_>) -> anyhow::Result<()> {
    links_pinned(d.f)?;
    let mut looks = Looks::default();
    for mode in Mode::ALL {
        let project = Project::new(&d.dir.join(mode.word()))?;
        drive_agent(d, &project, mode, &mut looks)?;
    }
    looks.check().map_err(|e| anyhow::anyhow!("{:?}: {e}\n{}\n{}", d.engine, d.f.runner_log(), slates_log(d.dir)))
}

/// The runner links the Slates this checkout vendors, at its pinned commit ([`links_the_pin`]).
pub fn links_pinned(f: &vm_e2e::Fixture) -> anyhow::Result<()> {
    let manifest = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vendor/MANIFEST.toml"))?;
    let said = linked_slates(&f.get("/api/agents")?, &f.machine);
    links_the_pin(said.as_deref(), vendored_slates(&manifest).as_deref()).map_err(|e| anyhow::anyhow!("{e}\n{}", f.runner_log()))
}

/// Drives one agent in `mode` on the drive's engine, in its own volume over `project`, and checks it end to end;
/// `looks` gathers what the drive saw at the host.
pub fn drive_agent(d: &Drive<'_>, project: &Project, mode: Mode, looks: &mut Looks) -> anyhow::Result<()> {
    let Drive { f, dir, instance, engine, image } = *d;
    let before = tree(&project.path())?;
    let git_before = project.git_state()?;
    let probe = vm_e2e::probe_name();
    let mut spec = json!({"kind": "claude", "mode": mode.claude(), "cwd": project.dir, "prompt": prompt(&probe, mode), "workspace": {"kind": "volume"}});
    if let (Some(image), Some(o)) = (image, spec.as_object_mut()) {
        o.insert("isolation".to_owned(), json!({"kind": "microvm", "image": image}));
    }
    eprintln!("DRIVE: {engine:?}, an agent in mode {}, on {}, instance {}", mode.claude(), f.machine, instance.name);
    let id = vm_e2e::open(f, &spec)?;
    let mut pace = vm_e2e::Pace::owning(&vm_e2e::host_roots(f.runner_pid)?)?.reporting(landing_reports(&instance.name));
    let (info, events) = pace.until(f, &id, vm_e2e::Asked::Opening, "its first turn", |_, e| vm_e2e::said_marker(&vm_e2e::said(e), vm_e2e::STUB_DONE))?;
    let lines = vm_e2e::said(&events);
    let what = format!("{engine:?} session {id} ({mode:?})");
    let fail = |e: String| anyhow::anyhow!("{what}: {e}\n{}\n{}", f.runner_log(), slates_log(dir));
    check_edits(&lines, &probe).map_err(fail)?;
    let made = workspace_of(&info).map_err(fail)?;
    // From here a failure says what Slates says of the volume and its shards, and so does a kiln operation refused.
    let fail = |e: String| anyhow::anyhow!("{what}: {e}\n{}\n{}\n{}", f.runner_log(), slates_log(dir), slates_state(&instance.name, &made.volume));
    let op = |name: &str, input: &Value| op(f, name, input).map_err(|e| anyhow::anyhow!("{e:#}\n{}", slates_state(&instance.name, &made.volume)));
    anyhow::ensure!(canonical(Path::new(&made.over)) == project.path(), "{what}: its volume lies over {}, not its project {}", made.over, project.dir);
    let said_cwd = vm_e2e::cwd_said(&lines).ok_or_else(|| fail(format!("its harness never said its folder: {lines:?}")))?;
    let mounts_of = match engine {
        Engine::BareMetal => {
            let mount = made.mount.clone().ok_or_else(|| fail(format!("kiln names no mount of its volume on its host: {made:?}")))?;
            anyhow::ensure!(Path::new(&said_cwd).starts_with(canonical(Path::new(&mount))), "{what}: its harness works in {said_cwd}, outside its volume's mount {mount}");
            if cfg!(unix) {
                let at = canonical(Path::new(&mount));
                let table = host_mounts()?;
                anyhow::ensure!(table.iter().any(|m| is_slates(m) && Path::new(&m.at) == at), "{what}: nothing of Slates' is mounted at {}: {table:?}", at.display());
            }
            None
        }
        Engine::MicroVm => {
            anyhow::ensure!(made.mount.is_none(), "{what}: kiln names a host mount of its volume: {made:?}");
            vm_e2e::check_in_guest(vm_e2e::where_said(&lines).as_ref(), std::env::consts::OS, vm_e2e::host_boot_id().as_deref()).map_err(fail)?;
            let guest = vm_e2e::guest_path(&project.dir, std::env::consts::OS).map_err(fail)?;
            anyhow::ensure!(said_cwd == guest, "{what}: its harness works in {said_cwd}, not its project's guest path {guest}");
            Some((dir, &made))
        }
    };
    looks.look(instance, mounts_of)?;
    looks.check().map_err(fail)?;
    eprintln!("PROVEN: {what}: its three edits made in volume {} over {} ({}), its harness at {said_cwd}", made.volume, made.over, made.mount.as_deref().map_or_else(|| "no host mount".to_owned(), |m| format!("mounted at {m}")));
    if mode == Mode::Ask {
        // Its work, and its own landing held for a person's approval, left the project's folder as it was.
        anyhow::ensure!(tree(&project.path())? == before, "{what}: the project's folder changed before any approval: {:?}", changes(&before, &tree(&project.path())?));
        // Review, which needs no landing: its change request holds exactly its changes.
        let changed = op("agents.change", &json!({"agent": id, "title": "g266 end to end"}))?;
        let branch = changed.pointer("/change_request/branch").and_then(Value::as_str).ok_or_else(|| fail(format!("its change request names no branch: {changed}")))?.to_owned();
        let proposed = changes(&project.tree_at("main")?, &project.tree_at(&branch)?);
        anyhow::ensure!(proposed == expected(&probe), "{what}: its change request {branch} holds {proposed:?}, not its harness's changes {:?}", expected(&probe));
        looks.look(instance, mounts_of)?;
        looks.check().map_err(fail)?;
        eprintln!("PROVEN: {what}: its folder untouched; its change request {branch} holds exactly its changes");
    }
    check_landing(&lines, mode).map_err(fail)?;
    eprintln!("PROVEN: {what}: its own landing answered as its mode says: {:?}", lines.iter().filter(|l| l.starts_with("LAND") || l.starts_with("RAISE")).collect::<Vec<_>>());
    if mode == Mode::Ask {
        // The person is shown the request its agent was answered, which writes nothing, and approves it.
        let asked = approval_said(&lines).ok_or_else(|| fail(format!("its harness never said the manifest it was held for: {lines:?}")))?;
        let shown = approval_manifest(&op("agents.land", &json!({"agent": id}))?).map_err(fail)?;
        anyhow::ensure!(shown == asked, "{what}: the person was shown request {shown}, not the one its agent was answered, {asked}");
        anyhow::ensure!(tree(&project.path())? == before, "{what}: showing a person the approval request wrote to the project's folder");
        eprintln!("PROVEN: {what}: the person was shown request {shown}, the one its agent was answered");
        written(&op("agents.land", &json!({"agent": id, "manifest": shown}))?).map_err(fail)?;
    }
    // Its landing written: at once where its mode asks nothing, as its turn said; on the person's approval where it asks.
    let after = tree(&project.path())?;
    anyhow::ensure!(changes(&before, &after) == expected(&probe), "{what}: its landing changed {:?}, not its harness's changes {:?}", changes(&before, &after), expected(&probe));
    anyhow::ensure!(project.git_state()? == git_before, "{what}: its landing changed the project's git HEAD or index");
    eprintln!("PROVEN: {what}: its folder holds exactly its changes, git's HEAD and index as they were");
    looks.look(instance, mounts_of)?;
    pace.stop(f, &id)?;
    if engine == Engine::BareMetal
        && cfg!(unix)
        && let Some(mount) = &made.mount
    {
        mount_gone(&canonical(Path::new(mount))).map_err(|e| anyhow::anyhow!("{what}: {e}"))?;
    }
    Ok(())
}

/// How many of the end lines of each Slates log a failed drive says.
pub const SLATES_LOG_LINES: usize = 80;

/// The end of each Slates instance's own log under the drive's folder `dir` (kiln's `<state>/slates-anchor.log`: its
/// anchor's and its daemon's words, a runner's state folder each), so a drive that fails says what Slates said.
pub fn slates_log(dir: &Path) -> String {
    let Ok(entries) = std::fs::read_dir(dir) else { return String::new() };
    let logs = entries.flatten().map(|e| e.path().join("slates-anchor.log")).filter(|p| p.is_file());
    logs.map(|p| format!("--- the end of {} ---\n{}", p.display(), common::tail(&p, SLATES_LOG_LINES))).collect::<Vec<_>>().join("\n")
}

/// What Slates itself says of the drive's own instance `instance` and a session's volume `volume` when a step fails,
/// asked through its library as an operator would (attached to the instance by name): the volume's status, and each
/// shard's reserve, charges, maps, version slots and refusals, so a refusal names the bound that fired.
pub fn slates_state(instance: &str, volume: &str) -> String {
    let mut i = match slates_api::Instance::attach(instance) {
        Ok(i) => i,
        Err(e) => return format!("--- Slates' own state: attaching to instance {instance}: {e}"),
    };
    let status = match kiln::workspace::slates::volume_id(volume) {
        Ok(v) => i.status(v).map_or_else(|e| format!("status of {volume}: {e}"), |s| format!("status of {volume}: referenced {} B, unique {} B, attachments {}, snapshots {}", s.referenced_bytes, s.unique_bytes, s.attachments, s.snapshots)),
        Err(e) => format!("status of {volume}: {e:#}"),
    };
    let shards = match i.daemon_status() {
        Ok(d) => d
            .shards
            .iter()
            .map(|s| {
                format!(
                    "shard {}: volumes {}, reserve {} B, committed {} B, version slots {}, committed versions {}, retained {} B / {} versions, metadata {} B, committed metadata {} B, mapped {} B, locked {} B, refusals {:?}",
                    s.partition, s.volumes, s.reserve_bytes, s.committed_bytes, s.version_slots, s.committed_versions, s.retained_bytes, s.retained_versions, s.metadata_bytes, s.committed_metadata, s.mapped_bytes, s.locked_bytes, s.refusals
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Err(e) => format!("daemon status: {e}"),
    };
    format!("--- Slates' own state ---\n{status}\n{shards}")
}

/// What a landing in the drive's own instance `instance` reports of itself, for a wait's progress (`vm_e2e::Pace::reporting`):
/// the sequence of the last record of Slates' audit, which a landing moves at each step (presented, granted, validated,
/// each entry written, finished), asked through its library as an operator would. What kept it from saying, where it
/// couldn't, which doesn't move.
fn landing_reports(instance: &str) -> impl FnMut() -> Value + 'static {
    let name = instance.to_owned();
    let mut attached: Option<slates_api::Instance> = None;
    let mut seen = 0u64;
    move || {
        if attached.is_none() {
            attached = slates_api::Instance::attach(&name).ok();
        }
        let Some(i) = attached.as_mut() else { return json!({"audit": "not attached"}) };
        match i.audit(seen) {
            Ok(records) => {
                seen = records.iter().map(|r| r.seq).max().unwrap_or(seen).max(seen);
                json!({"audit": seen})
            }
            Err(e) => {
                attached = None;
                json!({"audit": format!("{e}")})
            }
        }
    }
}

/// Waits, a bounded while ([`OP_WITHIN`]), for the Slates mount at `at` to leave the host's table, as an ended session's
/// does once kiln has let its volume go.
fn mount_gone(at: &Path) -> anyhow::Result<()> {
    let until = Instant::now() + OP_WITHIN;
    loop {
        let table = host_mounts()?;
        if !table.iter().any(|m| is_slates(m) && Path::new(&m.at) == at) {
            return Ok(());
        }
        anyhow::ensure!(Instant::now() < until, "its volume is still mounted at {} after its session ended", at.display());
        std::thread::sleep(LOOK_EVERY);
    }
}

// =================================================================================================
// The checks, against samples (any OS). Each guard's red is beside its green.
// =================================================================================================
mod checks {
    use super::*;

    #[test]
    fn g266_linux_mountinfo_names_each_mounts_place_type_and_source() {
        let text = "36 35 98:0 /mnt1 /mnt/with\\040space rw,noatime master:1 - ext3 /dev/root rw,errors=continue\n\
                    612 30 0:77 / /home/ada/.kiln/workspaces/kiln-01a1 rw,nosuid,nodev,relatime shared:300 - fuse.slates slates:0000000000000002 rw,user_id=1000\n\
                    garbage line without a separator\n";
        let m = mountinfo(text);
        assert_eq!(m.len(), 2, "{m:?}");
        assert_eq!(m.first().map(|x| x.at.as_str()), Some("/mnt/with space"));
        let slates = m.get(1).cloned().unwrap_or(Mount { at: String::new(), kind: String::new(), source: String::new() });
        assert_eq!((slates.kind.as_str(), slates.source.as_str()), ("fuse.slates", "slates:0000000000000002"));
        assert!(is_slates(&slates) && !m.first().is_some_and(is_slates));
    }

    #[test]
    fn g266_macos_mount_lists_each_mounts_place_type_and_source() {
        let text = "/dev/disk3s1s1 on / (apfs, sealed, local, read-only, journaled)\n\
                    slates:/kiln-01a1 on /Users/ada/.kiln/workspaces/kiln 01a1 (nfs, nodev, nosuid, mounted by ada)\n\
                    map auto_home on /System/Volumes/Data/home (autofs, automounted, nobrowse)\n";
        let m = bsd_mounts(text);
        assert_eq!(m.len(), 3, "{m:?}");
        let slates: Vec<&Mount> = m.iter().filter(|x| is_slates(x)).collect();
        assert_eq!(slates.len(), 1, "{m:?}");
        assert_eq!(slates.first().map(|x| (x.at.as_str(), x.kind.as_str())), Some(("/Users/ada/.kiln/workspaces/kiln 01a1", "nfs")));
    }

    fn made(volume: &str, name: &str) -> Made {
        Made { volume: volume.to_owned(), name: name.to_owned(), mount: None, over: String::new() }
    }

    fn mount(at: &str, kind: &str, source: &str) -> Mount {
        Mount { at: at.to_owned(), kind: kind.to_owned(), source: source.to_owned() }
    }

    /// A microVM session's mounts are the Slates mounts under its drive's folder, or naming its volume or its name
    /// anywhere; another drive's, a person's own Slates mount, and any other file system are none of its.
    #[test]
    fn g266_a_sessions_host_mounts_are_slates_mounts_of_its_folder_or_its_volume() {
        let dir = Path::new("/home/ada/drive");
        let m = made("0000000000000007", "kiln-01a1");
        let table = [
            mount("/home/ada/drive/vm-state/workspaces/kiln-01a1", "fuse.slates", "slates:0000000000000009"),
            mount("/elsewhere/x", "nfs", "slates:/kiln-01a1"),
            mount("/elsewhere/y", "fuse.slates", "slates:0000000000000007"),
            mount("/home/ada/other-drive/w", "fuse.slates", "slates:0000000000000003"),
            mount("/home/ada/drive/disk", "ext4", "/dev/sda1"),
            mount("/Users/ada/mine", "nfs", "slates:/persons-own"),
        ];
        let found: Vec<String> = session_mounts(&table, dir, &m).into_iter().map(|x| x.at).collect();
        assert_eq!(found, ["/home/ada/drive/vm-state/workspaces/kiln-01a1", "/elsewhere/x", "/elsewhere/y"]);
        // A made volume with no name or id matches no source by an empty word.
        assert_eq!(session_mounts(&table, Path::new("/nowhere"), &made("", "")).len(), 0);
    }

    fn process_of(pid: u32, argv: &[&str]) -> HostProc {
        HostProc { pid, ppid: 1, argv: argv.iter().map(|s| (*s).to_owned()).collect() }
    }

    /// Slates' command line run for the drive's instance is found by its program and the instance its arguments name,
    /// in either form; the same command for another instance, and kiln's own process naming the instance, are not.
    #[test]
    fn g266_slates_command_line_runs_of_the_drives_instance_are_found() {
        let procs = [
            process_of(10, &["/home/ada/.cargo/bin/slates", "--instance", "kilne2e1", "anchor", "--shards", "2"]),
            process_of(11, &["C:\\tools\\Slates.exe", "--instance=kilne2e1", "status"]),
            process_of(12, &["/usr/local/bin/slates", "--instance", "kilne2e2", "status"]),
            process_of(13, &["/opt/kiln/bin/kiln", "slates-anchor", "--instance", "kilne2e1"]),
            process_of(14, &["slates", "status"]),
        ];
        let cli: Vec<u32> = cli_of_instance(&procs, "kilne2e1").iter().map(|p| p.pid).collect();
        assert_eq!(cli, [10, 11]);
        let all: Vec<u32> = of_instance(&procs, "kilne2e1").iter().map(|p| p.pid).collect();
        assert_eq!(all, [10, 11, 13], "every process of the instance is the drive's to end, whatever runs it");
        assert!(of_instance(&procs, "kilne2e").is_empty(), "a prefix of a name is another instance");
    }

    #[test]
    fn g266_a_path_folder_holding_slates_is_found() -> anyhow::Result<()> {
        let base = tempfile::tempdir()?;
        let (with, without) = (base.path().join("with"), base.path().join("without"));
        std::fs::create_dir_all(&with)?;
        std::fs::create_dir_all(&without)?;
        std::fs::write(with.join(if cfg!(windows) { "slates.exe" } else { "slates" }), "")?;
        let path = std::env::join_paths([&without, &with])?.to_string_lossy().into_owned();
        assert_eq!(holding_cli(&path), std::slice::from_ref(&with));
        let clean = std::env::join_paths([&without])?.to_string_lossy().into_owned();
        assert!(holding_cli(&clean).is_empty());
        Ok(())
    }

    #[test]
    fn g266_a_trees_changes_are_its_files_written_rewritten_and_gone() {
        let before = BTreeMap::from([("a".to_owned(), b"1".to_vec()), ("b".to_owned(), b"2".to_vec()), ("c".to_owned(), b"3".to_vec())]);
        let after = BTreeMap::from([("a".to_owned(), b"1".to_vec()), ("b".to_owned(), b"2+".to_vec()), ("d".to_owned(), b"4".to_vec())]);
        let c = changes(&before, &after);
        assert_eq!(c, BTreeMap::from([("b".to_owned(), Some(b"2+".to_vec())), ("c".to_owned(), None), ("d".to_owned(), Some(b"4".to_vec()))]));
        assert!(changes(&before, &before).is_empty());
    }

    /// The changes [`expected`] names are those the stand-in's words make, read back from a folder it ran in.
    #[test]
    fn g266_the_expected_changes_are_the_ones_the_turn_asks_for() {
        let e = expected("p.txt");
        assert_eq!(e.get("p.txt"), Some(&Some(b"p.txt\n".to_vec())));
        assert_eq!(e.get("README.md"), Some(&Some(b"the project\nAPPENDED README.md\n".to_vec())));
        assert_eq!(e.get("gone.txt"), Some(&None));
        assert_eq!(e.len(), 3);
        assert_eq!(prompt("p.txt", Mode::Bypass), "WRITE=p.txt APPEND=README.md REMOVE=gone.txt LAND do the task");
        assert_eq!(prompt("p.txt", Mode::Ask), "WRITE=p.txt APPEND=README.md REMOVE=gone.txt RAISE LAND do the task", "an ask agent asks to raise its own mode before it lands");
    }

    fn lines(words: &[&str]) -> Vec<String> {
        words.iter().map(|s| (*s).to_owned()).collect()
    }

    /// `lines` with each line that starts with `from` replaced by `to` (none: dropped).
    fn with(lines: &[String], from: &str, to: Option<&str>) -> Vec<String> {
        lines.iter().filter_map(|l| if l.starts_with(from) { to.map(str::to_owned) } else { Some(l.clone()) }).collect()
    }

    const MANIFEST: &str = "ab12cd34ab12cd34ab12cd34ab12cd34ab12cd34ab12cd34ab12cd34ab12cd34";

    /// An agent whose mode asks nothing passes only with its changes made and its own landing written at once: never
    /// held for an approval, refused, or failed for want of a kiln to ask.
    #[test]
    fn g266_a_turn_in_a_mode_that_asks_nothing_passes_only_with_its_landing_written_at_once() {
        let ok = lines(&["CWD /w", "WROTE p.txt", "APPENDED README.md", "REMOVED gone.txt", r#"LAND_WRITTEN {"outcome": "written", "written": {"written": 3}}"#, "STUB_DONE"]);
        assert_eq!(check_turn(&ok, "p.txt", Mode::Bypass), Ok(()));
        assert!(check_turn(&with(&ok, "WROTE", Some("WRITE_FAIL no")), "p.txt", Mode::Bypass).is_err(), "a file it couldn't write");
        assert!(check_turn(&with(&ok, "REMOVED", Some("REMOVE_FAIL gone")), "p.txt", Mode::Bypass).is_err(), "a file it couldn't remove");
        let approval = format!("LAND_APPROVAL {MANIFEST}");
        let held = with(&ok, "LAND_WRITTEN", Some(approval.as_str()));
        assert!(check_turn(&held, "p.txt", Mode::Bypass).is_err_and(|e| e.contains("held")), "a landing held for a person");
        assert!(check_turn(&with(&ok, "LAND_WRITTEN", Some("LAND_REFUSED a mode that forbids writing")), "p.txt", Mode::Bypass).is_err(), "a landing refused");
        assert!(check_turn(&with(&ok, "LAND_WRITTEN", Some("LAND_FAIL no kiln MCP server was given to this harness")), "p.txt", Mode::Bypass).is_err(), "an ask that reached no kiln");
        assert!(check_turn(&with(&ok, "LAND_WRITTEN", None), "p.txt", Mode::Bypass).is_err(), "a landing never said");
        let conflicted = with(&ok, "LAND_WRITTEN", Some(r#"LAND_CONFLICTED {"outcome": "conflicted"}"#));
        assert!(check_turn(&conflicted, "p.txt", Mode::Bypass).is_err_and(|e| e.contains("conflicts")), "a landing that met conflicts");
    }

    /// An agent whose mode asks passes only with its ask to raise its own mode answered, its own landing held as an
    /// approval request naming its manifest and its own approval of it refused: never written by it, never refused
    /// outright, never failed.
    #[test]
    fn g266_a_turn_in_a_mode_that_asks_passes_only_with_its_landing_held_and_its_own_approval_refused() {
        let approval = format!("LAND_APPROVAL {MANIFEST}");
        let ok = lines(&["CWD /w", "WROTE p.txt", "APPENDED README.md", "REMOVED gone.txt", "RAISE_REFUSED an agent can't raise its own mode", approval.as_str(), "LAND_SELF_REFUSED approving an agent's landing is its person's to do", "STUB_DONE"]);
        assert_eq!(check_turn(&ok, "p.txt", Mode::Ask), Ok(()));
        assert_eq!(approval_said(&ok).as_deref(), Some(MANIFEST));
        // Its raise taken by kiln is no failure: its landing, still held, says the raise did nothing.
        assert_eq!(check_turn(&with(&ok, "RAISE_REFUSED", Some(r#"RAISE_ASKED {"results": [{"result": "sent"}]}"#)), "p.txt", Mode::Ask), Ok(()));
        assert!(check_turn(&with(&ok, "RAISE_REFUSED", Some("RAISE_FAIL no kiln MCP server was given to this harness")), "p.txt", Mode::Ask).is_err(), "a raise that reached no kiln");
        assert!(check_turn(&with(&ok, "RAISE_REFUSED", None), "p.txt", Mode::Ask).is_err(), "a raise never asked");
        assert!(check_turn(&with(&ok, "APPENDED", Some("APPEND_FAIL no such file")), "p.txt", Mode::Ask).is_err(), "a file it couldn't change");
        let wrote = with(&with(&ok, "LAND_APPROVAL", Some(r#"LAND_WRITTEN {"outcome": "written"}"#)), "LAND_SELF_REFUSED", None);
        assert!(check_turn(&wrote, "p.txt", Mode::Ask).is_err_and(|e| e.contains("wrote")), "an agent that asks wrote at once");
        assert!(check_turn(&with(&ok, "LAND_SELF_REFUSED", Some("LAND_SELF_WRITTEN {}")), "p.txt", Mode::Ask).is_err_and(|e| e.contains("approved its own")), "an agent that approved its own landing");
        assert!(check_turn(&with(&ok, "LAND_SELF_REFUSED", Some("LAND_SELF_FAIL {}")), "p.txt", Mode::Ask).is_err(), "its own approval neither refused nor written");
        assert!(check_turn(&with(&ok, "LAND_SELF_REFUSED", None), "p.txt", Mode::Ask).is_err(), "its own approval never asked");
        let unnamed = with(&ok, "LAND_APPROVAL", Some("LAND_APPROVAL not-a-manifest"));
        assert!(check_turn(&unnamed, "p.txt", Mode::Ask).is_err() && approval_said(&unnamed).is_none(), "a request naming no manifest");
        assert!(check_turn(&with(&ok, "LAND_APPROVAL", Some("LAND_REFUSED plan mode")), "p.txt", Mode::Ask).is_err(), "refused outright");
    }

    /// The Slates a runner links is read from its facts and held to the commit this checkout's manifest pins; another
    /// revision, none, or a manifest that pins no Slates fails, saying which.
    #[test]
    fn g266_a_runner_links_the_slates_this_kiln_pins() {
        let manifest = "[projects.hyper-raft]\ncommit = \"aa\"\n\n[projects.slates]\nkind = \"git\"\ncommit = \"dbf53afa21bf0e88f7099247c00c2c252ff96b1b\"\npath = \"vendor/slates\"\n";
        assert_eq!(vendored_slates(manifest).as_deref(), Some("dbf53afa21bf0e88f7099247c00c2c252ff96b1b"));
        assert_eq!(vendored_slates("[projects.hex]\ncommit = \"bb\"\n"), None);
        assert_eq!(vendored_slates("not toml ["), None);
        let runners = json!([{"name": "m0", "facts": {}}, {"name": "m1", "facts": {"workspaces": {"available": true, "slates": "slates dbf53afa21bf0e88f7099247c00c2c252ff96b1b", "mount": "nfs"}}}]);
        let said = linked_slates(&runners, "m1");
        assert_eq!(links_the_pin(said.as_deref(), vendored_slates(manifest).as_deref()), Ok(()));
        assert!(links_the_pin(linked_slates(&runners, "m0").as_deref(), Some("dbf53")).is_err_and(|e| e.contains("names no Slates")));
        assert!(links_the_pin(Some("slates 0b850153f"), Some("dbf53afa2")).is_err_and(|e| e.contains("not the Slates this kiln pins")));
        assert!(links_the_pin(said.as_deref(), None).is_err_and(|e| e.contains("vendors none")));
        assert_eq!(linked_slates(&json!({"agents": [{"name": "m1", "facts": {"workspaces": {"slates": "slates x"}}}]}), "m1").as_deref(), Some("slates x"));
    }

    /// A failed drive says the end of each Slates log under its folder, a runner's state folder each, and nothing where
    /// none is.
    #[test]
    fn g266_a_failed_drive_says_the_end_of_each_slates_log_under_its_folder() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        assert_eq!(slates_log(dir.path()), "", "no runner state, no log");
        std::fs::create_dir_all(dir.path().join("slatesvm-state"))?;
        std::fs::create_dir_all(dir.path().join("project"))?;
        let lines: Vec<String> = (0..SLATES_LOG_LINES + 20).map(|n| format!("anchor line {n}")).collect();
        std::fs::write(dir.path().join("slatesvm-state").join("slates-anchor.log"), lines.join("\n"))?;
        let said = slates_log(dir.path());
        assert!(said.starts_with("--- the end of ") && said.contains("slatesvm-state"), "{said}");
        assert!(said.contains(&format!("anchor line {}", SLATES_LOG_LINES + 19)) && !said.contains("anchor line 19\n"), "only its end: {said}");
        assert_eq!(said.lines().count(), SLATES_LOG_LINES + 1, "a header and the end lines");
        Ok(())
    }

    /// Drift is read whether kiln lists it as Slates' lines or as its typed drifts.
    #[test]
    fn g266_a_workspaces_drift_is_read_as_lines_or_typed_drifts() {
        assert_eq!(drifted(&json!({"drift": ["/README.md (Modified)"]})), ["/README.md (Modified)"]);
        assert_eq!(drifted(&json!({"drift": [{"path": "/README.md", "kind": "modified"}, {"kind": "gone"}]})), ["/README.md"]);
        assert!(drifted(&json!({"drift": []})).is_empty() && drifted(&json!({"workspace": null})).is_empty());
    }

    #[test]
    fn g266_claudes_modes_name_the_drives_two_agents() {
        assert_eq!(Mode::ALL.map(Mode::claude), ["default", "bypassPermissions"]);
        assert_eq!(Mode::ALL.map(Mode::word), ["ask", "bypass"]);
        assert!(is_manifest(MANIFEST) && is_manifest(&"AB".repeat(32)));
        assert!(!is_manifest(&"ab".repeat(31)) && !is_manifest(&"ab".repeat(33)) && !is_manifest(&MANIFEST.replace('a', "g")) && !is_manifest(""));
    }

    #[test]
    fn g266_kilns_word_on_a_runners_workspaces_says_why_not() {
        let all = json!({"local": {"available": false, "why_not": "this kiln keeps no workspaces"}, "agent:m1": {"available": true}, "agent:m2": {"available": false, "why_not": "Slates isn't installed on this machine"}});
        assert_eq!(workspaces_said(&all, "m1"), Ok(()));
        assert_eq!(workspaces_said(&all, "m2"), Err("Slates isn't installed on this machine".to_owned()));
        assert!(workspaces_said(&all, "m3").is_err_and(|e| e.contains("nothing")));
    }

    #[test]
    fn g266_a_sessions_volume_is_read_from_kilns_word_on_it() {
        let info = json!({"workspace": {"kind": "volume", "over": "/p", "made": {"volume": "0000000000000007", "name": "kiln-01a1", "mount": "/s/workspaces/kiln-01a1"}}});
        assert_eq!(workspace_of(&info), Ok(Made { volume: "0000000000000007".into(), name: "kiln-01a1".into(), mount: Some("/s/workspaces/kiln-01a1".into()), over: "/p".into() }));
        let served = json!({"workspace": {"kind": "volume", "over": "/p", "made": {"volume": "0000000000000007", "name": "kiln-01a1", "mount": ""}}});
        assert_eq!(workspace_of(&served).map(|m| m.mount), Ok(None), "an empty mount is none");
        assert!(workspace_of(&json!({"workspace": {"kind": "folder"}})).is_err());
        assert!(workspace_of(&json!({"workspace": {"kind": "volume"}})).is_err_and(|e| e.contains("never made")));
        assert!(workspace_of(&json!({"cwd": "/p"})).is_err());
    }

    /// kiln's answer to a landing is one of Slates' typed outcomes, read by its `outcome`; anything else, draft 2's
    /// plan among it, is no outcome; an approval request must name the manifest an approval binds.
    #[test]
    fn g266_kilns_answer_to_a_landing_is_one_typed_outcome() {
        let written_answer = json!({"outcome": "written", "written": {"landing": 7, "written": 3, "conflicts": 0}});
        let held = json!({"outcome": "approval_required", "approval": {"manifest": MANIFEST, "summary": {"bytes": 12}, "entries": [], "conflicts": []}});
        let refused = json!({"outcome": "refused", "refusal": {"reason": "mode_forbids_writing"}});
        let conflicted = json!({"outcome": "conflicted", "conflicts": [{"path": "/README.md", "class": "changed_on_disk"}]});
        assert_eq!(outcome_of(&written_answer), Ok(Outcome::Written));
        assert_eq!(outcome_of(&conflicted), Ok(Outcome::Conflicted { conflicts: json!([{"path": "/README.md", "class": "changed_on_disk"}]) }));
        assert!(approval_manifest(&conflicted).is_err() && written(&conflicted).is_err(), "conflicts are neither a request nor a write");
        assert_eq!(outcome_of(&held), Ok(Outcome::ApprovalRequired { manifest: MANIFEST.to_owned() }));
        assert_eq!(outcome_of(&refused), Ok(Outcome::Refused { refusal: json!({"reason": "mode_forbids_writing"}) }));
        assert!(outcome_of(&json!({"outcome": "approval_required", "approval": {"manifest": "ab12"}})).is_err(), "a manifest no hash");
        assert!(outcome_of(&json!({"outcome": "approval_required"})).is_err(), "a request naming nothing");
        let draft2 = json!({"plan": {"grant_required": true, "manifest": MANIFEST, "landing": 7}, "excludes": ["/.git"], "can_write": true});
        assert!(outcome_of(&draft2).is_err_and(|e| e.contains("no landing outcome")), "draft 2's plan");
        assert!(outcome_of(&json!({"outcome": "landed"})).is_err(), "an outcome of no such name");
        // A person's look and a person's approval each read the one they need.
        assert_eq!(approval_manifest(&held), Ok(MANIFEST.to_owned()));
        assert!(approval_manifest(&written_answer).is_err(), "written, never held for a person");
        assert_eq!(written(&written_answer), Ok(()));
        assert!(written(&held).is_err(), "what was approved moved on: a new request, nothing written");
        assert!(written(&refused).is_err());
    }

    /// A project of the drive's own: its files committed and pushed, so its remote's `main` is its tree, and the
    /// changes of a branch are read from the remote as its tree's.
    #[test]
    fn g266_a_drives_project_is_its_remotes_main() -> anyhow::Result<()> {
        if std::process::Command::new("git").arg("--version").output().is_err() {
            eprintln!("SKIP: no git here");
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let p = Project::new(dir.path())?;
        let here = tree(&p.path())?;
        assert_eq!(here, BTreeMap::from([(APPENDED.0.to_owned(), APPENDED.1.as_bytes().to_vec()), (REMOVED.0.to_owned(), REMOVED.1.as_bytes().to_vec()), (IGNORED.0.to_owned(), IGNORED.1.as_bytes().to_vec())]));
        assert_eq!(p.tree_at("main")?, here);
        let (head, index) = p.git_state()?;
        assert_eq!(head.len(), 40, "{head}");
        assert!(!index.is_empty());
        Ok(())
    }
}

// =================================================================================================
// The stand-in's words for a project's files and its own landing, driven as a process (Unix: python3)
// =================================================================================================
#[cfg(unix)]
mod stand_in {
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::process::{Command, Stdio};

    use serde_json::{Value, json};

    /// What the stand-in said in one turn: its lines, and the permission modes it said it ran in (its answer to
    /// initialize, its turn's init).
    struct Turn {
        said: Vec<String>,
        modes: Vec<String>,
    }

    /// The stand-in, as Claude Code started with `args` (kiln's own words: `--permission-mode`, `--mcp-config`), asked
    /// initialize and given one turn of `prompt` in folder `dir`: what it said, once its turn completed. Its stdin stays
    /// open until then: at its stdin's end a harness stops the turn under way.
    fn one_turn_with(dir: &std::path::Path, prompt: &str, args: &[String]) -> anyhow::Result<Turn> {
        let mut child = Command::new("python3").arg(crate::vm_e2e::stub_path()).args(["--session-id", "g266"]).args(args).env("VM_STUB_PROTOCOL", "claude").current_dir(dir).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()?;
        let mut stdin = child.stdin.take().ok_or_else(|| anyhow::anyhow!("no stdin"))?;
        writeln!(stdin, "{}", json!({"type": "control_request", "request_id": "kiln_1", "request": {"subtype": "initialize"}}))?;
        writeln!(stdin, "{}", json!({"type": "user", "uuid": "m1", "message": {"role": "user", "content": prompt}}))?;
        stdin.flush()?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow::anyhow!("no stdout"))?;
        let mut turn = Turn { said: Vec::new(), modes: Vec::new() };
        for line in BufReader::new(stdout).lines() {
            let v: Value = serde_json::from_str(&line?).unwrap_or(Value::Null);
            if let Some(t) = v.pointer("/message/content/0/text").and_then(Value::as_str) {
                turn.said.push(t.trim().to_owned());
            }
            for mode in [v.pointer("/response/response/current_permission_mode"), v.get("permissionMode")].into_iter().flatten().filter_map(Value::as_str) {
                turn.modes.push(mode.to_owned());
            }
            if v.get("state").and_then(Value::as_str) == Some("completed") {
                break;
            }
        }
        drop(stdin);
        child.wait()?;
        Ok(turn)
    }

    fn one_turn(dir: &std::path::Path, prompt: &str) -> anyhow::Result<Vec<String>> {
        Ok(one_turn_with(dir, prompt, &[])?.said)
    }

    /// A kiln MCP server of the test's own (stdio, a JSON-RPC message a line): its answer to initialize names the agent
    /// it serves (`a1`), and it answers `agents_land` as kiln does for an agent in the landing permission `FAKE_POLICY`
    /// names (the contract's draft 3): `write`, written at once, its approval too; `ask`, an approval request naming the
    /// manifest `ab` 32 times, the agent's own approval refused as the tool's error; `conflict`, its presentation's
    /// conflicts; `refuse`, refused as a JSON-RPC error. An agent's raise of its own mode (`agents_broadcast`'s `set_mode`) is refused as the tool's error, and any
    /// call naming an agent other than `a1` is refused. Each call is written to `FAKE_LOG` as `{"tool", "arguments"}`, a
    /// line each.
    const FAKE_KILN: &str = r#"
import json, os, sys
POLICY, LOG = os.environ["FAKE_POLICY"], os.environ["FAKE_LOG"]
MANIFEST = "ab" * 32
def say(m):
    sys.stdout.write(json.dumps(dict(m, jsonrpc="2.0")) + "\n")
    sys.stdout.flush()
def tool(answer, error=False):
    return {"content": [{"type": "text", "text": answer if isinstance(answer, str) else json.dumps(answer)}], "isError": error}
for line in sys.stdin:
    try:
        m = json.loads(line)
    except ValueError:
        continue
    if "id" not in m:
        continue
    if m.get("method") == "initialize":
        say({"id": m["id"], "result": {"protocolVersion": "2025-06-18", "capabilities": {}, "serverInfo": {"name": "kiln"}, "instructions": "You are kiln agent a1: the agent this connection serves."}})
        continue
    params = m.get("params") or {}
    name, args = params.get("name"), params.get("arguments") or {}
    with open(LOG, "a") as f:
        f.write(json.dumps({"tool": name, "arguments": args}, sort_keys=True) + "\n")
    if name == "agents_broadcast":
        mine = (args.get("to") or {}).get("ids") == ["a1"]
        say({"id": m["id"], "result": tool("an agent can't raise its own mode" if mine else "not yours", True)})
    elif name != "agents_land" or args.get("agent") != "a1":
        say({"id": m["id"], "result": tool("an agent lands only its own work", True)})
    elif POLICY == "write":
        say({"id": m["id"], "result": tool({"outcome": "written", "written": {"written": 3}})})
    elif POLICY == "ask" and "manifest" not in args:
        say({"id": m["id"], "result": tool({"outcome": "approval_required", "approval": {"manifest": MANIFEST, "summary": {}, "entries": [], "conflicts": []}})})
    elif POLICY == "ask":
        say({"id": m["id"], "result": tool("approving its own landing is its person's to do", True)})
    elif POLICY == "conflict":
        say({"id": m["id"], "result": tool({"outcome": "conflicted", "conflicts": [{"path": "/README.md", "class": "changed_on_disk"}]})})
    else:
        say({"id": m["id"], "error": {"code": -32000, "message": "agents_land: a mode that forbids writing"}})
"#;

    /// The stand-in's turn in `mode` ([`crate::prompt`]) with kiln's MCP server the fake one ([`FAKE_KILN`]) in landing
    /// permission `policy`: what it said, and each call it made of kiln, by its tool and arguments.
    fn landing_turn(policy: &str, mode: crate::Mode) -> anyhow::Result<(Vec<String>, Vec<Value>)> {
        let dir = tempfile::tempdir()?;
        let (fake, log) = (dir.path().join("fake-kiln.py"), dir.path().join("asked.log"));
        std::fs::write(&fake, FAKE_KILN)?;
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project)?;
        for (name, text) in [crate::APPENDED, crate::REMOVED] {
            std::fs::write(project.join(name), text)?;
        }
        let env = json!({"FAKE_POLICY": policy, "FAKE_LOG": log.to_string_lossy()});
        let config = json!({"mcpServers": {"kiln": {"command": "python3", "args": ["-I", fake.to_string_lossy()], "env": env}}});
        let said = one_turn_with(&project, &crate::prompt("p.txt", mode), &["--mcp-config".to_owned(), config.to_string()])?.said;
        let asked = std::fs::read_to_string(&log).unwrap_or_default().lines().map(|l| serde_json::from_str(l).unwrap_or(Value::Null)).collect();
        Ok((said, asked))
    }

    /// A call of kiln's MCP tool `tool` with `arguments`, as [`FAKE_KILN`] writes it down.
    fn call(tool: &str, arguments: Value) -> Value {
        json!({"tool": tool, "arguments": arguments})
    }

    /// The stand-in lands its own work by asking kiln as itself, and says what kiln answered by its landing permission:
    /// written at once; or, after its ask to raise its own mode (refused), held for a person's approval, then its own
    /// approval of that request asked and refused; or refused. What it says is what [`crate::check_turn`] reads for each
    /// mode.
    #[test]
    fn g266_the_stand_in_lands_as_itself_and_says_each_outcome_kiln_answers() -> anyhow::Result<()> {
        if !have_python() {
            eprintln!("SKIP: the stand-in harness needs python3");
            return Ok(());
        }
        let (said, asked) = landing_turn("write", crate::Mode::Bypass)?;
        anyhow::ensure!(asked == [call("agents_land", json!({"agent": "a1"}))], "one landing asked, as itself: {asked:?}");
        anyhow::ensure!(said.iter().any(|l| l.starts_with("LAND_WRITTEN ") && l.contains("written")), "{said:?}");
        crate::check_turn(&said, "p.txt", crate::Mode::Bypass).map_err(anyhow::Error::msg)?;
        anyhow::ensure!(crate::check_turn(&said, "p.txt", crate::Mode::Ask).is_err(), "written at once is no ask agent's turn");

        let (said, asked) = landing_turn("ask", crate::Mode::Ask)?;
        let manifest = "ab".repeat(32);
        let raise = call("agents_broadcast", json!({"to": {"ids": ["a1"]}, "command": {"type": "set_mode", "mode": "bypassPermissions"}}));
        let want = [raise, call("agents_land", json!({"agent": "a1"})), call("agents_land", json!({"agent": "a1", "manifest": manifest}))];
        anyhow::ensure!(asked == want, "its raise, its landing, then its own approval of what it was told: {asked:?}");
        anyhow::ensure!(said.iter().any(|l| l.starts_with("RAISE_REFUSED ") && l.contains("raise its own")), "{said:?}");
        anyhow::ensure!(crate::approval_said(&said) == Some(manifest), "{said:?}");
        anyhow::ensure!(said.iter().any(|l| l.starts_with("LAND_SELF_REFUSED ") && l.contains("person")), "{said:?}");
        crate::check_turn(&said, "p.txt", crate::Mode::Ask).map_err(anyhow::Error::msg)?;
        anyhow::ensure!(crate::check_turn(&said, "p.txt", crate::Mode::Bypass).is_err(), "held for a person is no bypass agent's turn");

        let (said, _) = landing_turn("conflict", crate::Mode::Bypass)?;
        anyhow::ensure!(said.iter().any(|l| l.starts_with("LAND_CONFLICTED ") && l.contains("README.md")), "{said:?}");
        anyhow::ensure!(crate::Mode::ALL.iter().all(|m| crate::check_turn(&said, "p.txt", *m).is_err()), "a landing that met conflicts passes for neither mode: {said:?}");

        let (said, asked) = landing_turn("refuse", crate::Mode::Bypass)?;
        anyhow::ensure!(asked == [call("agents_land", json!({"agent": "a1"}))], "refused, nothing more asked: {asked:?}");
        anyhow::ensure!(said.iter().any(|l| l.starts_with("LAND_REFUSED ") && l.contains("forbids writing")), "{said:?}");
        anyhow::ensure!(crate::Mode::ALL.iter().all(|m| crate::check_turn(&said, "p.txt", *m).is_err()), "a refused landing passes for neither mode: {said:?}");
        Ok(())
    }

    /// As Claude Code, the stand-in says the permission mode kiln started it in, in its answer to initialize and its
    /// turn's init alike, and `default` when kiln names none: kiln's word on a session's mode is the harness's own.
    #[test]
    fn g266_the_stand_in_says_the_permission_mode_it_was_started_in() -> anyhow::Result<()> {
        if !have_python() {
            eprintln!("SKIP: the stand-in harness needs python3");
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        for (args, want) in [(vec!["--permission-mode".to_owned(), "bypassPermissions".to_owned()], "bypassPermissions"), (Vec::new(), "default")] {
            let turn = one_turn_with(dir.path(), "do the task", &args)?;
            anyhow::ensure!(turn.modes == [want, want], "{args:?}: {:?}", turn.modes);
            anyhow::ensure!(turn.said.iter().any(|l| l == "STUB_DONE"), "{:?}", turn.said);
        }
        Ok(())
    }

    fn have_python() -> bool {
        Command::new("python3").arg("--version").output().is_ok_and(|o| o.status.success())
    }

    /// The stand-in writes, appends to and removes its project's files as the words of its turn ask, and says so; and
    /// asks kiln to write its own work to disk only where kiln's MCP server was given it, saying it reached none here.
    #[test]
    fn g266_the_stand_in_changes_its_projects_files_as_its_turn_asks() -> anyhow::Result<()> {
        if !have_python() {
            eprintln!("SKIP: the stand-in harness needs python3");
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        for (name, text) in [crate::APPENDED, crate::REMOVED] {
            std::fs::write(dir.path().join(name), text)?;
        }
        let before = crate::tree(dir.path())?;
        let said = one_turn(dir.path(), &crate::prompt("p.txt", crate::Mode::Bypass))?;
        for want in ["WROTE p.txt", "APPENDED README.md", "REMOVED gone.txt", "STUB_DONE"] {
            anyhow::ensure!(said.iter().any(|l| l == want), "{want}: {said:?}");
        }
        anyhow::ensure!(said.iter().any(|l| l.starts_with("LAND_FAIL ") && l.contains("no kiln MCP server")), "no kiln to ask here: {said:?}");
        let changed = crate::changes(&before, &crate::tree(dir.path())?);
        anyhow::ensure!(changed == crate::expected("p.txt"), "{changed:?}");
        Ok(())
    }

    /// An append to a file that isn't there makes none, and says why; a removal of one that isn't there says why.
    #[test]
    fn g266_the_stand_in_makes_no_file_it_was_asked_to_append_to() -> anyhow::Result<()> {
        if !have_python() {
            eprintln!("SKIP: the stand-in harness needs python3");
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let said = one_turn(dir.path(), "APPEND=missing.txt REMOVE=missing.txt do the task")?;
        anyhow::ensure!(said.iter().any(|l| l.starts_with("APPEND_FAIL ")) && said.iter().any(|l| l.starts_with("REMOVE_FAIL ")), "{said:?}");
        anyhow::ensure!(crate::tree(dir.path())?.is_empty(), "it made a file");
        Ok(())
    }
}

// =================================================================================================
// The bare-metal engine: an agent on the runner's host in its own volume, mounted there (Unix: the stand-in is a Python
// script run by its name). Skipped in the runner's own words until it gives agents volumes with no Slates binary.
// =================================================================================================
#[cfg(unix)]
mod bare_metal {
    use super::*;

    /// A server and runner of the drive's own under `dir`, the runner's engine its host (a kiln that carries Shards runs
    /// sessions in microVMs unless told otherwise: kiln plan 01a1dd2e…, P2.3), its environment [`runner_env`]'s.
    fn fixture(dir: &Path, instance: &Instance) -> anyhow::Result<vm_e2e::Fixture> {
        let env = runner_env(dir, instance)?;
        let f = vm_e2e::up_own(dir, "slates", &env.iter().map(|(k, v)| (*k, v.as_str())).collect::<Vec<_>>())?;
        let machine = f.machine.clone();
        vm_e2e::set_engine(&f, &machine, "host")?;
        Ok(f)
    }

    fn have_python() -> bool {
        std::process::Command::new("python3").arg("--version").output().is_ok_and(|o| o.status.success())
    }

    /// The module's 1 to 5 on the host: two agents, each in its own volume mounted on its host (macOS: NFS on loopback;
    /// Linux: FUSE), started through kiln with no Slates binary in reach; the one whose mode asks nothing writes its work
    /// to its folder at once, the one whose mode asks goes up for review and writes only once a person approves it.
    #[test]
    fn g266_on_its_host_a_bypass_agents_landing_writes_at_once_and_an_ask_agents_only_once_a_person_approves() -> anyhow::Result<()> {
        if !have_python() {
            eprintln!("SKIP: the stand-in harness needs python3");
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let instance = Instance::new(dir.path());
        let f = fixture(dir.path(), &instance)?;
        if !workspaces_ready(&f)? {
            return Ok(());
        }
        let dir = canonical(dir.path());
        drive(&Drive { f: &f, dir: &dir, instance: &instance, engine: Engine::BareMetal, image: None })
    }

    /// W6's drift, in either mode: a file the agent changed in its volume, then changed on disk by a person, is drift
    /// (`agents.workspace`), and no landing writes over the person's bytes: neither one the agent whose mode asks
    /// nothing asks in its next turn, written at once, nor one a person approves for the agent whose mode asks.
    #[test]
    fn g266_a_file_changed_on_disk_after_the_agent_changed_it_is_drift_and_no_landing_writes_over_the_persons_bytes() -> anyhow::Result<()> {
        if !have_python() {
            eprintln!("SKIP: the stand-in harness needs python3");
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let instance = Instance::new(dir.path());
        let f = fixture(dir.path(), &instance)?;
        if !workspaces_ready(&f)? {
            return Ok(());
        }
        for mode in Mode::ALL {
            let project = Project::new(&dir.path().join(mode.word()))?;
            drift_kept(&f, &instance, &project, mode).map_err(|e| anyhow::anyhow!("{mode:?}: {e:#}"))?;
        }
        Ok(())
    }

    /// One agent in `mode` over `project`: its append, the person's change of the same file on disk, its drift, then
    /// its landing (asked by the agent in its next turn where its mode asks nothing; approved by the person where it
    /// asks), which keeps the person's bytes.
    fn drift_kept(f: &vm_e2e::Fixture, instance: &Instance, project: &Project, mode: Mode) -> anyhow::Result<()> {
        let spec = json!({"kind": "claude", "mode": mode.claude(), "cwd": project.dir, "prompt": format!("APPEND={} do the task", APPENDED.0), "workspace": {"kind": "volume"}});
        let id = vm_e2e::open(f, &spec)?;
        let mut pace = vm_e2e::Pace::default().reporting(landing_reports(&instance.name));
        let (_, events) = pace.until(f, &id, vm_e2e::Asked::Opening, "its first turn", |_, e| vm_e2e::said_marker(&vm_e2e::said(e), vm_e2e::STUB_DONE))?;
        anyhow::ensure!(vm_e2e::said(&events).iter().any(|l| *l == format!("APPENDED {}", APPENDED.0)), "{:?}", vm_e2e::said(&events));
        let persons = b"the person's\n".to_vec();
        std::fs::write(project.path().join(APPENDED.0), &persons)?;
        let ws = op(f, "agents.workspace", &json!({"agent": id}))?;
        anyhow::ensure!(drifted(&ws).iter().any(|d| d.contains(APPENDED.0)), "its drift doesn't name {}: {ws}", APPENDED.0);
        // Its landing, written or not as kiln and Slates judge it: what each said is kept for the failure's words.
        let said = match mode {
            Mode::Bypass => {
                let client = format!("land-{}", nonce());
                vm_e2e::command(f, &id, &json!({"type": "send", "id": client, "text": "LAND do the task", "mode": "queue"}))?;
                let (_, events) = pace.until(f, &id, vm_e2e::Asked::Landing(&client), "its landing's turn", |_, e| vm_e2e::said_marker(&vm_e2e::said_after(e, &client), vm_e2e::STUB_DONE))?;
                let lines = vm_e2e::said_after(&events, &client);
                anyhow::ensure!(!lines.iter().any(|l| l.starts_with("LAND_FAIL ")), "its landing reached no answer: {lines:?}");
                format!("{lines:?}")
            }
            Mode::Ask => {
                let shown = op(f, "agents.land", &json!({"agent": id}))?;
                match outcome_of(&shown) {
                    Ok(Outcome::ApprovalRequired { manifest }) => op(f, "agents.land", &json!({"agent": id, "manifest": manifest})).map_or_else(|e| format!("{e:#}"), |v| v.to_string()),
                    _ => shown.to_string(),
                }
            }
        };
        anyhow::ensure!(std::fs::read(project.path().join(APPENDED.0))? == persons, "a landing wrote over the person's {}: {said}", APPENDED.0);
        pace.stop(f, &id)
    }
}

// =================================================================================================
// The microVM engine: an agent in a Shards microVM of its own, its volume served to its guest over Slates' own
// virtio-fs route by a descriptor, no host mount between (kiln carrying Shards, on HVF or KVM). Skipped in the runner's
// own words until it starts microVMs and gives agents volumes with no Slates binary.
// =================================================================================================
#[cfg(unix)]
mod microvm {
    use super::*;

    /// The module's 1 to 5 in microVMs: the stand-in's Agentfile booted for two agents, each one's volume reaching its
    /// guest with no host mount at any look; the one whose mode asks nothing writes its work to its folder at once, the
    /// one whose mode asks goes up for review and writes only once a person approves it.
    #[test]
    fn g266_in_its_microvm_a_bypass_agents_landing_writes_at_once_and_an_ask_agents_only_once_a_person_approves() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let instance = Instance::new(dir.path());
        let env = runner_env(dir.path(), &instance)?;
        let f = vm_e2e::up_own(dir.path(), "slatesvm", &env.iter().map(|(k, v)| (*k, v.as_str())).collect::<Vec<_>>())?;
        if !vm_e2e::isolation_ready(&f)? || !workspaces_ready(&f)? {
            return Ok(());
        }
        let kept = vm_e2e::keep_stub(&f, "claude")?;
        let image = format!("agentfile:{}", kept.id);
        let dir = canonical(dir.path());
        drive(&Drive { f: &f, dir: &dir, instance: &instance, engine: Engine::MicroVm, image: Some(image.as_str()) })
    }
}
