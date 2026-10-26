//! Step reuse (docs/ci.md W24-R07): a step's `memo:`. A step that says `memo: check` (nothing to
//! restore: lint, tests) or `memo: { outputs: [paths] }` is looked up in the work ledger before it
//! runs, under its work key ([`super::key`], a [`Kind::Step`] unit), and a recorded success is
//! replayed instead of running it: its log, exit code, outputs and test reports, and the files
//! its `outputs` name, kept as trees in the content store and put in place only when a later step
//! misses. Only successes are recorded; a failure, a cancellation or a timeout never is.
//!
//! Pure: the runner gathers what a step's key covers and this module reads `memo:` ([`Memo`]),
//! makes the step's unit ([`unit`]): the step evaluated ([`Definition`]), the environment the
//! rules keep, the platform, its inputs ([`inputs`]: the git trees of the paths it declares, else
//! the whole tree), what earlier memo steps and the job's upstream jobs produced (by digest, W24-R03)
//! and its trust. It also says what a success leaves for the steps after it ([`Replay`], as the
//! ledger keeps it) and shapes the files a step keeps as the Remote Execution API's trees
//! ([`Tree`], W24's first decision).
//!
//! What a key covers is what the step declares: the files earlier steps that aren't memo steps
//! leave in the workspace are not among its inputs unless git sees them (what `.gitignore` leaves
//! out it doesn't), and the toolchains' identities wait for W24-R01's probes. A step that receives
//! secrets isn't reused (W24-R05); the runner says why.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::key::{self, Kind, Platform, Unit};
use crate::trigger::pattern::Pattern;
use crate::wf::ir::YVal;

/// How long a step's result stays reusable unless it says otherwise: a job's `reuse:`'s (W08-R10).
pub use crate::plan::reuse::MAX_AGE;

/// The variables toolchains read that change what they produce (W24-R01): a step's key keeps them
/// whether or not the workflow declares them. Paths (`CARGO_HOME`, `GOPATH`) are left out: they
/// differ from runner to runner and say where, not what.
pub const TOOLCHAIN_ENV: &[&str] = &[
    "RUSTFLAGS",
    "RUSTDOCFLAGS",
    "RUSTC_BOOTSTRAP",
    "RUSTUP_TOOLCHAIN",
    "CARGO_ENCODED_RUSTFLAGS",
    "CARGO_INCREMENTAL",
    "CARGO_BUILD_*",
    "CARGO_PROFILE_*",
    "GOFLAGS",
    "GOOS",
    "GOARCH",
    "GOEXPERIMENT",
    "CGO_ENABLED",
    "NODE_ENV",
    "NODE_OPTIONS",
    "CC",
    "CXX",
    "CFLAGS",
    "CXXFLAGS",
    "LDFLAGS",
];

/// A step's `memo:`: `check`, or `{ inputs, outputs, env, max-age, secrets }`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Memo {
    /// Paths and globs, from the repository's root, whose git trees the step depends on; the whole
    /// tree when there are none.
    pub inputs: Vec<String>,
    /// Paths in the workspace the step leaves for the steps after it, kept as one tree.
    pub outputs: Vec<String>,
    /// Variables it depends on besides those the workflow declares (W24-R01's `memo.env`).
    pub env: Vec<String>,
    /// Seconds a result stays reusable.
    pub max_age: u64,
    /// `secrets: hmac` (W24-R05): a step that receives secrets keyed by their HMAC. Not built yet:
    /// such a step still runs.
    pub hmac: bool,
}

impl Memo {
    /// A step's `memo:`; `None` when it doesn't opt in (`false`). Keys `memo` doesn't have are the
    /// front end's to report (KCI1010), so they are left alone here.
    pub fn from_yval(v: &YVal) -> Result<Option<Memo>, String> {
        let list = |k: &str, check: fn(&str) -> Result<String, String>| -> Result<Vec<String>, String> {
            let items: Vec<&str> = match v.get(k) {
                None | Some(YVal::Null) => Vec::new(),
                Some(YVal::Seq(items)) => items.iter().map(|i| i.as_str().ok_or_else(|| format!("`memo.{k}` lists strings"))).collect::<Result<_, _>>()?,
                Some(YVal::Str(t)) => t.src.lines().map(str::trim).filter(|l| !l.is_empty()).collect(),
                Some(_) => return Err(format!("`memo.{k}` is a list")),
            };
            items.into_iter().map(|i| check(i).map_err(|e| format!("`memo.{k}`: {e}"))).collect()
        };
        match v {
            YVal::Null | YVal::Bool(false) => Ok(None),
            YVal::Str(t) if t.src.trim() == "check" => Ok(Some(Memo { max_age: MAX_AGE, ..Memo::default() })),
            YVal::Map(_) => {
                let max_age = match v.get("max-age") {
                    None => MAX_AGE,
                    Some(YVal::Num(n)) if *n >= 0.0 => *n as u64,
                    Some(a) => a.as_str().and_then(crate::plan::reuse::parse_age).ok_or("`memo.max-age` is a duration (`14d`, `12h`)")?,
                };
                let hmac = match v.get("secrets") {
                    None | Some(YVal::Null) => false,
                    Some(s) if s.as_str() == Some("hmac") => true,
                    Some(_) => return Err("`memo.secrets` is `hmac`".into()),
                };
                let env = list("env", |n| if n.is_empty() || n.contains('=') { Err(format!("`{n}` isn't a variable's name")) } else { Ok(n.to_owned()) })?;
                Ok(Some(Memo { inputs: list("inputs", |p| path(p, true))?, outputs: list("outputs", |p| path(p, false))?, env, max_age, hmac }))
            }
            _ => Err("`memo` is `check` or `{ inputs, outputs, env, max-age }`".into()),
        }
    }
}

/// A path in the workspace as `memo:` takes one: relative, `/`-separated, never above the
/// workspace (no `..`), without its `./` and trailing `/`. Inputs may be globs.
fn path(p: &str, glob: bool) -> Result<String, String> {
    let p = p.trim();
    let p = p.strip_prefix("./").unwrap_or(p).trim_end_matches('/');
    let outside = p.starts_with('/') || p.starts_with('~') || p.contains('\\') || p.get(1..2) == Some(":") || p.split('/').any(|s| s.is_empty() || s == "." || s == "..");
    if p.is_empty() || outside {
        return Err(format!("`{p}` isn't a path in the workspace"));
    }
    if !glob && p.contains(['*', '?', '[']) {
        return Err(format!("`{p}` is a glob: name the paths themselves"));
    }
    Ok(p.to_owned())
}

/// The step as its key sees it: evaluated, with the leg's own paths put as names ([`unplaced`]),
/// so the same step keys alike in every leg and on every runner. Its name, `id` and `if` aren't
/// here: they don't change what it does.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Definition {
    pub uses: Option<String>,
    /// The action `uses` names, as pinned when the run started (`repo@sha:path`).
    pub pin: Option<String>,
    pub run: Option<String>,
    pub shell: Option<String>,
    pub with: BTreeMap<String, String>,
    /// Its own `env:`, evaluated.
    pub env: BTreeMap<String, String>,
    /// Where it runs, from the workspace.
    pub working_directory: String,
    /// Its `test-results:`, as written (JSON).
    pub test_results: Option<String>,
    /// Its `memo.outputs`: a result keeps these paths' files.
    pub outputs: Vec<String>,
}

/// `text` with each of the leg's own paths (its workspace, its temp, …: `(path, name)`) put as
/// `${name}`, the longest first.
pub fn unplaced(text: &str, places: &[(String, &str)]) -> String {
    let mut sorted: Vec<&(String, &str)> = places.iter().filter(|(p, _)| !p.is_empty()).collect();
    sorted.sort_by_key(|(p, _)| std::cmp::Reverse(p.len()));
    sorted.into_iter().fold(text.to_owned(), |t, (p, name)| t.replace(p.as_str(), &format!("${{{name}}}")))
}

/// What a step's key covers, as the runner found it before the step runs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Facts {
    pub definition: Definition,
    /// Every variable the step gets: the job's and its own, kiln's and GitHub's, the runner's
    /// toolchain variables.
    pub env: BTreeMap<String, String>,
    /// The variables the workflow declares for it (the workflow's, the job's and its own `env:`,
    /// `env-from`), those earlier steps set (`$GITHUB_ENV`) and its `memo.env`.
    pub declared: BTreeSet<String>,
    pub platform: Platform,
    /// Its [`inputs`].
    pub inputs: BTreeMap<String, String>,
    /// What came before it, by digest (W24-R03): earlier memo steps' results (`step.<n>`) and the
    /// job's upstream jobs' outputs (`needs`).
    pub upstream: BTreeMap<String, String>,
    pub trusted: bool,
}

/// A step's unit (W24-R01): its definition as canonical JSON, the environment
/// [`key::relevant_env`] keeps of it, and the rest as found. Its weak key is the step's work key.
pub fn unit(f: &Facts) -> Unit {
    let definition = serde_json::to_string(&f.definition).unwrap_or_default();
    let env = key::relevant_env(&f.env, &f.declared, TOOLCHAIN_ENV, &definition, &BTreeSet::new());
    Unit {
        kind: Kind::Step,
        definition,
        secrets: BTreeSet::new(),
        env,
        platform: f.platform.clone(),
        inputs: f.inputs.clone(),
        upstream: f.upstream.clone(),
        trust: (if f.trusted { "trusted" } else { "untrusted" }).to_owned(),
    }
}

/// Which paths a pattern names: a glob, or a path and everything below it.
struct Matcher {
    plain: Option<String>,
    glob: Option<Pattern>,
}

impl Matcher {
    fn new(p: &str) -> Result<Matcher, String> {
        if p.contains(['*', '?', '[']) {
            return Pattern::new(p).map(|g| Matcher { plain: None, glob: Some(g) }).map_err(|e| e.to_string());
        }
        Ok(Matcher { plain: Some(p.to_owned()), glob: None })
    }

    fn matches(&self, path: &str) -> bool {
        match (&self.plain, &self.glob) {
            (Some(p), _) => under(path, p),
            (None, Some(g)) => g.matches(path),
            (None, None) => false,
        }
    }
}

/// Whether `path` is `dir` or below it.
pub fn under(path: &str, dir: &str) -> bool {
    path == dir || path.strip_prefix(dir).is_some_and(|rest| rest.starts_with('/'))
}

/// SHA-256 of lines, each length-prefixed, as hex.
fn digest_lines<I: IntoIterator<Item = String>>(lines: I) -> String {
    let mut h = Sha256::new();
    for l in lines {
        h.update((l.len() as u64).to_be_bytes());
        h.update(l.as_bytes());
    }
    hex(&h.finalize())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A step's inputs (W24-R01): for each path or glob it declares, a digest of its git objects in
/// the tree the step sees (`listing`: `git ls-tree -r` of `HEAD`, a `<mode> <type> <object>\t<path>`
/// line each) and of what differs from that tree there (`changed`: a path to its content's digest,
/// or `-` when it's gone); with none declared, the whole tree, as `/`: `HEAD`'s tree id (`tree`)
/// when nothing differs. Keys take tree ids, never commits.
pub fn inputs(declared: &[String], tree: &str, listing: &str, changed: &BTreeMap<String, String>) -> Result<BTreeMap<String, String>, String> {
    let dirty = |m: &dyn Fn(&str) -> bool| changed.iter().filter(|(p, _)| m(p)).map(|(p, d)| format!("{d}\t{p}")).collect::<Vec<_>>();
    if declared.is_empty() {
        let all = dirty(&|_| true);
        let v = if all.is_empty() { tree.to_owned() } else { digest_lines(std::iter::once(format!("tree {tree}")).chain(all)) };
        return Ok(BTreeMap::from([("/".to_owned(), v)]));
    }
    let mut out = BTreeMap::new();
    for p in declared {
        let m = Matcher::new(p)?;
        let committed = listing.lines().filter(|l| l.split_once('\t').is_some_and(|(_, path)| m.matches(path))).map(str::to_owned);
        let lines: Vec<String> = committed.chain(std::iter::once("--".to_owned())).chain(dirty(&|path| m.matches(path))).collect();
        out.insert(p.clone(), digest_lines(lines));
    }
    Ok(out)
}

/// A JSON value's digest, its objects' keys sorted, whatever order the map keeps: the job's
/// upstream outputs (`needs`) as a step's key takes them.
pub fn digest_json(v: &serde_json::Value) -> String {
    fn canonical(v: &serde_json::Value, out: &mut String) {
        match v {
            serde_json::Value::Object(m) => {
                let sorted: BTreeMap<&String, &serde_json::Value> = m.iter().collect();
                out.push('{');
                for (i, (k, x)) in sorted.into_iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&serde_json::Value::String(k.clone()).to_string());
                    out.push(':');
                    canonical(x, out);
                }
                out.push('}');
            }
            serde_json::Value::Array(a) => {
                out.push('[');
                for (i, x) in a.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    canonical(x, out);
                }
                out.push(']');
            }
            other => out.push_str(&other.to_string()),
        }
    }
    let mut s = String::new();
    canonical(v, &mut s);
    hex(&Sha256::digest(s.as_bytes()))
}

/// What a step's success leaves for the steps after it (W24-R02's outputs), as the ledger keeps
/// it and a hit replays it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Replay {
    /// Its outputs (`steps.<id>.outputs`).
    pub outputs: BTreeMap<String, String>,
    /// The kiln dialect's typed outputs (`$KILN_OUTPUT_JSON`), each as JSON.
    pub json: BTreeMap<String, String>,
    /// What it set in the job's environment (`$GITHUB_ENV`).
    pub env: BTreeMap<String, String>,
    /// What it put on `PATH` (`$GITHUB_PATH`), in order.
    pub path: Vec<String>,
    /// Its `run.env` lines (`$KILN_RUN_ENV`, W04-R21).
    pub run_env: Vec<(String, String)>,
    /// The files its `memo.outputs` name, as one tree in the content store.
    pub files: Option<Digest>,
}

const OUTPUT: &str = "output.";
const JSON: &str = "json.";
const ENV: &str = "env.";
const PATH: &str = "path.";
const RUN_ENV: &str = "run-env.";
const FILES: &str = "files";

impl Replay {
    /// As the ledger's outputs (`Recorded.outputs`): a flat map of names to values.
    pub fn encode(&self) -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        m.extend(self.outputs.iter().map(|(k, v)| (format!("{OUTPUT}{k}"), v.clone())));
        m.extend(self.json.iter().map(|(k, v)| (format!("{JSON}{k}"), v.clone())));
        m.extend(self.env.iter().map(|(k, v)| (format!("{ENV}{k}"), v.clone())));
        m.extend(self.path.iter().enumerate().map(|(i, p)| (format!("{PATH}{i:06}"), p.clone())));
        m.extend(self.run_env.iter().enumerate().map(|(i, (k, v))| (format!("{RUN_ENV}{i:06}"), format!("{k}={v}"))));
        if let Some(d) = &self.files {
            m.insert(FILES.to_owned(), d.to_string());
        }
        m
    }

    /// Back from the ledger's outputs.
    pub fn decode(m: &BTreeMap<String, String>) -> Result<Replay, String> {
        let mut r = Replay::default();
        for (k, v) in m {
            if let Some(name) = k.strip_prefix(OUTPUT) {
                r.outputs.insert(name.to_owned(), v.clone());
            } else if let Some(name) = k.strip_prefix(JSON) {
                r.json.insert(name.to_owned(), v.clone());
            } else if let Some(name) = k.strip_prefix(ENV) {
                r.env.insert(name.to_owned(), v.clone());
            } else if k.starts_with(PATH) {
                r.path.push(v.clone());
            } else if k.starts_with(RUN_ENV) {
                let (name, value) = v.split_once('=').ok_or_else(|| format!("`{k}` isn't a `run.env` line"))?;
                r.run_env.push((name.to_owned(), value.to_owned()));
            } else if k == FILES {
                r.files = Some(Digest::parse(v).ok_or_else(|| format!("`{v}` isn't a digest"))?);
            }
        }
        Ok(r)
    }

    /// Its digest, as the keys of the steps after it take it (W24-R03): a step that runs again and
    /// produces the same outputs and files leaves them reusable.
    pub fn digest(&self) -> String {
        digest_lines(self.encode().into_iter().flat_map(|(k, v)| [k, v]))
    }
}

/// A content digest, as the Remote Execution API has one: SHA-256 as hex, and the size.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Digest {
    pub hash: String,
    pub size_bytes: u64,
}

impl Digest {
    /// The digest of `bytes`.
    pub fn of(bytes: &[u8]) -> Digest {
        Digest { hash: hex(&Sha256::digest(bytes)), size_bytes: bytes.len() as u64 }
    }

    /// `<hash>/<size>`, as [`Digest`]'s `Display` writes it.
    pub fn parse(s: &str) -> Option<Digest> {
        let (hash, size) = s.split_once('/')?;
        (hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())).then_some(())?;
        Some(Digest { hash: hash.to_owned(), size_bytes: size.parse().ok()? })
    }
}

impl std::fmt::Display for Digest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.hash, self.size_bytes)
    }
}

/// A file in a [`Directory`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileNode {
    pub name: String,
    pub digest: Digest,
    #[serde(default)]
    pub is_executable: bool,
}

/// A directory in a [`Directory`], by its digest.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectoryNode {
    pub name: String,
    pub digest: Digest,
}

/// A symbolic link in a [`Directory`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymlinkNode {
    pub name: String,
    pub target: String,
}

/// A directory, as the Remote Execution API has one: its entries in name order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Directory {
    pub files: Vec<FileNode>,
    pub directories: Vec<DirectoryNode>,
    pub symlinks: Vec<SymlinkNode>,
}

impl Directory {
    /// Its digest: of its JSON, which its entries' order makes canonical.
    pub fn digest(&self) -> Digest {
        Digest::of(&serde_json::to_vec(self).unwrap_or_default())
    }
}

/// A tree of directories, as the Remote Execution API has one: the root, and every directory
/// below it once, by digest.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Tree {
    pub root: Directory,
    pub children: Vec<Directory>,
}

/// One thing a step left, at its path from the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    File { path: String, digest: Digest, executable: bool },
    Dir { path: String },
    Link { path: String, target: String },
}

impl Entry {
    pub fn path(&self) -> &str {
        match self {
            Entry::File { path, .. } | Entry::Dir { path } | Entry::Link { path, .. } => path,
        }
    }
}

/// The most entries a tree holds, and the deepest it goes.
pub const MAX_ENTRIES: usize = 1_000_000;
const MAX_DEPTH: usize = 256;

/// Whether `name` can be one directory entry's: never `.`, `..` or empty, and no separator.
fn name_ok(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0'])
}

#[derive(Default)]
struct Node {
    files: BTreeMap<String, (Digest, bool)>,
    dirs: BTreeMap<String, Node>,
    links: BTreeMap<String, String>,
}

impl Node {
    fn taken(&self, name: &str) -> bool {
        self.files.contains_key(name) || self.dirs.contains_key(name) || self.links.contains_key(name)
    }

    fn seal(self, children: &mut BTreeMap<String, Directory>) -> Directory {
        let mut d = Directory::default();
        for (name, node) in self.dirs {
            let sub = node.seal(children);
            let digest = sub.digest();
            children.insert(digest.hash.clone(), sub);
            d.directories.push(DirectoryNode { name, digest });
        }
        d.files = self.files.into_iter().map(|(name, (digest, is_executable))| FileNode { name, digest, is_executable }).collect();
        d.symlinks = self.links.into_iter().map(|(name, target)| SymlinkNode { name, target }).collect();
        d
    }
}

/// The entries as one tree, rooted at the workspace: directories nested, each one's entries in
/// name order, each named by its digest. A path used twice, or under a file or a link, is refused.
pub fn tree(entries: &[Entry]) -> Result<Tree, String> {
    if entries.len() > MAX_ENTRIES {
        return Err(format!("{} entries, more than the {MAX_ENTRIES} a tree holds", entries.len()));
    }
    let mut root = Node::default();
    for e in entries {
        let parts: Vec<&str> = e.path().split('/').collect();
        if parts.len() > MAX_DEPTH || !parts.iter().all(|p| name_ok(p)) {
            return Err(format!("`{}` isn't a path in the workspace", e.path()));
        }
        let Some((last, dirs)) = parts.split_last() else { continue };
        let mut at = &mut root;
        for d in dirs {
            if at.files.contains_key(*d) || at.links.contains_key(*d) {
                return Err(format!("`{}` is under a file or a link", e.path()));
            }
            at = at.dirs.entry((*d).to_owned()).or_default();
        }
        match e {
            Entry::Dir { .. } if at.dirs.contains_key(*last) => {}
            _ if at.taken(last) => return Err(format!("`{}` is named twice", e.path())),
            Entry::Dir { .. } => {
                at.dirs.insert((*last).to_owned(), Node::default());
            }
            Entry::File { digest, executable, .. } => {
                at.files.insert((*last).to_owned(), (digest.clone(), *executable));
            }
            Entry::Link { target, .. } => {
                at.links.insert((*last).to_owned(), target.clone());
            }
        }
    }
    let mut children = BTreeMap::new();
    let root = root.seal(&mut children);
    Ok(Tree { root, children: children.into_values().collect() })
}

/// A tree's entries, as paths from its root: directories first (each before what's in it), then
/// files, then links, so links are made only once nothing more is written below where they are.
/// A tree that names a directory it doesn't hold, uses a name twice in a directory, or has a name
/// that isn't one ([`name_ok`]: never `..`) is refused, so nothing it names lands outside.
pub fn entries(t: &Tree) -> Result<Vec<Entry>, String> {
    let by: BTreeMap<String, &Directory> = t.children.iter().map(|d| (d.digest().hash, d)).collect();
    let (mut dirs, mut files, mut links) = (Vec::new(), Vec::new(), Vec::new());
    let mut stack: Vec<(String, &Directory, usize)> = vec![(String::new(), &t.root, 0)];
    while let Some((at, d, depth)) = stack.pop() {
        if depth > MAX_DEPTH {
            return Err("the tree is deeper than a workspace's".into());
        }
        let mut names = BTreeSet::new();
        let join = |name: &str| if at.is_empty() { name.to_owned() } else { format!("{at}/{name}") };
        for name in d.files.iter().map(|f| &f.name).chain(d.directories.iter().map(|x| &x.name)).chain(d.symlinks.iter().map(|s| &s.name)) {
            if !name_ok(name) || !names.insert(name.as_str()) {
                return Err(format!("`{}` isn't a name a tree can hold once", join(name)));
            }
        }
        for x in &d.directories {
            let sub = by.get(&x.digest.hash).ok_or_else(|| format!("the tree names a directory it doesn't hold: `{}`", join(&x.name)))?;
            dirs.push(Entry::Dir { path: join(&x.name) });
            stack.push((join(&x.name), sub, depth + 1));
        }
        files.extend(d.files.iter().map(|f| Entry::File { path: join(&f.name), digest: f.digest.clone(), executable: f.is_executable }));
        links.extend(d.symlinks.iter().map(|s| Entry::Link { path: join(&s.name), target: s.target.clone() }));
        if dirs.len() + files.len() + links.len() > MAX_ENTRIES {
            return Err(format!("the tree holds more than {MAX_ENTRIES} entries"));
        }
    }
    for list in [&mut dirs, &mut files, &mut links] {
        list.sort_by(|a, b| a.path().cmp(b.path()));
    }
    Ok(dirs.into_iter().chain(files).chain(links).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wf::ir::Tpl;

    fn s(t: &str) -> YVal {
        YVal::Str(Tpl::new(t, Default::default()))
    }

    fn map(entries: &[(&str, YVal)]) -> YVal {
        YVal::Map(entries.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect())
    }

    #[test]
    fn w24_r07_memo_is_check_or_a_mapping() -> Result<(), String> {
        assert_eq!(Memo::from_yval(&s("check"))?, Some(Memo { max_age: MAX_AGE, ..Memo::default() }));
        assert_eq!(Memo::from_yval(&YVal::Bool(false))?, None);
        let m = Memo::from_yval(&map(&[("inputs", YVal::Seq(vec![s("./src/"), s("Cargo.*")])), ("outputs", s("out\ndist/app")), ("env", YVal::Seq(vec![s("MODE")])), ("max-age", s("2d"))]))?.ok_or("opts in")?;
        assert_eq!(m.inputs, ["src", "Cargo.*"]);
        assert_eq!(m.outputs, ["out", "dist/app"]);
        assert_eq!((m.env.as_slice(), m.max_age, m.hmac), (["MODE".to_owned()].as_slice(), 2 * 86_400, false));
        assert!(Memo::from_yval(&map(&[("secrets", s("hmac"))]))?.is_some_and(|m| m.hmac));
        for bad in [s("always"), YVal::Num(1.0), map(&[("outputs", s("../up"))]), map(&[("outputs", s("/etc"))]), map(&[("outputs", s("out/*"))]), map(&[("inputs", YVal::Num(2.0))]), map(&[("max-age", s("soon"))]), map(&[("secrets", s("plain"))])] {
            assert!(Memo::from_yval(&bad).is_err(), "{bad:?}");
        }
        Ok(())
    }

    fn facts() -> Facts {
        Facts {
            definition: Definition { run: Some("cargo clippy -- -D warnings".into()), shell: Some("bash".into()), ..Definition::default() },
            env: BTreeMap::from([
                ("PATH".to_owned(), "/usr/bin".to_owned()),
                ("MODE".to_owned(), "fast".to_owned()),
                ("RUSTFLAGS".to_owned(), "-Dwarnings".to_owned()),
                ("UNDECLARED".to_owned(), "x".to_owned()),
                ("GITHUB_SHA".to_owned(), "abc".to_owned()),
                ("KILN_JOB".to_owned(), "leg-1".to_owned()),
            ]),
            declared: BTreeSet::from(["MODE".to_owned()]),
            platform: Platform { os: "linux".into(), arch: "x86_64".into(), libc: "gnu".into(), executor: "host".into(), ..Platform::default() },
            inputs: BTreeMap::from([("/".to_owned(), "tree1".to_owned())]),
            upstream: BTreeMap::from([("needs".to_owned(), "n1".to_owned())]),
            trusted: true,
        }
    }

    /// W24-R07: a step's key is a step's unit over the step evaluated, the environment the rules
    /// keep, the platform, its inputs, what came before it and its trust: each changes it, kiln's
    /// per-run values and undeclared variables don't, and a leg's own paths don't either.
    #[test]
    fn w24_r07_a_steps_key() {
        let base = key::weak(&unit(&facts()));
        let u = unit(&facts());
        assert_eq!((u.kind, u.trust.as_str()), (Kind::Step, "trusted"));
        assert_eq!(u.env.keys().map(String::as_str).collect::<Vec<_>>(), ["MODE", "RUSTFLAGS"], "declared and toolchain variables only");
        assert_eq!(base, key::weak(&unit(&facts())), "the same step keys alike");
        assert_ne!(base, key::weak(&Unit { kind: Kind::Leg, ..unit(&facts()) }), "a step never keys as a leg");
        type Edit = (&'static str, fn(&mut Facts));
        let edits: [Edit; 10] = [
            ("its script", |f| f.definition.run = Some("cargo clippy".into())),
            ("an input of its action", |f| {
                f.definition.with.insert("toolchain".into(), "nightly".into());
            }),
            ("its outputs", |f| f.definition.outputs.push("target".into())),
            ("a declared variable", |f| {
                f.env.insert("MODE".into(), "slow".into());
            }),
            ("a toolchain's variable", |f| {
                f.env.insert("RUSTFLAGS".into(), "".into());
            }),
            ("its platform", |f| f.platform.arch = "aarch64".into()),
            ("its image", |f| f.platform.image = Some("sha256:1".into())),
            ("its inputs", |f| {
                f.inputs.insert("/".into(), "tree2".into());
            }),
            ("what came before it", |f| {
                f.upstream.insert("step.0".into(), "d".into());
            }),
            ("its trust", |f| f.trusted = false),
        ];
        for (what, edit) in edits {
            let mut f = facts();
            edit(&mut f);
            assert_ne!(base, key::weak(&unit(&f)), "{what}");
        }
        // kiln's per-run values and what nothing declares stay out...
        let mut f = facts();
        f.env.insert("KILN_JOB".into(), "leg-2".into());
        f.env.insert("GITHUB_RUN_ID".into(), "9".into());
        f.env.insert("UNDECLARED".into(), "y".into());
        f.env.insert("PATH".into(), "/opt/bin:/usr/bin".into());
        assert_eq!(base, key::weak(&unit(&f)));
        // ...unless the step names one, or declares it.
        let mut f = facts();
        f.definition.run = Some("cargo clippy -- -D warnings # $GITHUB_SHA".into());
        let named = key::weak(&unit(&f));
        f.env.insert("GITHUB_SHA".into(), "def".into());
        assert_ne!(named, key::weak(&unit(&f)), "a per-run variable its text names counts");
        let mut f = facts();
        f.declared.insert("UNDECLARED".into());
        assert_ne!(base, key::weak(&unit(&f)));
        // Two legs' workspaces, put as names, key alike.
        let place = |ws: &str| unplaced(&format!("cd {ws}/sub && ls {ws}/_temp/x"), &[(ws.to_owned(), "workspace"), (format!("{ws}/_temp"), "temp")]);
        assert_eq!(place("/w/1a2b/app"), "cd ${workspace}/sub && ls ${temp}/x");
        assert_eq!(place("/w/1a2b/app"), place("/home/r/w/ffee/app"));
    }

    #[test]
    fn w24_r01_inputs_are_the_declared_paths_trees() -> Result<(), String> {
        let listing = "100644 blob a1\tsrc/a.rs\n100644 blob b1\tsrc/x/b.rs\n100644 blob c1\tREADME.md\n100644 blob d1\tsrcs/z\n100644 blob e1\tCargo.lock\n";
        let none = BTreeMap::new();
        let declared = vec!["src".to_owned(), "Cargo.*".to_owned()];
        let base = inputs(&declared, "t1", listing, &none)?;
        assert_eq!(base.keys().map(String::as_str).collect::<Vec<_>>(), ["Cargo.*", "src"]);
        // Another path's change, or another tree with the same paths' objects: the same inputs.
        assert_eq!(base, inputs(&declared, "t2", &listing.replace("c1\tREADME", "c2\tREADME").replace("d1\tsrcs", "d2\tsrcs"), &none)?, "`src` isn't `srcs`");
        // A declared path's object, or what the workspace has there, changed: not.
        let edited = inputs(&declared, "t1", &listing.replace("b1\tsrc/x", "b2\tsrc/x"), &none)?;
        assert_ne!(base.get("src"), edited.get("src"));
        assert_eq!(base.get("Cargo.*"), edited.get("Cargo.*"));
        let dirty = BTreeMap::from([("src/new.rs".to_owned(), "f00d".to_owned())]);
        assert_ne!(base.get("src"), inputs(&declared, "t1", listing, &dirty)?.get("src"));
        assert_eq!(base, inputs(&declared, "t1", listing, &BTreeMap::from([("docs/x".to_owned(), "-".to_owned())]))?);
        // None declared: the whole tree, by its id while nothing differs from it.
        assert_eq!(inputs(&[], "t1", listing, &none)?, BTreeMap::from([("/".to_owned(), "t1".to_owned())]));
        let whole = inputs(&[], "t1", listing, &dirty)?;
        assert_ne!(whole.get("/").map(String::as_str), Some("t1"));
        assert_ne!(whole, inputs(&[], "t1", listing, &BTreeMap::from([("src/new.rs".to_owned(), "beef".to_owned())]))?);
        Ok(())
    }

    #[test]
    fn w24_r03_a_replay_round_trips_and_digests_what_it_left() -> Result<(), String> {
        let r = Replay {
            outputs: BTreeMap::from([("version".to_owned(), "1.2".to_owned()), ("a.b".to_owned(), "dotted".to_owned())]),
            json: BTreeMap::from([("n".to_owned(), "3".to_owned())]),
            env: BTreeMap::from([("MODE".to_owned(), "a=b".to_owned())]),
            path: (0..12).map(|i| format!("/opt/t{i}")).collect(),
            run_env: vec![("K".to_owned(), "v=w".to_owned())],
            files: Some(Digest::of(b"tree")),
        };
        assert_eq!(Replay::decode(&r.encode())?, r, "back as it was, `PATH` in order");
        assert_eq!(r.digest(), Replay::decode(&r.encode())?.digest(), "the same outputs, the same digest (early cutoff)");
        let mut other = r.clone();
        other.files = Some(Digest::of(b"another tree"));
        assert_ne!(r.digest(), other.digest());
        let mut other = r.clone();
        other.outputs.insert("version".into(), "1.3".into());
        assert_ne!(r.digest(), other.digest());
        assert!(Replay::decode(&BTreeMap::from([("files".to_owned(), "nope".to_owned())])).is_err());
        Ok(())
    }

    fn file(path: &str, content: &[u8]) -> Entry {
        Entry::File { path: path.into(), digest: Digest::of(content), executable: path.ends_with(".sh") }
    }

    #[test]
    fn w24_r07_files_are_kept_as_trees() -> Result<(), String> {
        let list = vec![file("out/bin", b"built"), file("out/run.sh", b"#!/bin/sh"), Entry::Dir { path: "out/empty".into() }, file("out/sub/a", b"a"), file("dist/b", b"b"), Entry::Link { path: "out/latest".into(), target: "bin".into() }];
        let t = tree(&list)?;
        assert_eq!(t.root.directories.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["dist", "out"]);
        let mut back = entries(&t)?;
        // Directories first (each before what's in it), then files, then links.
        assert_eq!(back.iter().map(Entry::path).collect::<Vec<_>>(), ["dist", "out", "out/empty", "out/sub", "dist/b", "out/bin", "out/run.sh", "out/sub/a", "out/latest"]);
        back.retain(|e| !matches!(e, Entry::Dir { path } if path == "dist" || path == "out" || path == "out/sub"));
        let mut want = list.clone();
        want.sort_by(|a, b| a.path().cmp(b.path()));
        back.sort_by(|a, b| a.path().cmp(b.path()));
        assert_eq!(back, want);
        // The same files in another order make the same tree; another file's content doesn't.
        let mut shuffled = list.clone();
        shuffled.reverse();
        assert_eq!(tree(&shuffled)?, t);
        let mut changed = list.clone();
        if let Some(e) = changed.first_mut() {
            *e = file("out/bin", b"built again");
        }
        assert_ne!(Digest::of(&serde_json::to_vec(&tree(&changed)?).map_err(|e| e.to_string())?), Digest::of(&serde_json::to_vec(&t).map_err(|e| e.to_string())?));
        // Nothing lands outside, nor under a file or a link.
        for bad in [vec![file("../x", b"")], vec![file("out/./x", b"")], vec![file("out", b""), file("out/x", b"")], vec![Entry::Link { path: "l".into(), target: "/etc".into() }, file("l/passwd", b"")], vec![file("a", b"1"), file("a", b"2")]] {
            assert!(tree(&bad).is_err(), "{bad:?}");
        }
        let mut evil = t.clone();
        if let Some(f) = evil.root.directories.first_mut() {
            f.name = "..".into();
        }
        assert!(entries(&evil).is_err(), "a name that isn't one");
        let mut lost = t.clone();
        lost.children.clear();
        assert!(entries(&lost).is_err(), "a directory the tree doesn't hold");
        assert_eq!(entries(&tree(&[])?)?, Vec::new());
        Ok(())
    }

    /// W24-R07 in the kiln dialect: `memo:` on a step, read as written; a value `memo` can't be and
    /// a key it doesn't have are said where they are; GitHub's dialect has no `memo`.
    #[test]
    fn w24_r07_memo_in_workflows() -> Result<(), String> {
        let wf = |memo: &str| format!("on: push\njobs:\n  build:\n    runs-on: local\n    steps:\n      - run: make\n        memo: {memo}\n");
        let (w, d) = crate::wf::parse_workflow(".kiln/ci.yaml", &wf("{ inputs: [src], outputs: [out] }"), crate::expr::Dialect::Kiln);
        assert!(d.iter().all(|x| !x.is_error()), "{d:?}");
        let memo = w.and_then(|w| w.jobs.into_iter().next()).and_then(|j| j.steps.into_iter().next()).and_then(|s| s.kiln.memo).ok_or("the step's memo")?;
        let m = Memo::from_yval(&memo)?.ok_or("opts in")?;
        assert_eq!((m.inputs.as_slice(), m.outputs.as_slice()), (["src".to_owned()].as_slice(), ["out".to_owned()].as_slice()));
        let codes = |memo: &str, dialect| crate::wf::parse_workflow(".kiln/ci.yaml", &wf(memo), dialect).1.into_iter().filter(|x| x.is_error()).map(|x| (x.code, x.path)).collect::<Vec<_>>();
        assert_eq!(codes("check", crate::expr::Dialect::Kiln), []);
        assert_eq!(codes("sometimes", crate::expr::Dialect::Kiln), [("KCI1015", "jobs.build.steps[0].memo".to_owned())]);
        assert_eq!(codes("{ outputs: [out], keep: 1 }", crate::expr::Dialect::Kiln), [("KCI1010", "jobs.build.steps[0].memo.keep".to_owned())]);
        assert!(!codes("check", crate::expr::Dialect::Gha).is_empty(), "a kiln key");
        // A step without `memo` serializes as it did, so workflows' hashes don't move.
        let (w, _) = crate::wf::parse_workflow(".kiln/ci.yaml", "on: push\njobs:\n  b:\n    runs-on: local\n    steps:\n      - run: make\n", crate::expr::Dialect::Kiln);
        let json = serde_json::to_string(&w.ok_or("a workflow")?).map_err(|e| e.to_string())?;
        assert!(!json.contains("memo"), "{json}");
        Ok(())
    }

    #[test]
    fn w24_r07_digests_read_back() {
        let d = Digest::of(b"x");
        assert_eq!(Digest::parse(&d.to_string()), Some(d));
        assert_eq!(Digest::parse("abc/1"), None);
        assert_eq!(Digest::parse(&format!("{}/x", "0".repeat(64))), None);
        assert_eq!(digest_json(&serde_json::json!({"b": 1, "a": [1, {"d": 2, "c": 3}]})), digest_json(&serde_json::json!({"a": [1, {"c": 3, "d": 2}], "b": 1})));
        assert_ne!(digest_json(&serde_json::json!({"a": 1})), digest_json(&serde_json::json!({"a": "1"})));
    }
}
