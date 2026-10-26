//! Path sets from the tools' own records (docs/ci.md W24-R04, R05), pure: what a unit read, taken
//! from what its tools already write down, the best of R04's sources:
//!
//! - **cargo's dependency files** ([`dep_info`]): rustc's `target/<profile>/deps/<unit>.d` and
//!   cargo's own `target/<profile>/<artifact>.d`, makefile rules from each output to its sources
//!   (`include_str!`'s files among them), with rustc's `# env-dep:` lines for each variable `env!`
//!   and `option_env!` read, and the value it had (none when it was unset);
//! - **build scripts' directives** ([`build_script`]): `rerun-if-changed` and
//!   `rerun-if-env-changed`, in their `cargo::` and older `cargo:` forms alike. A path is a file or
//!   a folder whose every file counts, and a script that gives neither directive depends on its
//!   whole package, as cargo has it;
//! - **TypeScript's `.tsbuildinfo`** ([`tsbuildinfo`]): the program's files (`fileNames`, under
//!   `program` before TypeScript 5.6), or, for a build that isn't incremental, its roots, which is
//!   all it records then ([`Observed::partial`] says so);
//! - **Go's record of what a test read** ([`go_test_log`]): its log of `open`, `stat`, `getenv` and
//!   `chdir`, which reaches a `GOCACHEPROG` as the body of a `put` ([`go_actions`]): the first half
//!   of cmd/go's two-level test cache, the same two phases as R04's. The go command keys its other
//!   actions itself and tells its cache program nothing of their files.
//!
//! [`Observed`] gathers them, each path placed by [`Roots`] so that a path set means the same on
//! every machine and in every workspace: the workspace's own paths relative, known folders under
//! their names (`$CARGO_HOME/…`), temporary ones left out (R05 keeps temporary paths out of keys).
//! [`Observed::path_set`] makes the [`PathSet`] a strong key takes, each path's digest read through
//! what the caller passes in. A path ending in `/` stands for everything under it: a folder's whole
//! tree, recursively, as a git tree id has it (a file's own digest when it names a file). Any other
//! path is a file's content, a folder's listing, or that it's absent.

use std::collections::{BTreeMap, BTreeSet};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use super::key::{PathSet, Read};

/// How paths become the same on every machine and in every workspace (R04, R05): each absolute
/// folder with the name what's under it takes in a path set, or left out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Roots {
    /// Normalized absolute folders, each with its name (`""`: the workspace, whose paths are
    /// relative) or `None` (left out).
    folders: Vec<(String, Option<String>)>,
    /// Windows paths ([`Roots::on_windows`]): `\` separates their parts as `/` does, and `C:`
    /// starts an absolute one.
    windows: bool,
}

/// Where one path lands.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Placed {
    /// In the workspace (relative) or under a named folder.
    In(String),
    /// Under a folder left out.
    Skipped,
    /// Absolute under no folder named, or climbing out of the workspace (`../`): kept as it is.
    Outside(String),
}

impl Roots {
    /// The workspace at `folder`: what's under it becomes relative.
    pub fn workspace(folder: &str) -> Roots {
        Roots::default().name(folder, "")
    }

    /// Windows paths.
    pub fn on_windows(mut self) -> Roots {
        self.windows = true;
        self.folders = self.folders.into_iter().map(|(f, n)| (normal(&f, true), n)).collect();
        self
    }

    /// `folder` known by `name` (`$CARGO_HOME`, `$TARGET`): what's under it is `<name>/<rest>` in a
    /// path set, and the machine that looks it up puts its own folder of that name back
    /// ([`Roots::resolve`]).
    pub fn name(mut self, folder: &str, name: &str) -> Roots {
        let folder = normal(folder, self.windows);
        self.folders.push((folder, Some(name.trim_end_matches('/').to_owned())));
        self
    }

    /// What's under `folder` left out: a temporary folder (R05), or one whose contents something
    /// else in the key covers (a registry's sources, which `Cargo.lock`'s checksums fix).
    pub fn skip(mut self, folder: &str) -> Roots {
        let folder = normal(folder, self.windows);
        self.folders.push((folder, None));
        self
    }

    /// A path from a path set, back on this machine: under the workspace or a named folder, or as
    /// it is when it was outside them all. `None` when it names a folder these roots don't have.
    pub fn resolve(&self, placed: &str) -> Option<String> {
        if is_absolute(placed, self.windows) {
            return Some(placed.to_owned());
        }
        let tree = placed.ends_with('/');
        let path = placed.trim_end_matches('/');
        let named = self.folders.iter().filter_map(|(f, n)| Some((f, n.as_deref()?))).filter(|(_, n)| !n.is_empty()).find_map(|(f, n)| {
            let rest = if path == n { Some("") } else { path.strip_prefix(n).and_then(|r| r.strip_prefix('/')) };
            rest.map(|r| (f, r))
        });
        let (folder, rest) = match named {
            Some(found) => found,
            None => (&self.folders.iter().find(|(_, n)| n.as_deref() == Some(""))?.0, if path == "." { "" } else { path }),
        };
        let mut out = if rest.is_empty() { folder.clone() } else { format!("{}/{rest}", folder.trim_end_matches('/')) };
        if tree {
            out.push('/');
        }
        Some(out)
    }

    /// Where `path`, relative to `base` unless absolute, lands.
    fn place(&self, base: &str, path: &str) -> Placed {
        let p = normal(&join(base, path, self.windows), self.windows);
        if !is_absolute(&p, self.windows) {
            return if p == ".." || p.starts_with("../") { Placed::Outside(p) } else { Placed::In(p) };
        }
        let best = self.folders.iter().filter_map(|(f, name)| under(&p, f).map(|rest| (f.len(), rest, name))).max_by_key(|(n, _, _)| *n);
        match best {
            None => Placed::Outside(p),
            Some((_, _, None)) => Placed::Skipped,
            Some((_, rest, Some(name))) => Placed::In(match (name.is_empty(), rest.is_empty()) {
                (true, true) => ".".to_owned(),
                (true, false) => rest.to_owned(),
                (false, true) => name.clone(),
                (false, false) => format!("{name}/{rest}"),
            }),
        }
    }
}

/// What's after `folder` in `path`, when `path` is `folder` or under it.
fn under<'a>(path: &'a str, folder: &str) -> Option<&'a str> {
    if folder == "/" {
        return path.strip_prefix('/');
    }
    if path == folder {
        return Some("");
    }
    path.strip_prefix(folder)?.strip_prefix('/')
}

fn is_absolute(path: &str, windows: bool) -> bool {
    if path.starts_with('/') {
        return true;
    }
    if !windows {
        return false;
    }
    let mut chars = path.chars();
    path.starts_with('\\') || matches!((chars.next(), chars.next()), (Some(d), Some(':')) if d.is_ascii_alphabetic())
}

fn join(base: &str, path: &str, windows: bool) -> String {
    if is_absolute(path, windows) || base.is_empty() || base == "." {
        path.to_owned()
    } else {
        format!("{}/{path}", base.trim_end_matches(['/', '\\']))
    }
}

/// `path` with its `.` and empty parts dropped and each `..` taking back the part before it,
/// `/`-separated (and `\` too for Windows paths). An absolute path never climbs above its root; a
/// relative one keeps the `..` it can't take back.
fn normal(path: &str, windows: bool) -> String {
    let unified = if windows { path.replace('\\', "/") } else { path.to_owned() };
    let (root, rest) = if let Some(rest) = unified.strip_prefix('/') {
        ("/".to_owned(), rest.to_owned())
    } else if windows && is_absolute(&unified, true) {
        let (drive, rest) = unified.split_once(':').unwrap_or((unified.as_str(), ""));
        (format!("{drive}:/"), rest.trim_start_matches('/').to_owned())
    } else {
        (String::new(), unified)
    };
    let mut parts: Vec<&str> = Vec::new();
    for part in rest.split('/') {
        match part {
            "" | "." => {}
            ".." => match parts.last() {
                Some(&last) if last != ".." => {
                    parts.pop();
                }
                _ if !root.is_empty() => {}
                _ => parts.push(".."),
            },
            p => parts.push(p),
        }
    }
    let joined = parts.join("/");
    match (root.is_empty(), joined.is_empty()) {
        (true, true) => ".".to_owned(),
        (true, false) => joined,
        (false, true) => root,
        (false, false) => {
            if root.ends_with('/') {
                format!("{root}{joined}")
            } else {
                format!("{root}/{joined}")
            }
        }
    }
}

/// What a unit read, gathered from its tools' records and placed by [`Roots`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Observed {
    /// The paths read, placed: the workspace's relative, known folders' under their names. One
    /// ending in `/` stands for everything under it.
    pub paths: BTreeSet<String>,
    /// Paths outside the workspace and every folder named (absolute, or climbing out with `../`):
    /// what the unit read there makes it less hermetic than its records alone say.
    pub outside: BTreeSet<String>,
    /// The environment variables read, each with the value the record gives (rustc's, placed as a
    /// path is when it's one), or `None`: unset when it was read, or a record that names it only.
    pub env: BTreeMap<String, Option<String>>,
    /// Records that don't list everything the unit read, and why: its path set needs the unit's
    /// declared inputs beside it.
    pub partial: BTreeSet<String>,
}

impl Observed {
    fn add(&mut self, roots: &Roots, base: &str, path: &str, tree: bool) {
        match roots.place(base, path) {
            Placed::In(p) => {
                self.paths.insert(if tree && !p.ends_with('/') { format!("{p}/") } else { p });
            }
            Placed::Outside(p) => {
                self.outside.insert(if tree && !p.ends_with('/') { format!("{p}/") } else { p });
            }
            Placed::Skipped => {}
        }
    }

    /// A variable read; a value only replaces what's known when it says more.
    fn var(&mut self, roots: &Roots, name: &str, value: Option<&str>) {
        let value = value.and_then(|v| {
            if !is_absolute(v, roots.windows) {
                return Some(v.to_owned());
            }
            match roots.place("", v) {
                Placed::In(p) | Placed::Outside(p) => Some(p),
                Placed::Skipped => None,
            }
        });
        let known = self.env.entry(name.to_owned()).or_insert(None);
        if value.is_some() {
            *known = value;
        }
    }

    /// A dependency file's inputs and variables, its relative paths under `base` (rustc's: the
    /// workspace's root, where cargo runs it).
    pub fn dep_info(&mut self, d: &DepInfo, base: &str, roots: &Roots) {
        for input in &d.inputs {
            self.add(roots, base, input, false);
        }
        for (name, value) in &d.env {
            self.var(roots, name, value.as_deref());
        }
    }

    /// A build script's directives; `package`: its package's folder, which its relative paths are
    /// under, and all of which it depends on when it gave no directive.
    pub fn build_script(&mut self, d: &Directives, package: &str, roots: &Roots) {
        for path in &d.changed {
            self.add(roots, package, path, true);
        }
        for name in &d.env {
            self.var(roots, name, None);
        }
        if d.whole_package() {
            self.add(roots, package, "", true);
        }
    }

    /// A `.tsbuildinfo`'s files; `file`: where it is (its paths are relative to its folder).
    pub fn tsbuildinfo(&mut self, t: &TsBuildInfo, file: &str, roots: &Roots) {
        let dir = file.rsplit_once(['/', '\\']).map_or("", |(dir, _)| dir);
        for f in &t.files {
            self.add(roots, dir, f, false);
        }
        if !t.complete {
            self.partial.insert(format!("{file}: a build that isn't incremental records only its roots, not what they import"));
        }
    }

    /// A Go test's log; `dir`: its package's folder, where the test ran. As cmd/go has it, what
    /// it read outside the folders named isn't counted (system files a test's runtime touches).
    pub fn go_test_log(&mut self, log: &TestLog, dir: &str, roots: &Roots) {
        let mut pwd = dir.to_owned();
        for (op, name) in &log.ops {
            match op {
                GoOp::Getenv => self.var(roots, name, None),
                GoOp::Chdir | GoOp::Open | GoOp::Stat => {
                    if let Placed::In(p) = roots.place(&pwd, name) {
                        self.paths.insert(p);
                    }
                    if *op == GoOp::Chdir {
                        pwd = join(&pwd, name, roots.windows);
                    }
                }
            }
        }
    }

    /// The test logs among what a go command asked its cache program for (each a `put`'s body);
    /// `dir`: the package folder its tests ran in. A stream from tests of several packages wants
    /// each package's logs given with its own folder ([`Observed::go_test_log`]).
    pub fn go_actions(&mut self, actions: &[GoAction], dir: &str, roots: &Roots) -> Result<(), String> {
        for a in actions {
            if let Some(body) = a.body.as_deref().filter(|b| b.starts_with(TEST_LOG.as_bytes())) {
                let text = std::str::from_utf8(body).map_err(|_| format!("request {}: a test log that isn't UTF-8", a.id))?;
                self.go_test_log(&go_test_log(text).map_err(|e| format!("request {}: {e}", a.id))?, dir, roots);
            }
        }
        Ok(())
    }

    /// Another unit's, or another record's, added to this.
    pub fn merge(&mut self, other: Observed) {
        self.paths.extend(other.paths);
        self.outside.extend(other.outside);
        for (k, v) in other.env {
            let known = self.env.entry(k).or_insert(None);
            if v.is_some() {
                *known = v;
            }
        }
        self.partial.extend(other.partial);
    }

    /// The names of the variables read, for [`super::key::relevant_env`]'s `read`.
    pub fn read_env(&self) -> BTreeSet<String> {
        self.env.keys().cloned().collect()
    }

    /// The path set a strong key takes: every path read, inside and outside, with what `now`
    /// says is there (a file's content digest, a folder's listing or, for a path ending in `/`, its
    /// whole tree, or that it's absent).
    pub fn path_set(&self, now: &dyn Fn(&str) -> Read) -> PathSet {
        self.paths.iter().chain(&self.outside).map(|p| (p.clone(), now(p))).collect()
    }
}

// ------------------------------------------------------------------ cargo's dependency files

/// A makefile-style dependency file: rustc's (`--emit=dep-info`), cargo's own beside an artifact,
/// or a C compiler's (`-MD`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DepInfo {
    /// What its rules make (rustc's: the `.d` itself, the `.rmeta`, the `.rlib`), each once.
    pub targets: Vec<String>,
    /// What they're made from, in order, each once: rustc's phony rules (`<input>:`) name them too.
    pub inputs: Vec<String>,
    /// rustc's `# env-dep:NAME=value` (`None`: `# env-dep:NAME`, unset when read).
    pub env: BTreeMap<String, Option<String>>,
    /// rustc's `# checksum:<algorithm>=<hex> file_len:<bytes> <path>` (`-Zchecksum-freshness`):
    /// each input's checksum and length.
    pub checksums: BTreeMap<String, (String, u64)>,
}

/// rustc's escapes in an `env-dep` line (`\n`, `\r`, `\\`) undone.
fn unescape_env(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// The words of a rule's side: split on whitespace, `\ ` (and a C compiler's `\#`) kept as the
/// character; any other backslash is a path's own (Windows').
fn words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if matches!(chars.peek(), Some(' ') | Some('#')) => {
                if let Some(next) = chars.next() {
                    word.push(next);
                }
            }
            c if c.is_whitespace() => {
                if !word.is_empty() {
                    out.push(std::mem::take(&mut word));
                }
            }
            c => word.push(c),
        }
    }
    if !word.is_empty() {
        out.push(word);
    }
    out
}

/// Where a rule's targets end: the first `:` that's followed by whitespace or ends the line and
/// isn't escaped (a Windows drive's colon is followed by its path).
fn separator(line: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        match b {
            b'\\' => escaped = matches!(bytes.get(i + 1), Some(b' ') | Some(b'#')),
            b':' if bytes.get(i + 1).is_none_or(|n| n.is_ascii_whitespace()) => return Some(i),
            _ => {}
        }
    }
    None
}

/// A dependency file, read. Lines ending in `\` go on, as make's do; comments other than rustc's
/// own are passed over; anything that's neither a rule nor a comment is an error.
pub fn dep_info(text: &str) -> Result<DepInfo, String> {
    let mut d = DepInfo::default();
    let (mut seen_targets, mut seen_inputs) = (BTreeSet::new(), BTreeSet::new());
    let mut logical = String::new();
    let mut first = 0;
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim_end_matches('\r');
        if logical.is_empty() {
            first = n + 1;
        }
        let trailing = line.len() - line.trim_end_matches('\\').len();
        if !line.starts_with('#') && trailing % 2 == 1 {
            logical.push_str(line.strip_suffix('\\').unwrap_or(line));
            logical.push(' ');
            continue;
        }
        logical.push_str(line);
        let line = std::mem::take(&mut logical);
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(c) = trimmed.strip_prefix('#') {
            let c = c.trim_start();
            if let Some(var) = c.strip_prefix("env-dep:") {
                let (name, value) = match var.split_once('=') {
                    Some((k, v)) => (k, Some(unescape_env(v))),
                    None => (var, None),
                };
                d.env.insert(unescape_env(name), value);
            } else if let Some(sum) = c.strip_prefix("checksum:") {
                let mut parts = sum.splitn(3, ' ');
                let (Some(hash), Some(len), Some(path)) = (parts.next(), parts.next(), parts.next()) else {
                    return Err(format!("line {first}: a checksum line is `# checksum:<algorithm>=<hex> file_len:<bytes> <path>`"));
                };
                let len = len.strip_prefix("file_len:").and_then(|l| l.parse::<u64>().ok()).ok_or_else(|| format!("line {first}: a checksum's `file_len:` isn't a number"))?;
                d.checksums.insert(words(path).join(" "), (hash.to_owned(), len));
            }
            continue;
        }
        let at = separator(trimmed).ok_or_else(|| format!("line {first}: neither a rule (`target: inputs`) nor a comment"))?;
        let (left, right) = (trimmed.get(..at).unwrap_or_default(), trimmed.get(at + 1..).unwrap_or_default());
        let (targets, prerequisites) = (words(left), words(right).into_iter().filter(|w| w != "|").collect::<Vec<_>>());
        if targets.is_empty() {
            return Err(format!("line {first}: a rule with no target"));
        }
        if prerequisites.is_empty() {
            // rustc's phony rule for each input, so make doesn't stop when one is gone.
            for t in targets {
                if !seen_targets.contains(&t) && seen_inputs.insert(t.clone()) {
                    d.inputs.push(t);
                }
            }
            continue;
        }
        for t in targets {
            if seen_targets.insert(t.clone()) {
                d.targets.push(t);
            }
        }
        for p in prerequisites {
            if seen_inputs.insert(p.clone()) {
                d.inputs.push(p);
            }
        }
    }
    if !logical.trim().is_empty() {
        return Err(format!("line {first}: the file ends inside a rule continued with `\\`"));
    }
    Ok(d)
}

// ------------------------------------------------------------------ build scripts

/// What a build script told cargo of what it depends on (its `output`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Directives {
    /// `rerun-if-changed`: files, or folders whose every file counts, relative to the package
    /// unless absolute; each once, in order.
    pub changed: Vec<String>,
    /// `rerun-if-env-changed`: each once, in order.
    pub env: Vec<String>,
}

impl Directives {
    /// With neither directive, cargo runs the script again when anything in its package changes.
    pub fn whole_package(&self) -> bool {
        self.changed.is_empty() && self.env.is_empty()
    }
}

/// A build script's output, read as cargo reads it: each line trimmed, `cargo::KEY=VALUE` or the
/// older `cargo:KEY=VALUE`; every other line (a C compiler's chatter, say) passed over.
pub fn build_script(output: &str) -> Directives {
    let mut d = Directives::default();
    for line in output.lines().map(str::trim) {
        let Some(data) = line.strip_prefix("cargo::").or_else(|| line.strip_prefix("cargo:")) else { continue };
        let Some((key, value)) = data.split_once('=') else { continue };
        match key {
            "rerun-if-changed" if !d.changed.iter().any(|c| c == value) => d.changed.push(value.to_owned()),
            "rerun-if-env-changed" if !d.env.iter().any(|e| e == value) => d.env.push(value.to_owned()),
            _ => {}
        }
    }
    d
}

// ------------------------------------------------------------------ TypeScript

/// What a `.tsbuildinfo` lists of a build's files.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TsBuildInfo {
    /// Relative to the folder the file is in.
    pub files: Vec<String>,
    /// Whether they're the whole program (an incremental build's `fileNames`), not only its roots.
    pub complete: bool,
    /// The TypeScript that wrote it.
    pub version: Option<String>,
}

/// A `.tsbuildinfo`, read: an incremental build's `fileNames` (TypeScript 5.6 on; `program`'s
/// before), or a build's `root` file names when it isn't incremental (5.6 on).
pub fn tsbuildinfo(json: &str) -> Result<TsBuildInfo, String> {
    let v: Json = serde_json::from_str(json).map_err(|e| format!("a .tsbuildinfo that isn't JSON: {e}"))?;
    let strings = |list: &Json, what: &str| -> Result<Vec<String>, String> {
        let list = list.as_array().ok_or_else(|| format!("`{what}` isn't a list"))?;
        list.iter().map(|f| f.as_str().map(str::to_owned).ok_or_else(|| format!("`{what}` lists something that isn't a file name"))).collect()
    };
    let version = v.get("version").and_then(Json::as_str).map(str::to_owned);
    if let Some(files) = v.pointer("/program/fileNames").or_else(|| v.get("fileNames")) {
        return Ok(TsBuildInfo { files: strings(files, "fileNames")?, complete: true, version });
    }
    match v.get("root") {
        Some(root) if root.as_array().is_some_and(|r| r.iter().all(Json::is_string)) => Ok(TsBuildInfo { files: strings(root, "root")?, complete: false, version }),
        _ => Err("a .tsbuildinfo with no file list (`fileNames`, `program.fileNames` or `root`'s names)".into()),
    }
}

// ------------------------------------------------------------------ Go

/// The magic a Go test log starts with (cmd/go's `testlogMagic`).
const TEST_LOG: &str = "# test log\n";

/// One request a go command made of its cache program, as `GOCACHEPROG`'s protocol has it: a JSON
/// object a line (`ID`, `Command`, `ActionID`, `OutputID`, `BodySize`, its byte strings in
/// base64), a `put`'s body after it on its own line as a base64 JSON string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoAction {
    pub id: i64,
    /// `get`, `put` or `close`.
    pub command: String,
    /// The action's id, in hex.
    pub action: Option<String>,
    /// What a `put` stores under it, in hex (`ObjectID` before Go 1.24).
    pub output: Option<String>,
    pub body: Option<Vec<u8>>,
}

fn b64(v: &Json, field: &str, id: i64) -> Result<Option<Vec<u8>>, String> {
    match v.get(field) {
        None | Some(Json::Null) => Ok(None),
        Some(Json::String(s)) => base64::engine::general_purpose::STANDARD.decode(s).map(Some).map_err(|_| format!("request {id}: `{field}` isn't base64")),
        Some(_) => Err(format!("request {id}: `{field}` isn't a string")),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The requests a go command sent its cache program, read in order.
pub fn go_actions(stream: &str) -> Result<Vec<GoAction>, String> {
    let mut out = Vec::new();
    let mut lines = stream.lines().map(str::trim).filter(|l| !l.is_empty());
    while let Some(line) = lines.next() {
        let v: Json = serde_json::from_str(line).map_err(|e| format!("a request that isn't JSON ({e}): {line}"))?;
        let id = v.get("ID").and_then(Json::as_i64).ok_or_else(|| format!("a request with no `ID`: {line}"))?;
        let command = v.get("Command").and_then(Json::as_str).ok_or_else(|| format!("request {id}: no `Command`"))?.to_owned();
        let action = b64(&v, "ActionID", id)?.map(|b| hex(&b));
        let output = match b64(&v, "OutputID", id)? {
            Some(b) => Some(hex(&b)),
            None => b64(&v, "ObjectID", id)?.map(|b| hex(&b)),
        };
        let size = v.get("BodySize").and_then(Json::as_u64).unwrap_or(0);
        let body = if command == "put" && size > 0 {
            let next = lines.next().ok_or_else(|| format!("request {id}: a put of {size} bytes with no body after it"))?;
            let encoded: String = serde_json::from_str(next).map_err(|_| format!("request {id}: its body isn't a JSON string"))?;
            let bytes = base64::engine::general_purpose::STANDARD.decode(encoded).map_err(|_| format!("request {id}: its body isn't base64"))?;
            if bytes.len() as u64 != size {
                return Err(format!("request {id}: a body of {} bytes, not the {size} it said", bytes.len()));
            }
            Some(bytes)
        } else {
            None
        };
        out.push(GoAction { id, command, action, output, body });
    }
    Ok(out)
}

/// What a Go test did that its result depends on, in its log's words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoOp {
    /// A variable read.
    Getenv,
    /// A file opened, or a folder listed.
    Open,
    /// A path looked at.
    Stat,
    /// Its working folder changed (to an absolute path).
    Chdir,
}

/// A Go test's log of what it read, in order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TestLog {
    pub ops: Vec<(GoOp, String)>,
}

/// A test log, read as cmd/go reads it (`computeTestInputsID`): the magic line, then `<op>
/// <name>` lines. Anything else is malformed, as it is to cmd/go, which caches nothing then.
pub fn go_test_log(text: &str) -> Result<TestLog, String> {
    let body = text.strip_prefix(TEST_LOG).ok_or("a test log starts with `# test log`")?;
    let mut log = TestLog::default();
    for line in body.lines().filter(|l| !l.is_empty()) {
        let (op, name) = line.split_once(' ').ok_or_else(|| format!("a test log line with no name: {line:?}"))?;
        let op = match op {
            "getenv" => GoOp::Getenv,
            "open" => GoOp::Open,
            "stat" => GoOp::Stat,
            "chdir" => GoOp::Chdir,
            other => return Err(format!("a test log's `{other}`: getenv, open, stat or chdir")),
        };
        log.ops.push((op, name.to_owned()));
    }
    Ok(log)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work::key::{self, Kind, Platform, Unit};

    macro_rules! fixture {
        ($name:literal) => {
            include_str!(concat!("../../tests/fixtures/work/pathset/", $name))
        };
    }

    /// The machine the fixtures come from (its home folder renamed): a workspace, cargo's home and
    /// a target folder outside the workspace.
    fn roots() -> Roots {
        Roots::workspace("/home/runner/Projects/kiln-w24-m0").name("/home/runner/.cargo", "$CARGO_HOME").name("/home/runner/.cache/w24/bench/target", "$TARGET")
    }

    /// W24-R04: rustc's dependency file for a registry crate, from a cargo target folder after a
    /// build: its sources under cargo's home (one written `src/./with_std.rs`), the files its build
    /// script generated in `OUT_DIR`, and the variable `env!` read.
    #[test]
    fn w24_r04_rustc_dep_info_of_a_registry_crate() -> Result<(), String> {
        let d = dep_info(fixture!("bigdecimal.d"))?;
        assert_eq!(d.targets.len(), 3, "the .d, the .rlib and the .rmeta: {:?}", d.targets);
        assert!(d.targets.iter().all(|t| t.contains("bigdecimal-7fd7264018b3c28a")));
        assert_eq!(d.inputs.len(), 38);
        assert!(d.checksums.is_empty());
        let mut o = Observed::default();
        o.dep_info(&d, "/home/runner/Projects/kiln-w24-m0", &roots());
        assert!(o.outside.is_empty(), "{:?}", o.outside);
        assert_eq!(o.paths.len(), 38);
        let src = "$CARGO_HOME/registry/src/index.crates.io-1949cf8c6b5b557f/bigdecimal-0.4.11/src";
        assert!(o.paths.contains(&format!("{src}/lib.rs")));
        assert!(o.paths.contains(&format!("{src}/with_std.rs")), "`./` made plain");
        assert!(o.paths.contains("$TARGET/debug/build/bigdecimal-10ed25bddfe432d0/out/default_precision.rs"));
        assert_eq!(o.env, BTreeMap::from([("OUT_DIR".to_owned(), Some("$TARGET/debug/build/bigdecimal-10ed25bddfe432d0/out".to_owned()))]), "a path's value placed as a path is (R05)");
        // A registry left out (Cargo.lock's checksums fix its sources): only the generated files stay.
        let mut lean = Observed::default();
        lean.dep_info(&d, "", &roots().skip("/home/runner/.cargo/registry"));
        assert_eq!(lean.paths.len(), 3, "{:?}", lean.paths);
        Ok(())
    }

    /// W24-R04: a workspace crate's, from a clippy build of its tests: workspace paths as cargo
    /// passed them (relative to the workspace's root), `include_str!`'s files among them, one
    /// through `../..`; a variable with a value, one with a path as its value, one unset.
    #[test]
    fn w24_r04_rustc_dep_info_of_a_workspace_crate() -> Result<(), String> {
        let d = dep_info(fixture!("kiln_ci.d"))?;
        assert_eq!(d.targets.len(), 2, "a check build: the .d and the .rmeta");
        assert_eq!(d.inputs.len(), 102);
        let mut o = Observed::default();
        o.dep_info(&d, "", &roots());
        assert_eq!(o.paths.len(), 102);
        assert!(o.paths.contains("crates/kiln-ci/src/work/key.rs"));
        assert!(o.paths.contains("crates/kiln-ci/src/trigger/schemas/octokit-webhooks-7.6.1.json"), "include_str!'s file");
        assert!(o.paths.contains("crates/kiln-ci/tests/fixtures/results/surefire.xml"), "`src/results/../../tests/…` made plain");
        assert!(o.paths.iter().all(|p| !p.contains("..") && !p.starts_with('/')));
        assert_eq!(o.env.get("CARGO_MANIFEST_DIR"), Some(&Some("crates/kiln-ci".to_owned())), "the workspace's own folder, relative");
        assert_eq!(o.env.get("CLIPPY_ARGS"), Some(&Some("-D__CLIPPY_HACKERY__warnings__CLIPPY_HACKERY__".to_owned())));
        assert_eq!(o.env.get("CLIPPY_CONF_DIR"), Some(&None), "unset when read");
        let small = dep_info(fixture!("agents_md.d"))?;
        assert_eq!(small.inputs, ["crates/kiln/tests/agents_md.rs", "Cargo.toml"]);
        assert_eq!(small.env.len(), 3);
        Ok(())
    }

    /// W24-R04: what else dependency files hold: cargo's own (one rule, absolute paths, spaces
    /// escaped), rules continued over lines (a C compiler's), Windows paths, rustc's escapes and
    /// checksums, a variable set to nothing; and what isn't one.
    #[test]
    fn w24_r04_dep_info_forms() -> Result<(), String> {
        let cargo = "/w/target/debug/kiln: /w/crates/kiln/build.rs /w/crates/kiln/src/a\\ b.rs /w/Cargo.lock\n";
        let d = dep_info(cargo)?;
        assert_eq!(d.targets, ["/w/target/debug/kiln"]);
        assert_eq!(d.inputs, ["/w/crates/kiln/build.rs", "/w/crates/kiln/src/a b.rs", "/w/Cargo.lock"]);
        let gcc = "out/foo.o: src/foo.c \\\n  include/foo.h \\\n  include/bar\\#1.h\n";
        assert_eq!(dep_info(gcc)?.inputs, ["src/foo.c", "include/foo.h", "include/bar#1.h"]);
        let windows = "C:\\w\\target\\debug\\deps\\app-1.d: src\\main.rs C:\\Users\\r\\.cargo\\registry\\src\\x\\lib.rs\n\nsrc\\main.rs:\n";
        let w = dep_info(windows)?;
        assert_eq!(w.targets, ["C:\\w\\target\\debug\\deps\\app-1.d"]);
        let mut o = Observed::default();
        o.dep_info(&w, "C:\\w", &Roots::workspace("C:\\w").name("C:\\Users\\r\\.cargo", "$CARGO_HOME").on_windows());
        assert_eq!(o.paths, BTreeSet::from(["src/main.rs".to_owned(), "$CARGO_HOME/registry/src/x/lib.rs".to_owned()]));
        let rustc = "a.d: src/lib.rs\n\nsrc/lib.rs:\n\n# env-dep:KILN_COMMIT=\n# env-dep:NOTE=line\\nnext \\\\ end\n# checksum:sha256=00ff file_len:12 src/lib.rs\n# a comment of someone else's\n";
        let r = dep_info(rustc)?;
        assert_eq!(r.env.get("KILN_COMMIT"), Some(&Some(String::new())), "set to nothing isn't unset");
        assert_eq!(r.env.get("NOTE"), Some(&Some("line\nnext \\ end".to_owned())));
        assert_eq!(r.checksums.get("src/lib.rs"), Some(&("sha256=00ff".to_owned(), 12)));
        assert!(dep_info("just words\n").is_err_and(|e| e.contains("line 1")));
        assert!(dep_info("a.d: x \\\n").is_err(), "ends inside a rule");
        assert!(dep_info("a.d: b\n# checksum:sha256=1 file_len:x b\n").is_err());
        Ok(())
    }

    /// W24-R04: build scripts' directives in both forms, among a C compiler's lines (blake3's);
    /// absolute paths and a folder (kiln's own, its migrations); and a script with none, which
    /// depends on its whole package (crc32fast's).
    #[test]
    fn w24_r04_build_script_directives() {
        let blake3 = build_script(fixture!("blake3.output"));
        assert_eq!(blake3.env, ["CARGO_FEATURE_PURE", "CARGO_FEATURE_NO_NEON", "CC_ENABLE_DEBUG_OUTPUT", "CARGO_FEATURE_PREFER_INTRINSICS", "CARGO_FEATURE_NEON", "CC", "CFLAGS"], "`cargo::` and `cargo:` alike, each once");
        assert_eq!(blake3.changed.len(), 36);
        assert_eq!(blake3.changed.first().map(String::as_str), Some("c/.gitignore"));
        let package = "/home/runner/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/blake3-1.8.7";
        let mut o = Observed::default();
        o.build_script(&blake3, package, &roots());
        assert!(o.paths.contains("$CARGO_HOME/registry/src/index.crates.io-1949cf8c6b5b557f/blake3-1.8.7/c/blake3.c/"), "every path stands for what's under it");
        assert!(o.paths.contains("$CARGO_HOME/registry/src/index.crates.io-1949cf8c6b5b557f/blake3-1.8.7/c/cmake/"));
        assert_eq!(o.env.get("CC"), Some(&None));

        let kiln = build_script(fixture!("kiln.output"));
        assert_eq!(kiln.env, ["KILN_COMMIT", "KILN_UI_SERVER", "KILN_UI_EMBED_DIR"]);
        assert!(!kiln.whole_package());
        let mut k = Observed::default();
        k.build_script(&kiln, "crates/kiln", &roots());
        assert!(k.paths.contains("crates/kiln/build.rs/"));
        assert!(k.paths.contains("crates/kiln/src/store/migrations/"), "an absolute path in the workspace, made relative");
        assert!(k.paths.contains("crates/kiln/src/store/migrations/20261017-0000_ci_baseline.sql/"));
        assert_eq!(k.outside, BTreeSet::from(["/home/runner/Projects/kiln/.git/packed-refs/".to_owned(), "/home/runner/Projects/kiln/.git/refs/heads/ci-w24-m0-reach/".to_owned(), "/home/runner/Projects/kiln/.git/worktrees/kiln-w24-m0/HEAD/".to_owned()]), "the main checkout's git files, outside the worktree");

        let none = build_script(fixture!("crc32fast.output"));
        assert!(none.whole_package());
        let mut c = Observed::default();
        c.build_script(&none, "/home/runner/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/crc32fast-1.5.0", &roots());
        assert_eq!(c.paths, BTreeSet::from(["$CARGO_HOME/registry/src/index.crates.io-1949cf8c6b5b557f/crc32fast-1.5.0/".to_owned()]));
        let mut root = Observed::default();
        root.build_script(&none, "", &roots());
        assert_eq!(root.paths, BTreeSet::from(["./".to_owned()]), "a root package's script: the whole workspace");
    }

    /// W24-R04: TypeScript 6's build info, from `tsc -b`: an incremental project's whole program
    /// (its lib files in node_modules among them) and a project that isn't incremental, which
    /// records its roots only; and the shape before TypeScript 5.6.
    #[test]
    fn w24_r04_tsbuildinfo() -> Result<(), String> {
        let inc = tsbuildinfo(fixture!("tsconfig.inc.tsbuildinfo"))?;
        assert!(inc.complete);
        assert_eq!(inc.version.as_deref(), Some("6.0.3"));
        assert_eq!(inc.files.len(), 64);
        let mut o = Observed::default();
        o.tsbuildinfo(&inc, "ui/node_modules/.tmp/tsconfig.inc.tsbuildinfo", &roots());
        assert_eq!(o.paths.len(), 64);
        assert!(o.paths.contains("ui/node_modules/typescript/lib/lib.es5.d.ts"));
        assert!(o.paths.contains("ui/src/lib/util.ts") && o.paths.contains("ui/src/main.ts") && o.paths.contains("ui/src/env.d.ts"));
        assert!(o.partial.is_empty());
        let app = tsbuildinfo(fixture!("tsconfig.app.tsbuildinfo"))?;
        assert!(!app.complete);
        let mut a = Observed::default();
        a.tsbuildinfo(&app, "ui/node_modules/.tmp/tsconfig.app.tsbuildinfo", &roots());
        assert_eq!(a.paths, BTreeSet::from(["ui/src/env.d.ts".to_owned(), "ui/src/lib/util.ts".to_owned(), "ui/src/main.ts".to_owned()]));
        assert_eq!(a.partial.len(), 1, "roots only: the unit's declared inputs must cover the rest");
        assert!(a.partial.iter().all(|p| p.starts_with("ui/node_modules/.tmp/tsconfig.app.tsbuildinfo: ")));
        let old = tsbuildinfo(r#"{"program":{"fileNames":["../node_modules/typescript/lib/lib.d.ts","../src/a.ts"],"fileInfos":[]},"version":"5.4.5"}"#)?;
        assert_eq!(old.files, ["../node_modules/typescript/lib/lib.d.ts", "../src/a.ts"]);
        assert!(tsbuildinfo("{}").is_err() && tsbuildinfo("[").is_err() && tsbuildinfo(r#"{"fileNames":[1]}"#).is_err());
        Ok(())
    }

    /// W24-R04: what a go command (1.26) sent a `GOCACHEPROG` as `go test` ran a package's test,
    /// recorded: puts with and without bodies, gets, the test's log (twice, under both its keys)
    /// and its output, then close. The log is Go's own record of what the test read.
    #[test]
    fn w24_r04_go_actions_and_test_logs() -> Result<(), String> {
        let actions = go_actions(fixture!("go-cacheprog.jsonl"))?;
        assert_eq!(actions.len(), 15);
        assert_eq!(actions.iter().filter(|a| a.command == "get").count(), 3);
        assert_eq!(actions.last().map(|a| a.command.as_str()), Some("close"));
        let empty = actions.first().ok_or("no actions")?;
        assert_eq!((empty.body.as_ref(), empty.output.as_deref()), (None, Some("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")), "a put of nothing: SHA-256 of nothing");
        let logs: Vec<&GoAction> = actions.iter().filter(|a| a.body.as_deref().is_some_and(|b| b.starts_with(TEST_LOG.as_bytes()))).collect();
        assert_eq!(logs.len(), 2);
        let mut o = Observed::default();
        o.go_actions(&actions, "greet", &roots())?;
        assert_eq!(o.paths, BTreeSet::from(["greet/testdata/name.txt".to_owned(), "greet/testdata/optional.txt".to_owned()]));
        assert_eq!(o.read_env(), BTreeSet::from(["GREETING_STYLE".to_owned()]));
        // chdir moves what's relative; a system file is Go's rule's to leave out.
        let log = go_test_log("# test log\nopen /etc/hosts\nchdir /home/runner/Projects/kiln-w24-m0/greet/testdata\nopen name.txt\nstat ../missing\n")?;
        let mut c = Observed::default();
        c.go_test_log(&log, "greet", &roots());
        assert_eq!(c.paths, BTreeSet::from(["greet/missing".to_owned(), "greet/testdata".to_owned(), "greet/testdata/name.txt".to_owned()]));
        assert!(c.outside.is_empty());
        assert!(go_test_log("open x\n").is_err() && go_test_log("# test log\nexec x\n").is_err() && go_test_log("# test log\nopen\n").is_err());
        assert!(go_actions("{\"ID\":1,\"Command\":\"put\",\"ActionID\":\"AA==\",\"OutputID\":\"AA==\",\"BodySize\":4}\n\"AAA=\"\n").is_err_and(|e| e.contains("not the 4")));
        assert!(go_actions("{\"ID\":1,\"Command\":\"put\",\"BodySize\":4}\n").is_err());
        assert!(go_actions("not json\n").is_err());
        Ok(())
    }

    /// W24-R04, R05: a unit's path set and the variables it read, from its records, give its
    /// strong key: a change to a file it read changes the key, one to a file it didn't doesn't, nor
    /// does where the workspace is or what's in a temporary folder.
    #[test]
    fn w24_r04_path_sets_key_what_was_read() -> Result<(), String> {
        let digests: BTreeMap<&str, &str> = BTreeMap::from([("crates/kiln/tests/agents_md.rs", "a1"), ("Cargo.toml", "c1"), ("crates/kiln/build.rs/", "b1"), ("README.md", "r1")]);
        let now = |edit: Option<(&'static str, &'static str)>| {
            let mut d = digests.clone();
            if let Some((k, v)) = edit {
                d.insert(k, v);
            }
            move |p: &str| d.get(p).map_or(Read::Absent, |v| Read::File((*v).to_owned()))
        };
        let observe = |workspace: &str, tmp: &str| -> Result<Observed, String> {
            let roots = Roots::workspace(workspace).skip(tmp);
            let mut o = Observed::default();
            o.dep_info(&dep_info(&fixture!("agents_md.d").replace("/home/runner/Projects/kiln-w24-m0", workspace))?, "", &roots);
            o.build_script(&build_script(&format!("cargo:rerun-if-changed=build.rs\ncargo:rerun-if-changed={tmp}/scratch.rs\ncargo::rerun-if-env-changed=PROFILE_NAME\n")), "crates/kiln", &roots);
            Ok(o)
        };
        let o = observe("/home/runner/Projects/kiln-w24-m0", "/tmp/run1")?;
        assert_eq!(o.paths, BTreeSet::from(["Cargo.toml".to_owned(), "crates/kiln/build.rs/".to_owned(), "crates/kiln/tests/agents_md.rs".to_owned()]), "the temporary file left out");
        let unit = Unit { kind: Kind::Step, definition: "cargo clippy".into(), secrets: BTreeSet::new(), env: BTreeMap::new(), platform: Platform::default(), inputs: BTreeMap::new(), upstream: BTreeMap::new(), trust: "trusted".into() };
        let all = BTreeMap::from([("PROFILE_NAME".to_owned(), "fast".to_owned()), ("UNREAD".to_owned(), "x".to_owned())]);
        assert_eq!(o.read_env(), BTreeSet::from(["CARGO_MANIFEST_DIR".to_owned(), "CLIPPY_ARGS".to_owned(), "CLIPPY_CONF_DIR".to_owned(), "PROFILE_NAME".to_owned()]));
        let env = key::relevant_env(&all, &BTreeSet::new(), &[], &unit.definition, &o.read_env());
        assert_eq!(env.keys().map(String::as_str).collect::<Vec<_>>(), ["PROFILE_NAME"], "a variable seen read, from the step's environment");
        let weak = key::weak(&Unit { env, ..unit });
        let strong = |o: &Observed, edit| key::strong(&weak, &o.path_set(&now(edit)));
        let base = strong(&o, None);
        assert_ne!(base, strong(&o, Some(("crates/kiln/tests/agents_md.rs", "a2"))), "a file it read");
        assert_ne!(base, strong(&o, Some(("crates/kiln/build.rs/", "b2"))), "what its build script named");
        assert_eq!(base, strong(&o, Some(("README.md", "r2"))), "a file it didn't read");
        let elsewhere = observe("/srv/kiln/warm/acme/lane/3", "/tmp/run2")?;
        assert_eq!(elsewhere, o, "another workspace and temporary folder record the same");
        assert_eq!(base, strong(&elsewhere, None));
        Ok(())
    }

    /// W24-R04: paths placed and put back: relative, under named folders, outside, left out, and
    /// the workspace's own root.
    #[test]
    fn w24_r04_roots_place_and_resolve() {
        let r = roots().skip("/tmp");
        assert_eq!(r.place("", "a/./b/../c.rs"), Placed::In("a/c.rs".into()));
        assert_eq!(r.place("crates/x", "../../../etc"), Placed::Outside("../etc".into()));
        assert_eq!(r.place("", "/home/runner/.cargo/bin/x"), Placed::In("$CARGO_HOME/bin/x".into()));
        assert_eq!(r.place("", "/home/runner/Projects/kiln-w24-m0"), Placed::In(".".into()));
        assert_eq!(r.place("", "/home/runner/Projects/kiln-w24-m0x/a"), Placed::Outside("/home/runner/Projects/kiln-w24-m0x/a".into()), "a folder's name, not every name it starts");
        assert_eq!(r.place("/tmp/x", "y"), Placed::Skipped);
        assert_eq!(r.place("", "/../../etc//passwd"), Placed::Outside("/etc/passwd".into()));
        assert_eq!(r.resolve("$CARGO_HOME/bin/x").as_deref(), Some("/home/runner/.cargo/bin/x"));
        assert_eq!(r.resolve("crates/kiln/build.rs/").as_deref(), Some("/home/runner/Projects/kiln-w24-m0/crates/kiln/build.rs/"));
        assert_eq!(r.resolve("./").as_deref(), Some("/home/runner/Projects/kiln-w24-m0/"));
        assert_eq!(r.resolve("/etc/passwd").as_deref(), Some("/etc/passwd"));
        assert_eq!(Roots::default().resolve("a/b"), None, "no workspace to put it under");
        assert_eq!(normal("C:\\a\\..\\b", true), "C:/b");
    }
}
