//! An isolated harness's configuration on a volume kiln keeps for the person it acts for (kiln plan
//! 01a1cd8e862e7169a074db3d5fa33f3d, step 18). A harness keeps its sign-in, settings, transcripts
//! and memories in its home; in a microVM, its image sees itself read-only, so only a volume makes
//! that home writable and lasting (step 19's research, f22). kiln keeps one directory on the
//! machine for each person and harness, `<state>/homes/<person>/<harness>/`, and gives each of the
//! image's volumes that holds the harness's configuration from beneath it, at its own path
//! (`/home/claude` from `.../claude/home/claude`). The person's own volume is theirs alone: no other
//! person's session, and no other harness, is ever given it.
//!
//! kiln reads a harness's files on its machine (its transcript, Codex's rollouts, memories for a
//! move) through the session's environment and home (`harness::homes::Dirs`, `transcript`,
//! `handoff`), so [`Layout::host_env`] is that environment as the machine sees the microVM's: each
//! of its harness's directories at the path its volume has here. What lies in no volume (the
//! microVM's scratch, `/tmp`) is nowhere on the machine, so its paths are under `.scratch/`, which
//! kiln never makes: reading there finds nothing, as reading the scratch from the machine would,
//! and never the machine's own files.
//!
//! Which volumes hold its configuration is read from the image, not named here: those at, above or
//! beneath a path its harness's run environment names (`HOME=/home/claude`, `CODEX_HOME`,
//! `XDG_CONFIG_HOME` ...). The project is no volume: Shards plugs it in at its host path (D235). A
//! volume none of those names (an older image's `/workspace`) isn't taken for its configuration.

use std::path::{Path, PathBuf};

use super::Refusal;

/// The directory beneath a layout's base for what's in no volume: the microVM's scratch.
pub const SCRATCH: &str = ".scratch";

/// A harness's home when its run environment names none: init's, its scratch (Shards D231,
/// "its scratch at `/tmp` (`HOME=/tmp`, init's)").
pub const INIT_HOME: &str = "/tmp";

/// The variables kiln's own reads of a harness's files take its directories from, each with what
/// it is when its run environment doesn't say, from its home (each harness's and the XDG Base
/// Directory Specification 0.8's own defaults, as `harness::homes::Dirs` has them for a process on
/// its machine). Each is in [`Layout::host_env`] whatever the image says, so that no read of an
/// isolated session's falls back to the machine's own environment, where the machine's person's
/// own harness keeps its files.
const READ: &[(&str, &str)] = &[
    ("CLAUDE_CONFIG_DIR", ".claude"),
    ("CODEX_HOME", ".codex"),
    ("GEMINI_CLI_HOME", ""),
    ("XDG_CONFIG_HOME", ".config"),
    ("XDG_DATA_HOME", ".local/share"),
    ("XDG_STATE_HOME", ".local/state"),
    ("XDG_CACHE_HOME", ".cache"),
];

/// Where one volume's directory is on this machine, and where the microVM sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub host: PathBuf,
    /// Its path in the microVM: absolute, plain.
    pub guest: String,
}

/// An isolated session's harness configuration: its person's volumes, and how the machine reads
/// them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// `<root>/<person>/<harness>`.
    base: PathBuf,
    /// The image's volumes that hold its configuration, as the microVM has them.
    volumes: Vec<String>,
    /// Its harness's run environment, as the microVM has it.
    env: Vec<(String, String)>,
}

impl Layout {
    /// The layout under `root` (a machine's `<state>/homes`) for `person` (`owner`, `user:<id>`)
    /// and `harness`'s image, whose harness volumes are `volumes` and whose run environment is
    /// `env` (its `harness.json`'s `run.env`, and what the run adds). Refused, typed: a person or
    /// harness kiln keeps nothing for, a volume no machine can keep a directory for, or an image
    /// that keeps its harness's configuration in no volume.
    pub fn new(root: &Path, person: &str, harness: &str, volumes: &[String], env: &[(String, String)]) -> Result<Self, Refusal> {
        let person_dir = person_dir(person)?;
        if crate::harness::kinds::Kind::get(harness).is_none() {
            return Err(Refusal::Harness(harness.to_owned()));
        }
        for v in volumes {
            portable(v)?;
        }
        // Two volumes one machine's file system takes for one (macOS's and Windows' fold case).
        for (i, a) in volumes.iter().enumerate() {
            if volumes.iter().skip(i + 1).any(|b| a != b && a.eq_ignore_ascii_case(b)) {
                return Err(Refusal::Volume { path: a.clone(), why: "another of its volumes differs from it only by case, and a file system that folds case keeps them as one" });
            }
        }
        let named: Vec<&str> = env.iter().map(|(_, v)| v.as_str()).filter(|v| guest_plain(v)).collect();
        let held: Vec<String> = volumes.iter().filter(|v| named.iter().any(|n| related(v, n))).cloned().collect();
        if held.is_empty() {
            let names: Vec<String> = env.iter().filter(|(_, v)| guest_plain(v)).map(|(k, v)| format!("{k}={v}")).collect();
            return Err(Refusal::NoConfigVolume { harness: harness.to_owned(), named: if names.is_empty() { "no directory".to_owned() } else { names.join(", ") } });
        }
        Ok(Self { base: root.join(person_dir).join(harness), volumes: held, env: env.to_vec() })
    }

    /// Each of its volumes: its directory here, and its path in the microVM.
    pub fn mounts(&self) -> Vec<Mount> {
        self.volumes.iter().map(|g| Mount { host: under(&self.base, g), guest: g.clone() }).collect()
    }

    /// Where `guest`, a path in the microVM, is on this machine: beneath one of its volumes, that
    /// volume's directory here; anywhere else, beneath [`SCRATCH`], which kiln never makes. None for
    /// a path that isn't absolute and plain.
    pub fn host_of(&self, guest: &str) -> Option<PathBuf> {
        if !guest_plain(guest) {
            return None;
        }
        if self.volumes.iter().any(|v| beneath_or_at(guest, v)) {
            return Some(under(&self.base, guest));
        }
        Some(under(&self.base.join(SCRATCH), guest))
    }

    /// Its harness's home in the microVM: its run environment's `HOME`, else init's.
    pub fn guest_home(&self) -> String {
        self.var("HOME").filter(|h| guest_plain(h)).unwrap_or(INIT_HOME).to_owned()
    }

    fn var(&self, key: &str) -> Option<&str> {
        self.env.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// The session's environment as this machine reads its harness's files (`harness::homes::Dirs`
    /// and the rest): its home, and each directory variable its run environment names or kiln's
    /// reads take, at its path here. The microVM never gets it: its harness has its own
    /// ([`super::env`]).
    pub fn host_env(&self) -> Vec<(String, String)> {
        let home = self.guest_home();
        let mut out: Vec<(String, String)> = Vec::new();
        let mut put = |k: &str, guest: &str| {
            if let Some(h) = self.host_of(guest)
                && !out.iter().any(|(o, _)| o == k)
            {
                out.push((k.to_owned(), h.to_string_lossy().into_owned()));
            }
        };
        put("HOME", &home);
        // A directory's path, not a list of them (`PATH`) nor a word (`DISABLE_UPDATES=1`).
        for (k, v) in self.env.iter().filter(|(_, v)| guest_plain(v) && v.bytes().all(|b| b == b'/' || portable_byte(b))) {
            put(k, v);
        }
        for (k, rel) in READ {
            let guest = if rel.is_empty() { home.clone() } else { format!("{}/{rel}", home.trim_end_matches('/')) };
            put(k, &guest);
        }
        out
    }

    /// Its home on this machine, as [`Self::host_env`]'s `HOME`.
    pub fn host_home(&self) -> PathBuf {
        self.host_of(&self.guest_home()).unwrap_or_else(|| self.base.join(SCRATCH))
    }

    /// Makes each volume's directory, readable by this machine's user alone, before the microVM
    /// is given it. Blocking.
    pub fn prepare(&self) -> std::io::Result<()> {
        for m in self.mounts() {
            std::fs::create_dir_all(&m.host)?;
            private(&m.host)?;
        }
        // The harness's and the person's directories too: one person's harnesses are theirs alone.
        private(&self.base)?;
        if let Some(person) = self.base.parent() {
            private(person)?;
        }
        Ok(())
    }
}

#[cfg(unix)]
fn private(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

/// Windows: a directory under the runner's own state directory inherits its ACL, which is the
/// runner's user's (`crate::os`'s state directory under the user's profile).
#[cfg(not(unix))]
fn private(_dir: &Path) -> std::io::Result<()> {
    Ok(())
}

/// The person's directory's name: `owner`, or `user-<id>` for `user:<id>`.
fn person_dir(person: &str) -> Result<String, Refusal> {
    let refused = || Refusal::Person(person.to_owned());
    if person == "owner" {
        return Ok("owner".to_owned());
    }
    let id = person.strip_prefix("user:").ok_or_else(refused)?;
    if id.is_empty() || !id.bytes().all(portable_byte) || id.starts_with('.') {
        return Err(refused());
    }
    Ok(format!("user-{id}"))
}

/// POSIX's portable filename character set (POSIX.1-2017, Base Definitions §3.282): names of these
/// alone mean one file on every system kiln runs on.
fn portable_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')
}

/// Windows' reserved device names, which no file there may have, whatever its extension
/// (learn.microsoft.com, "Naming Files, Paths, and Namespaces").
fn reserved_on_windows(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL") || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.bytes().last().is_some_and(|d| (b'1'..=b'9').contains(&d)))
}

/// A volume's path that every machine can keep a directory for.
fn portable(path: &str) -> Result<(), Refusal> {
    let refuse = |why: &'static str| Err(Refusal::Volume { path: path.to_owned(), why });
    if !guest_plain(path) {
        return refuse("a volume's path is absolute, with no empty, `.` or `..` part");
    }
    for c in path.split('/').skip(1) {
        if !c.bytes().all(portable_byte) {
            return refuse("its names are of letters, digits, `.`, `_` and `-` alone (POSIX's portable filename characters)");
        }
        if reserved_on_windows(c) {
            return refuse("a name Windows reserves for a device");
        }
    }
    Ok(())
}

/// An absolute Linux path with no empty, `.` or `..` component, as Shards holds a member's working
/// directory and a plug's path (D231, D235); `/` itself is no directory of a harness's.
fn guest_plain(p: &str) -> bool {
    p.strip_prefix('/').is_some_and(|rest| !rest.is_empty() && rest.split('/').all(|c| !c.is_empty() && c != "." && c != ".."))
}

/// `a` at or beneath `b`, by whole components.
fn beneath_or_at(a: &str, b: &str) -> bool {
    a == b || a.strip_prefix(b).is_some_and(|rest| rest.starts_with('/'))
}

/// One at or beneath the other.
fn related(a: &str, b: &str) -> bool {
    beneath_or_at(a, b) || beneath_or_at(b, a)
}

/// `guest`'s components beneath `base`.
fn under(base: &Path, guest: &str) -> PathBuf {
    guest.split('/').filter(|c| !c.is_empty()).fold(base.to_path_buf(), |p, c| p.join(c))
}

/// A built-in Agentfile's harness volumes and run environment (`agentfiles::builtin`), as its text
/// and its `harness.json` say them: its `VOLUME --target-kind=harness <path> FOR <harness>` lines,
/// and its `run.env`.
pub fn of_builtin(b: &crate::agentfiles::builtin::Builtin) -> (Vec<String>, Vec<(String, String)>) {
    let tail = format!(" FOR {}", b.harness);
    let volumes = b.text.lines().map(str::trim).filter_map(|l| l.strip_prefix("VOLUME --target-kind=harness ")?.strip_suffix(tail.as_str())).map(str::to_owned).collect();
    let env = b
        .files
        .iter()
        .find(|(n, _)| *n == "harness.json")
        .and_then(|(_, text)| serde_json::from_str::<serde_json::Value>(text).ok())
        .and_then(|h| h.pointer("/run/env").and_then(serde_json::Value::as_array).cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(serde_json::Value::as_str)
        .filter_map(|e| e.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    (volumes, env)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{Layout, Mount, SCRATCH, guest_plain, of_builtin, person_dir};
    use crate::harness::isolated::Refusal;

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
    }

    fn vols(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    /// Claude's image: its home is its person's volume, the project's isn't, and kiln reads
    /// Claude Code's transcripts at the volume's path here, never the machine's own.
    #[test]
    fn step18_a_harness_home_is_its_persons_volume_and_read_here() -> Result<(), Refusal> {
        let root = Path::new("/var/kiln/homes");
        let l = Layout::new(root, "user:u1", "claude", &vols(&["/workspace", "/home/claude"]), &env(&[("HOME", "/home/claude"), ("DISABLE_UPDATES", "1")]))?;
        let base = root.join("user-u1").join("claude");
        assert_eq!(l.mounts(), vec![Mount { host: base.join("home").join("claude"), guest: "/home/claude".into() }]);
        let host = l.host_env();
        let get = |k: &str| host.iter().find(|(n, _)| n == k).map(|(_, v)| Path::new(v).to_path_buf());
        assert_eq!(get("HOME"), Some(base.join("home").join("claude")));
        assert_eq!(get("CLAUDE_CONFIG_DIR"), Some(base.join("home").join("claude").join(".claude")));
        // A value that's no path stays the microVM's alone.
        assert_eq!(get("DISABLE_UPDATES"), None);
        // What lies in no volume is nowhere here.
        assert_eq!(l.host_of("/tmp/x"), Some(base.join(SCRATCH).join("tmp").join("x")));
        assert_eq!(l.host_of("relative"), None);
        Ok(())
    }

    /// Every variable kiln's reads take is set, whatever the image's environment says, so that no
    /// read falls back to this machine's own `CLAUDE_CONFIG_DIR`, `CODEX_HOME` or home; and a
    /// harness with no `HOME` of its own keeps init's, the scratch.
    #[test]
    fn step18_no_read_of_an_isolated_session_falls_back_to_the_machine() -> Result<(), Refusal> {
        let root = Path::new("/var/kiln/homes");
        let l = Layout::new(root, "owner", "codex", &vols(&["/workspace", "/home/codex/.codex"]), &env(&[("CODEX_HOME", "/home/codex/.codex")]))?;
        let host = l.host_env();
        for k in ["HOME", "CLAUDE_CONFIG_DIR", "CODEX_HOME", "GEMINI_CLI_HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME", "XDG_CACHE_HOME"] {
            let v = host.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
            assert!(v.as_deref().is_some_and(|v| Path::new(v).starts_with(root.join("owner").join("codex"))), "{k}: {v:?}");
        }
        let base = root.join("owner").join("codex");
        assert_eq!(l.guest_home(), "/tmp");
        assert_eq!(l.host_home(), base.join(SCRATCH).join("tmp"));
        assert!(host.contains(&("CODEX_HOME".into(), base.join("home").join("codex").join(".codex").to_string_lossy().into_owned())));
        Ok(())
    }

    /// One person's volumes are theirs alone, and one harness's its own: nothing names a way out of
    /// its directory.
    #[test]
    fn step18_each_persons_and_harness_volume_is_its_own() -> Result<(), Refusal> {
        let root = Path::new("/r");
        let make = |p: &str, h: &str| Layout::new(root, p, h, &vols(&["/home/h"]), &env(&[("HOME", "/home/h")]));
        let a = make("user:a", "claude")?.mounts();
        let b = make("user:b", "claude")?.mounts();
        let c = make("user:a", "codex")?.mounts();
        assert!(a.iter().all(|m| !b.contains(m) && !c.contains(m)));
        for bad in ["", "user:", "user:../owner", "user:a/b", "user:.hidden", "admin", "user:a b", "owner/x"] {
            assert_eq!(person_dir(bad), Err(Refusal::Person(bad.to_owned())), "{bad:?}");
        }
        assert!(matches!(make("owner", "not-a-harness"), Err(Refusal::Harness(_))));
        Ok(())
    }

    /// A volume no machine can keep a directory for is refused, saying why; so is an image that
    /// keeps its harness's configuration in no volume.
    #[test]
    fn step18_volumes_every_machine_can_keep_or_a_refusal() {
        let root = Path::new("/r");
        let home = env(&[("HOME", "/home/h")]);
        for bad in ["/home/h/../x", "/home//h", "home/h", "/home/h:x", "/home/con", "/home/LPT1.txt", "/home/a b"] {
            assert!(matches!(Layout::new(root, "owner", "claude", &vols(&["/home/h", bad]), &home), Err(Refusal::Volume { .. })), "{bad:?}");
        }
        assert!(matches!(Layout::new(root, "owner", "claude", &vols(&["/home/h", "/home/H"]), &home), Err(Refusal::Volume { .. })));
        assert!(matches!(Layout::new(root, "owner", "claude", &vols(&["/workspace"]), &home), Err(Refusal::NoConfigVolume { .. })));
        assert!(matches!(Layout::new(root, "owner", "claude", &vols(&["/home/h"]), &[]), Err(Refusal::NoConfigVolume { .. })));
        assert!(!guest_plain("/") && guest_plain("/a") && !guest_plain("/a/") && !guest_plain("/a/./b"));
    }

    /// Every built-in's image keeps its harness's configuration in a volume of its own, the
    /// project's volume apart, and each built-in's layout is made.
    #[test]
    fn step18_every_builtin_keeps_its_harness_configuration_in_a_volume() -> Result<(), Refusal> {
        for b in crate::agentfiles::builtin::ALL {
            let (volumes, env) = of_builtin(b);
            let l = Layout::new(Path::new("/r"), "owner", b.harness, &volumes, &env)?;
            let mounts = l.mounts();
            assert!(!mounts.is_empty() && mounts.iter().all(|m| m.guest != "/workspace"), "{}: {mounts:?}", b.name);
        }
        Ok(())
    }

    /// Made, each directory is its user's alone.
    #[cfg(unix)]
    #[test]
    fn step18_its_directories_are_made_readable_by_this_user_alone() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        let t = tempfile::tempdir()?;
        let l = Layout::new(t.path(), "user:u1", "devin", &vols(&["/home/devin/.config/devin", "/home/devin/.local/share/devin"]), &env(&[("XDG_CONFIG_HOME", "/home/devin/.config"), ("XDG_DATA_HOME", "/home/devin/.local/share")]))?;
        l.prepare()?;
        for m in l.mounts() {
            anyhow::ensure!(std::fs::metadata(&m.host)?.permissions().mode() & 0o777 == 0o700, "{}", m.host.display());
        }
        for d in [t.path().join("user-u1"), t.path().join("user-u1").join("devin")] {
            anyhow::ensure!(std::fs::metadata(&d)?.permissions().mode() & 0o777 == 0o700, "{}", d.display());
        }
        Ok(())
    }
}
