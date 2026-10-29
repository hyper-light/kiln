//! Agents' builds on their runner's work (docs/ci.md W24-R24; W24-AC04, AC05).
//!
//! - [`cache_env`]: the variables that point an agent's build tools at the runner's cache service
//!   (`crate::runner::work::service`), with a token of its person's own scope (`person:<id>`,
//!   W24-R21): what its builds put stays in that scope, and they read their repository's trusted
//!   entries too, so an agent's first build takes what CI and other builds already made. Each
//!   scope gets folders of its own for what the tools keep on disk (sccache's server, Go's outputs,
//!   ccache's local cache), so no person's agents read another's from there.
//! - [`warm_target`]: a new worktree's build folder as a copy-on-write clone of the nearest warm
//!   one among its repository's worktrees, with what the two hold alike dated as it is there, so
//!   cargo finds fresh what was built there and rebuilds only what the worktree changed.
//!
//! Nothing calls either as agents start yet: they change nothing of how running agents start.
//! Blocking: git and cp do the work.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use crate::runner::work::service::{Access, Service};
use crate::runner::work::token::is_person;

/// How long an agent's cache token works: the harness mints one each time it starts an agent.
pub const TOKEN_TTL: Duration = Duration::from_secs(7 * 24 * 3600);

/// The build folder a new worktree is given warm: cargo's.
pub const FOLDER: &str = "target";

/// A unix socket's path must fit `sun_path` (104 bytes on macOS, 108 on Linux).
const SOCKET_PATH_MAX: usize = 100;

/// The tools on this machine the variables name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tools {
    /// sccache: `RUSTC_WRAPPER` names it where the machine has it (and can give each scope a
    /// server of its own).
    pub sccache: Option<PathBuf>,
    /// This kiln, for `GOCACHEPROG`.
    pub kiln: Option<PathBuf>,
}

impl Tools {
    /// What this machine has: sccache only from 0.9 on, which gives each scope a server of its own
    /// (`SCCACHE_SERVER_UDS`); an older one would serve every scope from its one server.
    pub fn here() -> Tools {
        Tools { sccache: crate::os::which("sccache").filter(|p| sccache_serves_sockets(p)), kiln: crate::os::this_kiln().ok() }
    }
}

/// Whether the sccache at `path` takes `SCCACHE_SERVER_UDS` (0.9 and later).
fn sccache_serves_sockets(path: &Path) -> bool {
    let Ok(out) = Command::new(path).arg("--version").stdin(Stdio::null()).stderr(Stdio::null()).output() else { return false };
    version_at_least(&String::from_utf8_lossy(&out.stdout), (0, 9))
}

/// Whether a tool's `--version` answer (`sccache 0.10.0`) is `min` or later.
fn version_at_least(text: &str, min: (u64, u64)) -> bool {
    let Some(v) = text.split_whitespace().nth(1) else { return false };
    let mut parts = v.trim_start_matches('v').split('.').map(|p| p.parse::<u64>().ok());
    match (parts.next().flatten(), parts.next().flatten()) {
        (Some(major), Some(minor)) => (major, minor) >= min,
        _ => false,
    }
}

/// `person` as a person's trust domain: `person:<id>` (an id alone is taken as one).
fn person_domain(person: &str) -> anyhow::Result<String> {
    let domain = if person.starts_with("person:") { person.to_owned() } else { format!("person:{person}") };
    anyhow::ensure!(is_person(&domain), "an agent's builds belong to its person, and `{person}` names nobody");
    Ok(domain)
}

/// The folder kept for a scope's tools on this runner.
fn scope_dir(svc: &Service, repo: &str, domain: &str) -> PathBuf {
    let hash = crate::util::sha256_hex(format!("{repo}\0{domain}").as_bytes());
    svc.store().root().join("agents").join(hash.get(..16).unwrap_or("scope"))
}

/// A path as the go command splits `GOCACHEPROG` (`cmd/internal/quoted`): quoted when it holds a
/// space or a quote.
fn quoted(p: &Path) -> String {
    let s = p.to_string_lossy();
    if s.contains([' ', '\t', '"', '\'']) { format!("'{}'", s.replace('\'', "")) } else { s.into_owned() }
}

/// The variables an agent of `person` (`person:<id>`, or the id) working on `repo` gets for its
/// builds on `svc` (docs/ci.md W24-R24):
///
/// - kiln's own (`KILN_CACHE_URL`, `KILN_CACHE_TOKEN`), and Go's through `kiln gocacheprog`
///   (`GOCACHEPROG`, its outputs in the scope's folder);
/// - sccache's WebDAV (`SCCACHE_WEBDAV_ENDPOINT`, `SCCACHE_WEBDAV_TOKEN`), with `RUSTC_WRAPPER`
///   naming sccache where the machine has it and a server of the scope's own
///   (`SCCACHE_SERVER_UDS`: sccache's one server per machine would otherwise serve every scope
///   with the first one's token);
/// - Turborepo's (`TURBO_API`, `TURBO_TOKEN`, `TURBO_TEAM`) and ccache's
///   (`CCACHE_REMOTE_STORAGE`, the token as Basic's password, which every ccache with HTTP
///   storage sends).
///
/// A variable the agent's own environment sets is the harness's to keep over these.
pub fn cache_env(svc: &Service, repo: &str, person: &str, tools: &Tools) -> anyhow::Result<Vec<(String, String)>> {
    let domain = person_domain(person)?;
    let token = svc.mint(&Access { repo: repo.to_owned(), domain: domain.clone(), read: true, write: true, ttl: TOKEN_TTL })?;
    let at = svc.endpoints();
    let own = scope_dir(svc, repo, &domain);
    std::fs::create_dir_all(&own)?;
    let mut env: Vec<(String, String)> = Vec::new();
    let mut put = |k: &str, v: String| env.push((k.to_owned(), v));
    let kiln_url = match (at.url(), &at.unix) {
        (Some(u), _) => u,
        (None, Some(p)) => format!("unix:{}", p.display()),
        (None, None) => anyhow::bail!("this runner's cache service isn't listening"),
    };
    put("KILN_CACHE_URL", kiln_url);
    put("KILN_CACHE_TOKEN", token.clone());
    if let Some(kiln) = &tools.kiln {
        put("GOCACHEPROG", format!("{} gocacheprog", quoted(kiln)));
        put("KILN_GOCACHEPROG_DIR", own.join("go").display().to_string());
    }
    // The tools that speak HTTP take the loopback endpoint.
    if let Some(url) = at.url() {
        put("SCCACHE_WEBDAV_ENDPOINT", format!("{url}/sccache"));
        put("SCCACHE_WEBDAV_TOKEN", token.clone());
        put("SCCACHE_DIR", own.join("sccache").display().to_string());
        let socket = own.join("sccache.sock");
        if cfg!(unix)
            && let Some(sccache) = &tools.sccache
            && socket.as_os_str().len() <= SOCKET_PATH_MAX
        {
            put("SCCACHE_SERVER_UDS", socket.display().to_string());
            put("RUSTC_WRAPPER", sccache.display().to_string());
        }
        put("TURBO_API", url.clone());
        put("TURBO_TOKEN", token.clone());
        put("TURBO_TEAM", "kiln".to_owned());
        if let Some(host) = url.strip_prefix("http://") {
            put("CCACHE_REMOTE_STORAGE", format!("http://kiln:{token}@{host}/ccache"));
        }
        put("CCACHE_DIR", own.join("ccache").display().to_string());
    }
    Ok(env)
}

// ------------------------------------------------------------------------------- warm folders

/// Why a worktree's build folder starts cold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cold(pub String);

impl std::fmt::Display for Cold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A worktree's build folder, cloned from a warm one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warmed {
    /// The worktree whose folder it was cloned from.
    pub from: PathBuf,
    /// The folder, in both (`target`).
    pub folder: String,
    /// Tracked files the two hold otherwise: left as they are, so what depends on them rebuilds.
    pub differ: usize,
    /// Files and folders dated as they are there.
    pub dated: usize,
    pub took: Duration,
}

fn run_git(dir: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).env("GIT_TERMINAL_PROMPT", "0").stdin(Stdio::null()).output().map_err(|e| format!("git: {e}"))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(format!("git {}: {}", args.first().copied().unwrap_or(""), String::from_utf8_lossy(&out.stderr).trim()))
    }
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    Ok(String::from_utf8_lossy(&run_git(dir, args)?).trim_end().to_owned())
}

/// A `-z` listing's records.
fn records(out: &[u8]) -> Vec<String> {
    out.split(|b| *b == 0).filter(|r| !r.is_empty()).map(|r| String::from_utf8_lossy(r).into_owned()).collect()
}

fn canon(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Every worktree of the repository `dir` is in, with the commit its HEAD is at.
fn worktrees(dir: &Path) -> Vec<(PathBuf, String)> {
    let Ok(out) = git(dir, &["worktree", "list", "--porcelain"]) else { return Vec::new() };
    let mut list = Vec::new();
    let mut at: Option<PathBuf> = None;
    for line in out.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            at = Some(PathBuf::from(p));
        } else if let Some(head) = line.strip_prefix("HEAD ")
            && let Some(p) = at.take()
        {
            list.push((p, head.to_owned()));
        }
    }
    list
}

/// Tracked files changed in a worktree from its HEAD, staged or not.
fn dirty(dir: &Path) -> HashSet<String> {
    run_git(dir, &["diff", "--name-only", "-z", "--no-renames", "HEAD"]).map(|o| records(&o).into_iter().collect()).unwrap_or_default()
}

/// How many files two commits hold otherwise.
fn apart(dir: &Path, a: &str, b: &str) -> Option<usize> {
    if a == b {
        return Some(0);
    }
    run_git(dir, &["diff", "--name-only", "-z", "--no-renames", a, b]).ok().map(|o| records(&o).len())
}

/// Each tracked file's blob, as the worktree's index has it (symlinks and submodules aside).
fn blobs(dir: &Path) -> HashMap<String, String> {
    let Ok(out) = run_git(dir, &["ls-files", "-s", "-z"]) else { return HashMap::new() };
    records(&out)
        .into_iter()
        .filter_map(|rec| {
            let (meta, path) = rec.split_once('\t')?;
            let mut f = meta.split(' ');
            let (mode, blob) = (f.next()?, f.next()?);
            matches!(mode, "100644" | "100755").then(|| (path.to_owned(), blob.to_owned()))
        })
        .collect()
}

/// Dates `to` as `from` is dated: whether it could.
fn same_time(from: &Path, to: &Path) -> bool {
    let Ok(when) = std::fs::symlink_metadata(from).and_then(|m| m.modified()) else { return false };
    std::fs::File::open(to).and_then(|f| f.set_modified(when)).is_ok()
}

/// Dates `to`'s tracked files as `from`'s where both hold the same content (the same blob in
/// each index, and no change on top in either), and the folders that hold nothing else: what
/// cargo built in `from` is then fresh in `to`, and only what differs is newer than its build.
/// How many paths it dated, and how many tracked files differ.
fn date_alike(from: &Path, to: &Path) -> (usize, usize) {
    let (theirs, ours) = (blobs(from), blobs(to));
    let (their_dirty, our_dirty) = (dirty(from), dirty(to));
    let mut dated = 0;
    let mut differ: Vec<&str> = Vec::new();
    for (path, blob) in &ours {
        let alike = theirs.get(path) == Some(blob) && !their_dirty.contains(path) && !our_dirty.contains(path);
        if alike && same_time(&from.join(path), &to.join(path)) {
            dated += 1;
        } else {
            differ.push(path);
        }
    }
    let differing = differ.len();
    // A folder that holds a file that differs, or one `to` no longer has, changed too.
    let gone = theirs.keys().filter(|p| !ours.contains_key(*p)).map(String::as_str);
    let mut changed: HashSet<&str> = HashSet::new();
    for p in differ.into_iter().chain(gone) {
        let mut at = p;
        while let Some((parent, _)) = at.rsplit_once('/') {
            if !changed.insert(parent) {
                break;
            }
            at = parent;
        }
    }
    let mut folders: HashSet<&str> = HashSet::new();
    for p in ours.keys() {
        let mut at = p.as_str();
        while let Some((parent, _)) = at.rsplit_once('/') {
            if !folders.insert(parent) {
                break;
            }
            at = parent;
        }
    }
    for d in folders {
        if !changed.contains(d) && same_time(&from.join(d), &to.join(d)) {
            dated += 1;
        }
    }
    (dated, differing)
}

/// `from` cloned to `to`, copy-on-write (reflink on btrfs and XFS, clonefile on APFS), never
/// copied: where the volume can't clone, nothing is made and the error says so.
pub fn clone_tree(from: &Path, to: &Path) -> Result<(), String> {
    if cfg!(windows) {
        return Err("Windows volumes don't clone folders, so the worktree builds cold".to_owned());
    }
    let name = to.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| FOLDER.to_owned());
    let part = to.with_file_name(format!(".{name}.kiln-clone"));
    let _ = std::fs::remove_dir_all(&part);
    let mut cp = Command::new("cp");
    if cfg!(target_os = "macos") {
        cp.args(["-c", "-a"]);
    } else {
        cp.args(["-a", "--reflink=always"]);
    }
    let out = cp.arg(from).arg(&part).stdin(Stdio::null()).output().map_err(|e| format!("cp: {e}"))?;
    if !out.status.success() {
        let _ = std::fs::remove_dir_all(&part);
        return Err(format!("{}'s volume can't clone it ({}), so the worktree builds cold", from.display(), String::from_utf8_lossy(&out.stderr).trim()));
    }
    std::fs::rename(&part, to).map_err(|e| {
        let _ = std::fs::remove_dir_all(&part);
        format!("placing {}: {e}", to.display())
    })
}

/// A new worktree's build folder (docs/ci.md W24-R24): a copy-on-write clone of the nearest warm
/// one among the repository's other worktrees (the fewest files apart, counting what each has
/// changed on top of its commit; the most recently built of those), with what the two hold alike
/// dated as it is there. Cold, saying why, when the worktree has a build folder already, no other
/// worktree has one, or the volume can't clone.
pub fn warm_target(worktree: &Path) -> Result<Warmed, Cold> {
    let started = Instant::now();
    let root = git(worktree, &["rev-parse", "--show-toplevel"]).map(PathBuf::from).map_err(|e| Cold(format!("{} isn't in a git worktree: {e}", worktree.display())))?;
    let into = root.join(FOLDER);
    if into.exists() {
        return Err(Cold(format!("{} has a {FOLDER} folder already", root.display())));
    }
    let head = git(&root, &["rev-parse", "HEAD"]).map_err(|_| Cold(format!("{} has no commit yet", root.display())))?;
    let me = canon(&root);
    let mut nearest: Option<(usize, SystemTime, PathBuf)> = None;
    for (dir, theirs) in worktrees(&root) {
        let warm = dir.join(FOLDER);
        if canon(&dir) == me || !warm.is_dir() {
            continue;
        }
        let Some(n) = apart(&root, &theirs, &head) else { continue };
        let n = n + dirty(&dir).len();
        let built = std::fs::metadata(&warm).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
        if nearest.as_ref().is_none_or(|(best, when, _)| n < *best || (n == *best && built > *when)) {
            nearest = Some((n, built, dir));
        }
    }
    let Some((_, _, from)) = nearest else { return Err(Cold(format!("no other worktree of the repository has a {FOLDER} folder"))) };
    clone_tree(&from.join(FOLDER), &into).map_err(Cold)?;
    let (dated, differ) = date_alike(&from, &root);
    Ok(Warmed { from, folder: FOLDER.to_owned(), differ, dated, took: started.elapsed() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::work::client::{Conn, Endpoint};
    use crate::runner::work::service::tests::{call, on_thread, serve};

    /// Git for the tests' own repositories, free of this machine's config.
    fn sh(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "kiln")
            .env("GIT_AUTHOR_EMAIL", "kiln@localhost")
            .env("GIT_COMMITTER_NAME", "kiln")
            .env("GIT_COMMITTER_EMAIL", "kiln@localhost")
            .output()?;
        anyhow::ensure!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_owned())
    }

    fn put(dir: &Path, rel: &str, data: &[u8]) -> anyhow::Result<()> {
        let p = dir.join(rel);
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(p, data)?;
        Ok(())
    }

    fn get<'a>(env: &'a [(String, String)], k: &str) -> Option<&'a str> {
        env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
    }

    /// W24-R24: an agent's builds get the cache variables under its person's scope: its token
    /// reads and writes `person:<id>` and reads the repository's trusted entries, nothing else;
    /// another person's agents get folders of their own; sccache is the wrapper only where the
    /// machine has it.
    #[test]
    fn w24_r24_agents_get_the_cache_variables_under_their_persons_scope() -> anyhow::Result<()> {
        // A short root: sccache's server socket must fit a Unix socket's path (SOCKET_PATH_MAX).
        // Under a long TMPDIR (ci/gate.sh's, ~/.cache/kiln-gate/<tree>/tmp) the wrapper is rightly
        // left out, and what this test holds is the wrapper where it fits.
        let dir = if cfg!(unix) { tempfile::tempdir_in("/tmp")? } else { tempfile::tempdir()? };
        let root = dir.path().to_path_buf();
        crate::rt::block_on(async move {
            let (svc, at) = serve(&root).await?;
            let tools = Tools { sccache: Some(PathBuf::from("/usr/bin/sccache")), kiln: Some(PathBuf::from("/opt/kiln dir/kiln")) };
            let ada = cache_env(&svc, "acme/app", "ada", &tools)?;
            let url = format!("http://{at}");
            let token = get(&ada, "KILN_CACHE_TOKEN").unwrap_or_default().to_owned();
            anyhow::ensure!(get(&ada, "KILN_CACHE_URL") == Some(url.as_str()), "{ada:?}");
            anyhow::ensure!(get(&ada, "SCCACHE_WEBDAV_ENDPOINT") == Some(format!("{url}/sccache").as_str()) && get(&ada, "SCCACHE_WEBDAV_TOKEN") == Some(token.as_str()));
            anyhow::ensure!(get(&ada, "TURBO_API") == Some(url.as_str()) && get(&ada, "TURBO_TOKEN") == Some(token.as_str()) && get(&ada, "TURBO_TEAM").is_some());
            anyhow::ensure!(get(&ada, "GOCACHEPROG") == Some("'/opt/kiln dir/kiln' gocacheprog"), "{ada:?}");
            anyhow::ensure!(get(&ada, "CCACHE_REMOTE_STORAGE") == Some(format!("http://kiln:{token}@{at}/ccache").as_str()));
            if cfg!(unix) {
                anyhow::ensure!(get(&ada, "RUSTC_WRAPPER") == Some("/usr/bin/sccache") && get(&ada, "SCCACHE_SERVER_UDS").is_some(), "{ada:?}");
            }
            // Another person: another token and folders of their own; no sccache, no wrapper.
            let bob = cache_env(&svc, "acme/app", "person:bob", &Tools { sccache: None, kiln: tools.kiln.clone() })?;
            for k in ["KILN_GOCACHEPROG_DIR", "SCCACHE_DIR", "CCACHE_DIR", "KILN_CACHE_TOKEN"] {
                anyhow::ensure!(get(&ada, k).is_some() && get(&ada, k) != get(&bob, k), "{k} is each scope's own");
            }
            anyhow::ensure!(get(&bob, "RUSTC_WRAPPER").is_none() && get(&bob, "SCCACHE_SERVER_UDS").is_none(), "{bob:?}");
            anyhow::ensure!(cache_env(&svc, "acme/app", "person:", &tools).is_err() && cache_env(&svc, "acme/app", "", &tools).is_err(), "a person names someone");
            // sccache serves a scope of its own from 0.9 on: an older one isn't the wrapper.
            anyhow::ensure!(version_at_least("sccache 0.10.0\n", (0, 9)) && version_at_least("sccache 1.0.0", (0, 9)) && version_at_least("sccache 0.9.1", (0, 9)));
            anyhow::ensure!(!version_at_least("sccache 0.8.2", (0, 9)) && !version_at_least("", (0, 9)) && !version_at_least("sccache", (0, 9)));
            // What the token may do: the person's scope, reading their repository's trusted entries.
            let who = on_thread(move || -> anyhow::Result<serde_json::Value> {
                let r = call(&mut Conn::new(Endpoint::Tcp { addr: at.to_string(), prefix: String::new() }), "GET", "/_kiln/cas/v1/whoami", Some(&token), b"")?;
                Ok(serde_json::from_slice(&r.body)?)
            })
            .await??;
            let want = serde_json::json!([{ "repo": "acme/app", "domain": "person:ada" }, { "repo": "acme/app", "domain": "trusted" }]);
            anyhow::ensure!(who.get("reads") == Some(&want) && who.get("writes") == Some(&serde_json::json!(true)), "{who}");
            Ok(())
        })
    }

    /// A cargo workspace: `a` (a library holding `data` bytes its build script makes, which don't
    /// compress, so what a build folder holds on disk is what it holds), `b` (a library on `a`)
    /// and `app` (a binary on `b`), committed. Four units: `a`'s build script, `a`, `b`, `app`.
    fn fixture(dir: &Path, data: usize) -> anyhow::Result<()> {
        std::fs::create_dir_all(dir)?;
        sh(dir, &["init", "-q", "-b", "main"])?;
        put(dir, "Cargo.toml", b"[workspace]\nmembers = [\"a\", \"b\", \"app\"]\nresolver = \"2\"\n")?;
        put(dir, ".gitignore", b"/target\n")?;
        put(dir, "a/Cargo.toml", b"[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2021\"\nbuild = \"build.rs\"\n")?;
        let script = format!(
            "use std::io::Write;\nfn main() {{\n    println!(\"cargo:rerun-if-changed=build.rs\");\n    let mut s: u64 = 0x9e37_79b9_7f4a_7c15;\n    let mut out = Vec::with_capacity({data});\n    for _ in 0..{data} {{ s ^= s << 13; s ^= s >> 7; s ^= s << 17; out.push(s as u8); }}\n    let dir = std::env::var(\"OUT_DIR\").unwrap();\n    std::fs::File::create(format!(\"{{dir}}/data.bin\")).unwrap().write_all(&out).unwrap();\n}}\n"
        );
        put(dir, "a/build.rs", script.as_bytes())?;
        put(dir, "a/src/lib.rs", b"pub static DATA: &[u8] = include_bytes!(concat!(env!(\"OUT_DIR\"), \"/data.bin\"));\npub fn size() -> usize { DATA.len() }\n")?;
        put(dir, "b/Cargo.toml", b"[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\na = { path = \"../a\" }\n")?;
        put(dir, "b/src/lib.rs", b"pub fn twice() -> usize { a::size() * 2 }\n")?;
        put(dir, "app/Cargo.toml", b"[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nb = { path = \"../b\" }\n")?;
        put(dir, "app/src/main.rs", b"fn main() { println!(\"{}\", b::twice()); }\n")?;
        sh(dir, &["add", "-A"])?;
        sh(dir, &["commit", "-qm", "fixture"])?;
        Ok(())
    }

    /// `cargo build` in `dir`, in its own `target` (not the test's), as an agent's would: the
    /// units it built and how many of them it compiled (cargo's `fresh: false`).
    fn cargo_build(cargo: &Path, dir: &Path) -> anyhow::Result<(usize, usize)> {
        let out = Command::new(cargo)
            .args(["build", "--offline", "--message-format=json"])
            .current_dir(dir)
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("CARGO_BUILD_TARGET_DIR")
            .env_remove("RUSTC_WRAPPER")
            .env_remove("RUSTC_WORKSPACE_WRAPPER")
            .stdin(Stdio::null())
            .output()?;
        anyhow::ensure!(out.status.success(), "cargo build in {}: {}", dir.display(), String::from_utf8_lossy(&out.stderr));
        let (mut units, mut compiled) = (0, 0);
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
            if v.get("reason").and_then(serde_json::Value::as_str) == Some("compiler-artifact") {
                units += 1;
                if v.get("fresh").and_then(serde_json::Value::as_bool) == Some(false) {
                    compiled += 1;
                }
            }
        }
        Ok((units, compiled))
    }

    fn cargo() -> Option<PathBuf> {
        std::env::var_os("CARGO").map(PathBuf::from).filter(|p| p.is_file()).or_else(|| crate::os::which("cargo"))
    }

    /// W24-AC04 (integration): a second agent's first build, in a new worktree of the same
    /// commit, compiles none of the first's units once its `target` is a warm clone; one that
    /// changed a file compiles only what depends on it; without the clone, everything.
    #[test]
    fn w24_ac04_a_new_worktree_compiles_only_what_it_changed() -> anyhow::Result<()> {
        let Some(cargo) = cargo() else {
            eprintln!("w24_ac04: skipped, no cargo on this machine");
            return Ok(());
        };
        let t = tempfile::tempdir()?;
        let main = t.path().join("main");
        fixture(&main, 64 << 10)?;
        let (units, cold) = cargo_build(&cargo, &main)?;
        anyhow::ensure!(units == 4 && cold == 4, "the first build compiles every unit ({cold} of {units})");
        // A second agent's worktree at the same commit.
        let second = t.path().join("second");
        sh(&main, &["worktree", "add", "-q", "--detach", &second.to_string_lossy(), "HEAD"])?;
        let warmed = match warm_target(&second) {
            Ok(w) => w,
            Err(Cold(why)) if why.contains("clone") => {
                eprintln!("w24_ac04: skipped, {why}");
                return Ok(());
            }
            Err(Cold(why)) => anyhow::bail!("{why}"),
        };
        anyhow::ensure!(canon(&warmed.from) == canon(&main) && warmed.differ == 0 && warmed.dated > 0, "{warmed:?}");
        let (units2, compiled) = cargo_build(&cargo, &second)?;
        println!("w24_ac04: the second worktree's first build compiled {compiled} of {units2} units (cloned in {:?})", warmed.took);
        anyhow::ensure!(units2 == 4 && compiled * 100 <= units2 * 5, "at most 5 % of units compiled: {compiled} of {units2}");
        // A commit that changes the binary, made in a worktree that builds nothing; a third
        // agent's worktree there compiles only the binary.
        let edit = t.path().join("edit");
        sh(&main, &["worktree", "add", "-q", "-b", "edit", &edit.to_string_lossy(), "HEAD"])?;
        put(&edit, "app/src/main.rs", b"fn main() { println!(\"twice: {}\", b::twice()); }\n")?;
        sh(&edit, &["commit", "-qam", "edit app"])?;
        let third = t.path().join("third");
        sh(&main, &["worktree", "add", "-q", "--detach", &third.to_string_lossy(), "edit"])?;
        let w3 = warm_target(&third).map_err(|c| anyhow::anyhow!("{c}"))?;
        anyhow::ensure!(w3.differ == 1, "{w3:?}");
        anyhow::ensure!(cargo_build(&cargo, &third)? == (4, 1), "only app compiles");
        // Without a warm clone, everything compiles; with a build folder there already, it's left
        // alone.
        let fourth = t.path().join("fourth");
        sh(&main, &["worktree", "add", "-q", "--detach", &fourth.to_string_lossy(), "HEAD"])?;
        anyhow::ensure!(cargo_build(&cargo, &fourth)? == (4, 4));
        anyhow::ensure!(warm_target(&fourth).is_err_and(|c| c.0.contains("already")));
        Ok(())
    }

    /// The disk bytes the files under `set` hold on btrfs, each extent counted once (`btrfs
    /// filesystem du`: what's exclusive to them and what they share), and what `one` holds alike.
    fn btrfs_bytes(set: &Path, one: &Path) -> Option<(u64, u64)> {
        let _ = Command::new("sync").arg("-f").arg(set).status();
        let du = |p: &Path| -> Option<u64> {
            let out = Command::new("btrfs").args(["filesystem", "du", "-s", "--raw"]).arg(p).stderr(Stdio::null()).output().ok().filter(|o| o.status.success())?;
            let text = String::from_utf8_lossy(&out.stdout).into_owned();
            let n: Vec<u64> = text.lines().last()?.split_whitespace().take(3).map(|n| n.parse().ok()).collect::<Option<_>>()?;
            Some(n.get(1)?.saturating_add(*n.get(2)?))
        };
        Some((du(set)?, du(one)?))
    }

    /// W24-AC05 (measurement): ten agents' worktrees, each `target` a warm clone of the first and
    /// built once more, hold at most 1.3 × one folder's bytes on btrfs.
    #[test]
    fn w24_ac05_ten_agents_target_folders_hold_little_more_than_one() -> anyhow::Result<()> {
        let Some(cargo) = cargo() else {
            eprintln!("w24_ac05: skipped, no cargo on this machine");
            return Ok(());
        };
        let t = tempfile::tempdir()?;
        let main = t.path().join("main");
        fixture(&main, 8 << 20)?;
        cargo_build(&cargo, &main)?;
        let agents = t.path().join("agents");
        std::fs::create_dir_all(&agents)?;
        for n in 0..10 {
            let wt = agents.join(format!("agent{n}"));
            sh(&main, &["worktree", "add", "-q", "--detach", &wt.to_string_lossy(), "HEAD"])?;
            match warm_target(&wt) {
                Ok(_) => {}
                Err(Cold(why)) if why.contains("clone") => {
                    eprintln!("w24_ac05: skipped, {why}");
                    return Ok(());
                }
                Err(Cold(why)) => anyhow::bail!("{why}"),
            }
            cargo_build(&cargo, &wt)?;
        }
        let Some((ten, one)) = btrfs_bytes(&agents, &main.join(FOLDER)) else {
            eprintln!("w24_ac05: skipped, not on btrfs (or no btrfs tool)");
            return Ok(());
        };
        let ratio = ten as f64 / one.max(1) as f64;
        println!("w24_ac05: ten agents' worktrees hold {ten} bytes on disk; one target folder, {one}: {ratio:.3}×");
        anyhow::ensure!(one > 8 << 20, "the folder holds its build ({one} bytes)");
        anyhow::ensure!(ratio <= 1.3, "{ratio:.3}× one folder's bytes");
        Ok(())
    }

    #[test]
    fn w24_r24_a_worktree_with_nothing_warm_starts_cold_saying_why() -> anyhow::Result<()> {
        let t = tempfile::tempdir()?;
        let main = t.path().join("main");
        std::fs::create_dir_all(&main)?;
        sh(&main, &["init", "-q", "-b", "main"])?;
        put(&main, "README", b"x\n")?;
        sh(&main, &["add", "-A"])?;
        sh(&main, &["commit", "-qm", "one"])?;
        let wt = t.path().join("wt");
        sh(&main, &["worktree", "add", "-q", "--detach", &wt.to_string_lossy(), "HEAD"])?;
        anyhow::ensure!(warm_target(&wt).is_err_and(|c| c.0.contains("no other worktree")));
        anyhow::ensure!(warm_target(t.path()).is_err_and(|c| c.0.contains("isn't in a git worktree")));
        Ok(())
    }
}
