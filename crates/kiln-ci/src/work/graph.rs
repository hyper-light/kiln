//! Package graphs (docs/ci.md W24-R16) and [`affected`] (R17): the packages a repository's
//! manifests declare, which of them depend on which, and which of them a change can affect. Pure:
//! the caller passes the tree's files ([`read`] takes a way to read one and the list of paths).
//!
//! One workspace per ecosystem is read, from the repository's root:
//!
//! - **Cargo** (`cargo`): `Cargo.toml`'s `[workspace]` `members` (globs) less its `exclude`, and the
//!   root itself when it has a `[package]`. A package depends on its path dependencies
//!   (`[dependencies]`, `[dev-dependencies]`, `[build-dependencies]`, and each `[target.*]`'s), on
//!   `workspace = true` ones through its workspace's `[workspace.dependencies]`, and on registry
//!   ones the root's `[patch]` points at a folder. A member's path dependency that `exclude`
//!   doesn't keep out is a member too, as Cargo has it.
//! - **npm, pnpm and yarn** (`npm`): `package.json`'s `workspaces` (a list, or yarn's `packages`)
//!   and `pnpm-workspace.yaml`'s `packages`, less what their `!` patterns name. A package depends on
//!   the others its `dependencies`, `devDependencies`, `peerDependencies` and
//!   `optionalDependencies` name. The root is the workspace's, not one of its packages, unless it
//!   declares no workspace.
//! - **uv** (`uv`): `pyproject.toml`'s `[tool.uv.workspace]` `members` less its `exclude`, and the
//!   root itself when it has a `[project]`. A package depends on the requirements (`[project]`'s
//!   `dependencies` and `optional-dependencies`, `[dependency-groups]`, uv's `dev-dependencies`)
//!   that its `[tool.uv.sources]`, or the root's, declares `{ workspace = true }` or gives a `path`.
//! - **Go** (`go`): `go.work`'s `use`, else the root's `go.mod`. A module depends on the others it
//!   `require`s, and on the folders its local `replace`s (`=> ./dir`), and `go.work`'s, point at.
//!
//! Local code a member depends on from outside the workspace (a path dependency `exclude` keeps
//! out, a `[patch]`'s folder, a local `replace`, a uv `path` source) is in the graph too, but not as
//! a member: it owns its files and passes their changes on, and [`affected`] lists members only.
//! Finer graphs (`cargo metadata`, `go list`, `bazel query`) are R16's cached actions on runners,
//! not this module's.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::sbom::pep503;

/// A family of packages and the manifests that declare them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Ecosystem {
    Cargo,
    /// npm's, pnpm's and yarn's: their workspaces are `package.json`'s.
    Npm,
    Uv,
    Go,
}

impl Ecosystem {
    pub fn name(self) -> &'static str {
        match self {
            Ecosystem::Cargo => "cargo",
            Ecosystem::Npm => "npm",
            Ecosystem::Uv => "uv",
            Ecosystem::Go => "go",
        }
    }

    /// The ecosystem `affected()` names, in any case: `pnpm` and `yarn` are `npm`.
    pub fn parse(name: &str) -> Option<Ecosystem> {
        Some(match name.trim().to_ascii_lowercase().as_str() {
            "cargo" => Ecosystem::Cargo,
            "npm" | "pnpm" | "yarn" => Ecosystem::Npm,
            "uv" => Ecosystem::Uv,
            "go" => Ecosystem::Go,
            _ => return None,
        })
    }

    /// Its packages' manifest.
    fn manifest(self) -> &'static str {
        match self {
            Ecosystem::Cargo => "Cargo.toml",
            Ecosystem::Npm => "package.json",
            Ecosystem::Uv => "pyproject.toml",
            Ecosystem::Go => "go.mod",
        }
    }

    /// The files at the root that every package of the root's workspace depends on: the
    /// workspace's manifests and lockfiles.
    fn root_files(self) -> &'static [&'static str] {
        match self {
            Ecosystem::Cargo => &["Cargo.toml", "Cargo.lock"],
            Ecosystem::Npm => &["package.json", "pnpm-workspace.yaml", "package-lock.json", "npm-shrinkwrap.json", "pnpm-lock.yaml", "yarn.lock"],
            Ecosystem::Uv => &["pyproject.toml", "uv.lock"],
            Ecosystem::Go => &["go.work", "go.work.sum"],
        }
    }
}

impl std::fmt::Display for Ecosystem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// One package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Package {
    /// As its manifest names it: `kiln-ci`, `@scope/ui`, `example.com/mod`. An npm package
    /// without a `name` goes by its folder.
    pub name: String,
    pub ecosystem: Ecosystem,
    /// Its folder from the repository's root (`crates/kiln-ci`); empty for the root.
    pub dir: String,
    /// One of the root's workspace's packages (what `cargo metadata --no-deps` lists, say); else
    /// local code a member depends on from outside the workspace.
    pub member: bool,
    /// The folder of the workspace whose root manifest it inherits from: `Some("")` for the
    /// root's, a deeper one for a Cargo package of a workspace of its own (a vendored one), `None`
    /// for one in no workspace.
    pub workspace: Option<String>,
    /// The packages of its ecosystem it depends on, by name.
    pub deps: BTreeSet<String>,
}

/// A manifest that couldn't be read, or that names what isn't there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Problem {
    pub ecosystem: Ecosystem,
    /// The manifest.
    pub path: String,
    pub message: String,
}

impl Problem {
    fn new(ecosystem: Ecosystem, path: &str, message: impl Into<String>) -> Problem {
        Problem { ecosystem, path: path.to_owned(), message: message.into() }
    }
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

/// A repository's packages, every ecosystem's, and what couldn't be read. An ecosystem with
/// problems may be missing packages or dependencies, so what [`affected`] says of it may be short.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Graph {
    /// By ecosystem, then folder.
    pub packages: Vec<Package>,
    pub problems: Vec<Problem>,
}

impl Graph {
    /// The packages of `ecosystem`.
    pub fn of(&self, ecosystem: Ecosystem) -> impl Iterator<Item = &Package> {
        self.packages.iter().filter(move |p| p.ecosystem == ecosystem)
    }

    /// The problems reading `ecosystem`'s manifests.
    pub fn problems_of(&self, ecosystem: Ecosystem) -> impl Iterator<Item = &Problem> {
        self.problems.iter().filter(move |p| p.ecosystem == ecosystem)
    }
}

/// The manifests [`read`] reads, by file name: a caller that fetches files ahead fetches these.
pub const MANIFESTS: [&str; 6] = ["Cargo.toml", "package.json", "pnpm-workspace.yaml", "pyproject.toml", "go.mod", "go.work"];

/// Whether `path` is a manifest [`read`] may read.
pub fn is_manifest(path: &str) -> bool {
    MANIFESTS.contains(&path.rsplit('/').next().unwrap_or(path))
}

/// The package graph of a tree (W24-R16): `files` gives a file's text by its path from the root;
/// `paths` lists the tree's files (its manifests at least, [`is_manifest`]). Members' globs expand
/// over `paths`, and a manifest it doesn't list isn't there. Manifests under `node_modules` aren't
/// read. What can't be read is a [`Problem`], never a panic.
pub fn read(files: &dyn Fn(&str) -> Option<String>, paths: &[String]) -> Graph {
    let manifests = paths.iter().map(|p| p.trim_start_matches("./")).filter(|p| is_manifest(p) && !p.split('/').any(|s| s == "node_modules")).map(str::to_owned).collect();
    let tree = Tree { files, manifests };
    let mut problems = Vec::new();
    let mut packages = cargo(&tree, &mut problems);
    packages.extend(npm(&tree, &mut problems));
    packages.extend(uv(&tree, &mut problems));
    packages.extend(go(&tree, &mut problems));
    packages.sort_by(|a, b| (a.ecosystem, &a.dir).cmp(&(b.ecosystem, &b.dir)));
    Graph { packages, problems }
}

/// The packages of `ecosystem` that a change to `changed` (files, by their paths from the root)
/// can affect (W24-R17), sorted: each file's package (the one whose folder holds it most closely)
/// and every package that depends on one of those, transitively. Members only: local code outside
/// the workspace passes its changes on to the members that depend on it, but isn't listed.
///
/// A file in no package (docs, CI files) affects none, except a workspace's manifest or lockfile:
/// as no manifest says what of those a package reads, a change to one affects every package of the
/// workspace. That is conservative: a version bump in `Cargo.lock` affects every crate, though only
/// those that depend on the bumped crate could change. Those files are the root's `Cargo.toml` and
/// `Cargo.lock`; `package.json`, `pnpm-workspace.yaml`, `package-lock.json`,
/// `npm-shrinkwrap.json`, `pnpm-lock.yaml` and `yarn.lock`; `pyproject.toml` and `uv.lock`;
/// `go.work` and `go.work.sum`; and the root manifest of a deeper Cargo workspace, for its packages.
///
/// A graph with problems in `ecosystem` may be missing some of its packages or dependencies: check
/// [`Graph::problems_of`] first, as `affected()` in expressions does.
pub fn affected(graph: &Graph, ecosystem: Ecosystem, changed: &[String]) -> BTreeSet<String> {
    let packages: Vec<&Package> = graph.of(ecosystem).collect();
    let by_dir: BTreeMap<&str, &Package> = packages.iter().map(|p| (p.dir.as_str(), *p)).collect();
    // A workspace's manifests and lockfiles, each with the workspace's folder.
    let mut shared: BTreeMap<String, &str> = BTreeMap::new();
    for w in packages.iter().filter_map(|p| p.workspace.as_deref()) {
        if w.is_empty() {
            shared.extend(ecosystem.root_files().iter().map(|f| ((*f).to_owned(), w)));
        } else {
            shared.insert(file(w, ecosystem.manifest()), w);
        }
    }
    let mut hit: BTreeSet<&str> = BTreeSet::new();
    for f in changed {
        let f = f.trim_start_matches("./");
        if let Some(&w) = shared.get(f) {
            hit.extend(packages.iter().filter(|p| p.workspace.as_deref() == Some(w)).map(|p| p.name.as_str()));
        }
        if let Some(p) = owner(&by_dir, f) {
            hit.insert(&p.name);
        }
    }
    let mut dependents: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for p in &packages {
        for d in &p.deps {
            dependents.entry(d.as_str()).or_default().push(&p.name);
        }
    }
    let mut todo: Vec<&str> = hit.iter().copied().collect();
    while let Some(n) = todo.pop() {
        for &d in dependents.get(n).into_iter().flatten() {
            if hit.insert(d) {
                todo.push(d);
            }
        }
    }
    packages.iter().filter(|p| p.member && hit.contains(p.name.as_str())).map(|p| p.name.clone()).collect()
}

/// The package whose folder holds file `path` most closely.
fn owner<'g>(by_dir: &BTreeMap<&str, &'g Package>, path: &str) -> Option<&'g Package> {
    let mut at = path;
    while let Some((up, _)) = at.rsplit_once('/') {
        if let Some(&p) = by_dir.get(up) {
            return Some(p);
        }
        at = up;
    }
    by_dir.get("").copied()
}

// ------------------------------------------------------------------ reading

/// The tree [`read`] reads.
struct Tree<'a> {
    files: &'a dyn Fn(&str) -> Option<String>,
    /// The manifests among its paths.
    manifests: BTreeSet<String>,
}

impl Tree<'_> {
    /// A manifest's text: `Ok(None)` when the tree has none at `path`.
    fn text(&self, path: &str) -> Result<Option<String>, String> {
        if !self.manifests.contains(path) {
            return Ok(None);
        }
        (self.files)(path).map(Some).ok_or_else(|| "is in the tree but couldn't be read".to_owned())
    }

    fn toml(&self, path: &str) -> Result<Option<toml::Table>, String> {
        let Some(text) = self.text(path)? else { return Ok(None) };
        text.parse::<toml::Table>().map(Some).map_err(|e| {
            let what = e.message().trim().replace('\n', "; ");
            match e.span().and_then(|s| text.as_bytes().get(..s.start)) {
                Some(before) => format!("isn't valid TOML (line {}): {what}", before.iter().filter(|b| **b == b'\n').count() + 1),
                None => format!("isn't valid TOML: {what}"),
            }
        })
    }

    fn json(&self, path: &str) -> Result<Option<Json>, String> {
        let Some(text) = self.text(path)? else { return Ok(None) };
        serde_json::from_str(&text).map(Some).map_err(|e| format!("isn't valid JSON: {e}"))
    }

    fn yaml(&self, path: &str) -> Result<Option<Json>, String> {
        let Some(text) = self.text(path)? else { return Ok(None) };
        if text.trim().is_empty() {
            return Ok(Some(Json::Null));
        }
        serde_yaml::from_str(&text).map(Some).map_err(|e| format!("isn't valid YAML: {e}"))
    }

    /// The folders holding a manifest called `name`.
    fn dirs<'s>(&'s self, name: &'s str) -> impl Iterator<Item = &'s str> + 's {
        self.manifests.iter().filter_map(move |p| match p.rsplit_once('/') {
            Some((dir, base)) => (base == name).then_some(dir),
            None => (p == name).then_some(""),
        })
    }
}

/// A package as its manifest reads, before its dependencies are linked to packages.
struct Raw {
    package: Package,
    /// The folders it depends on.
    dirs: Vec<String>,
    /// The names it depends on, which count when a package of its ecosystem has one.
    names: Vec<String>,
}

/// Each package's dependencies, as the names of the packages they are (never its own).
fn link(ecosystem: Ecosystem, raws: Vec<Raw>) -> Vec<Package> {
    let by_dir: BTreeMap<String, String> = raws.iter().map(|r| (r.package.dir.clone(), r.package.name.clone())).collect();
    let by_name: BTreeMap<String, String> = raws.iter().map(|r| (key(ecosystem, &r.package.name), r.package.name.clone())).collect();
    raws.into_iter()
        .map(|r| {
            let mut p = r.package;
            let deps: BTreeSet<String> = r.dirs.iter().filter_map(|d| by_dir.get(d)).chain(r.names.iter().filter_map(|n| by_name.get(&key(ecosystem, n)))).filter(|n| **n != p.name).cloned().collect();
            p.deps = deps;
            p
        })
        .collect()
}

/// A name as its ecosystem compares names: Python's normalized (PEP 503), the others' as written.
fn key(ecosystem: Ecosystem, name: &str) -> String {
    if ecosystem == Ecosystem::Uv { pep503(name) } else { name.to_owned() }
}

/// The folders still to read: members first, so a folder that is one is read as one.
#[derive(Default)]
struct Queue {
    /// (folder, the manifest that names it).
    members: VecDeque<(String, String)>,
    others: VecDeque<(String, String)>,
    seen: BTreeSet<String>,
}

impl Queue {
    fn add(&mut self, member: bool, dir: String, from: &str) {
        let q = if member { &mut self.members } else { &mut self.others };
        q.push_back((dir, from.to_owned()));
    }

    /// The next folder not yet read, and whether it's a member's.
    fn next(&mut self) -> Option<(String, String, bool)> {
        loop {
            let (dir, from, member) = match self.members.pop_front() {
                Some((d, f)) => (d, f, true),
                None => {
                    let (d, f) = self.others.pop_front()?;
                    (d, f, false)
                }
            };
            if self.seen.insert(dir.clone()) {
                return Some((dir, from, member));
            }
        }
    }
}

/// The manifest at `path`, or why there is none: a problem for a folder `from` named.
fn manifest<T>(ecosystem: Ecosystem, got: Result<Option<T>, String>, path: &str, dir: &str, from: &str, problems: &mut Vec<Problem>) -> Option<T> {
    match got {
        Ok(Some(doc)) => Some(doc),
        Ok(None) => {
            problems.push(Problem::new(ecosystem, from, format!("names `{}`, which has no {}", shown(dir), ecosystem.manifest())));
            None
        }
        Err(e) => {
            problems.push(Problem::new(ecosystem, path, e));
            None
        }
    }
}

/// The root's manifest of an ecosystem, when it has one.
fn root_manifest<T>(ecosystem: Ecosystem, path: &str, got: Result<Option<T>, String>, problems: &mut Vec<Problem>) -> Option<T> {
    got.unwrap_or_else(|e| {
        problems.push(Problem::new(ecosystem, path, e));
        None
    })
}

// ------------------------------------------------------------------ Cargo

/// A Cargo workspace's root: what its packages inherit, and which folders it keeps out.
struct CargoRoot {
    /// `[workspace.dependencies]`.
    deps: toml::Table,
    members: Vec<String>,
    exclude: Vec<String>,
}

impl CargoRoot {
    fn of(doc: &toml::Table) -> Option<CargoRoot> {
        let ws = doc.get("workspace")?.as_table()?;
        Some(CargoRoot { deps: ws.get("dependencies").and_then(toml::Value::as_table).cloned().unwrap_or_default(), members: strings(ws.get("members")), exclude: strings(ws.get("exclude")) })
    }

    /// Whether it keeps `dir` (from its own folder) out, as Cargo's `exclude` does: a member
    /// listed by its path (not a glob) stays in.
    fn excludes(&self, dir: &str) -> bool {
        let named = |list: &[String]| list.iter().any(|e| join("", e).is_some_and(|e| under(&e, dir)));
        named(&self.exclude) && !named(&self.members)
    }
}

/// Where a Cargo dependency's code is.
enum CargoDep<'a> {
    /// In a folder (`path`), from the manifest's.
    Path(&'a str),
    /// Its workspace's entry of the same name (`workspace = true`).
    Workspace,
    /// A registry's or git's, by its package's name (`package`, else its own).
    Named(&'a str),
}

fn cargo_dep<'a>(name: &'a str, spec: &'a toml::Value) -> CargoDep<'a> {
    if let Some(p) = spec.get("path").and_then(toml::Value::as_str) {
        return CargoDep::Path(p);
    }
    if spec.get("workspace").and_then(toml::Value::as_bool) == Some(true) {
        return CargoDep::Workspace;
    }
    CargoDep::Named(spec.get("package").and_then(toml::Value::as_str).unwrap_or(name))
}

/// Every dependency a Cargo manifest declares, and whether it's a dev-dependency:
/// `[dependencies]`, `[build-dependencies]` and `[dev-dependencies]` (with their old `_`
/// spellings), and each `[target.*]`'s.
fn cargo_deps(doc: &toml::Table) -> Vec<(&str, &toml::Value, bool)> {
    const KINDS: [(&str, bool); 5] = [("dependencies", false), ("build-dependencies", false), ("build_dependencies", false), ("dev-dependencies", true), ("dev_dependencies", true)];
    let targets = doc.get("target").and_then(toml::Value::as_table).into_iter().flat_map(|t| t.values()).filter_map(toml::Value::as_table);
    let mut out = Vec::new();
    for t in std::iter::once(doc).chain(targets) {
        for (kind, dev) in KINDS {
            for (name, spec) in t.get(kind).and_then(toml::Value::as_table).into_iter().flatten() {
                out.push((name.as_str(), spec, dev));
            }
        }
    }
    out
}

/// The root's Cargo workspace.
fn cargo(tree: &Tree<'_>, problems: &mut Vec<Problem>) -> Vec<Package> {
    let eco = Ecosystem::Cargo;
    let Some(root) = root_manifest(eco, "Cargo.toml", tree.toml("Cargo.toml"), problems) else { return Vec::new() };
    // `[patch.<source>]`: a package, by its name, read from a folder instead.
    let mut patches: BTreeMap<String, String> = BTreeMap::new();
    for (name, spec) in root.get("patch").and_then(toml::Value::as_table).into_iter().flat_map(|t| t.values()).filter_map(toml::Value::as_table).flatten() {
        if let Some(dir) = spec.get("path").and_then(toml::Value::as_str).and_then(|p| join("", p)) {
            patches.insert(spec.get("package").and_then(toml::Value::as_str).unwrap_or(name).to_owned(), dir);
        }
    }
    let top = CargoRoot::of(&root);
    let mut queue = Queue::default();
    if root.contains_key("package") {
        queue.add(true, String::new(), "Cargo.toml");
    }
    if let Some(t) = &top {
        for m in &t.members {
            if is_glob(m) {
                for d in tree.dirs("Cargo.toml").filter(|d| glob(m, d) && !t.excludes(d)) {
                    queue.add(true, d.to_owned(), "Cargo.toml");
                }
            } else if let Some(d) = join("", m) {
                queue.add(true, d, "Cargo.toml");
            }
        }
    }
    let mut roots: BTreeMap<String, Option<CargoRoot>> = BTreeMap::from([(String::new(), top)]);
    let mut raws = Vec::new();
    while let Some((dir, from, member)) = queue.next() {
        let path = file(&dir, "Cargo.toml");
        let got = if dir.is_empty() { Ok(Some(root.clone())) } else { tree.toml(&path) };
        let Some(doc) = manifest(eco, got, &path, &dir, &from, problems) else { continue };
        let Some(name) = doc.get("package").and_then(|p| p.get("name")).and_then(toml::Value::as_str).map(str::to_owned) else {
            problems.push(Problem::new(eco, &path, "has no `[package]` `name`"));
            continue;
        };
        let workspace = if member { Some(String::new()) } else { cargo_root(tree, &dir, &doc, &mut roots, problems) };
        let mut dirs = Vec::new();
        for (key, spec, dev) in cargo_deps(&doc) {
            // Cargo builds a dev-dependency only for a member.
            if dev && !member {
                continue;
            }
            let (target, by_path) = match cargo_dep(key, spec) {
                CargoDep::Path(p) => (join(&dir, p), true),
                CargoDep::Named(n) => (patches.get(n).cloned(), false),
                CargoDep::Workspace => match workspace.as_deref().and_then(|w| Some((w, roots.get(w)?.as_ref()?))) {
                    None => {
                        problems.push(Problem::new(eco, &path, format!("`{key}.workspace = true`, but it's in no workspace")));
                        continue;
                    }
                    Some((w, r)) => match r.deps.get(key).map(|e| cargo_dep(key, e)) {
                        Some(CargoDep::Path(p)) => (join(w, p), true),
                        Some(CargoDep::Named(n)) => (patches.get(n).cloned(), false),
                        Some(CargoDep::Workspace) => (None, false),
                        None => {
                            problems.push(Problem::new(eco, &path, format!("`{key}.workspace = true`, but {} has no `[workspace.dependencies]` `{key}`", file(w, "Cargo.toml"))));
                            continue;
                        }
                    },
                },
            };
            // Outside the repository, or itself.
            let Some(target) = target.filter(|t| *t != dir) else { continue };
            // A member's path dependency is a member too, unless `exclude` keeps it out.
            let joins = member && by_path && roots.get("").and_then(Option::as_ref).is_some_and(|top| !top.excludes(&target));
            queue.add(joins, target.clone(), &path);
            dirs.push(target);
        }
        raws.push(Raw { package: Package { name, ecosystem: eco, dir, member, workspace, deps: BTreeSet::new() }, dirs, names: Vec::new() });
    }
    link(eco, raws)
}

/// The folder of the Cargo workspace a package that isn't the root's member is in: its own, its
/// `package.workspace`'s, or the nearest one above it that doesn't exclude it; read into `roots`.
fn cargo_root(tree: &Tree<'_>, dir: &str, doc: &toml::Table, roots: &mut BTreeMap<String, Option<CargoRoot>>, problems: &mut Vec<Problem>) -> Option<String> {
    if let Some(own) = CargoRoot::of(doc) {
        roots.insert(dir.to_owned(), Some(own));
        return Some(dir.to_owned());
    }
    if let Some(at) = doc.get("package").and_then(|p| p.get("workspace")).and_then(toml::Value::as_str) {
        let at = join(dir, at)?;
        return cargo_load(tree, &at, roots, problems).then_some(at);
    }
    let mut at = dir;
    while let Some(up) = parent(at) {
        if cargo_load(tree, up, roots, problems) && roots.get(up).and_then(Option::as_ref).is_some_and(|r| !r.excludes(relative(up, dir))) {
            return Some(up.to_owned());
        }
        at = up;
    }
    None
}

/// Reads the manifest in `dir` into `roots`, once: whether it's a workspace's root.
fn cargo_load(tree: &Tree<'_>, dir: &str, roots: &mut BTreeMap<String, Option<CargoRoot>>, problems: &mut Vec<Problem>) -> bool {
    if !roots.contains_key(dir) {
        let path = file(dir, "Cargo.toml");
        let root = root_manifest(Ecosystem::Cargo, &path, tree.toml(&path), problems).as_ref().and_then(CargoRoot::of);
        roots.insert(dir.to_owned(), root);
    }
    roots.get(dir).is_some_and(Option::is_some)
}

// ------------------------------------------------------------------ npm, pnpm and yarn

/// The root's npm, pnpm or yarn workspace.
fn npm(tree: &Tree<'_>, problems: &mut Vec<Problem>) -> Vec<Package> {
    let eco = Ecosystem::Npm;
    let root = root_manifest(eco, "package.json", tree.json("package.json"), problems);
    let pnpm = root_manifest(eco, "pnpm-workspace.yaml", tree.yaml("pnpm-workspace.yaml"), problems);
    let mut patterns: Option<Vec<String>> = None;
    if let Some(w) = root.as_ref().and_then(|r| r.get("workspaces")) {
        // A list, or yarn's `{ packages: [...] }`.
        patterns.get_or_insert_default().extend(json_strings(w.get("packages").or(Some(w))));
    }
    // Without `packages`, it only holds settings (pnpm reads them there too).
    if let Some(p) = pnpm.as_ref().and_then(|p| p.get("packages")) {
        patterns.get_or_insert_default().extend(json_strings(Some(p)));
    }
    let members: Vec<String> = match &patterns {
        Some(patterns) => {
            let (exclude, include): (Vec<&String>, Vec<&String>) = patterns.iter().partition(|p| p.starts_with('!'));
            let kept = |d: &str| !d.is_empty() && include.iter().any(|p| glob(p, d)) && !exclude.iter().any(|p| glob(p.trim_start_matches('!'), d));
            tree.dirs("package.json").filter(|d| kept(d)).map(str::to_owned).collect()
        }
        None if root.is_some() => vec![String::new()],
        None => Vec::new(),
    };
    let mut raws = Vec::new();
    for dir in members {
        let path = file(&dir, "package.json");
        let got = if dir.is_empty() { Ok(root.clone()) } else { tree.json(&path) };
        let Some(doc) = manifest(eco, got, &path, &dir, "package.json", problems) else { continue };
        let name = doc.get("name").and_then(Json::as_str).filter(|n| !n.is_empty()).unwrap_or(shown(&dir)).to_owned();
        let names = ["dependencies", "devDependencies", "peerDependencies", "optionalDependencies"].iter().filter_map(|k| doc.get(*k).and_then(Json::as_object)).flat_map(|deps| deps.keys().cloned()).collect();
        raws.push(Raw { package: Package { name, ecosystem: eco, dir, member: true, workspace: Some(String::new()), deps: BTreeSet::new() }, dirs: Vec::new(), names });
    }
    link(eco, raws)
}

// ------------------------------------------------------------------ uv

/// The root's uv workspace.
fn uv(tree: &Tree<'_>, problems: &mut Vec<Problem>) -> Vec<Package> {
    let eco = Ecosystem::Uv;
    let Some(root) = root_manifest(eco, "pyproject.toml", tree.toml("pyproject.toml"), problems) else { return Vec::new() };
    let mut queue = Queue::default();
    if root.contains_key("project") {
        queue.add(true, String::new(), "pyproject.toml");
    }
    if let Some(ws) = root.get("tool").and_then(|t| t.get("uv")).and_then(|u| u.get("workspace")) {
        let exclude = strings(ws.get("exclude"));
        let kept = |d: &str| !exclude.iter().any(|e| glob(e, d));
        for m in strings(ws.get("members")) {
            if is_glob(&m) {
                for d in tree.dirs("pyproject.toml").filter(|d| glob(&m, d) && kept(d)) {
                    queue.add(true, d.to_owned(), "pyproject.toml");
                }
            } else if let Some(d) = join("", &m).filter(|d| kept(d)) {
                queue.add(true, d, "pyproject.toml");
            }
        }
    }
    let root_sources = uv_sources(&root);
    let mut raws = Vec::new();
    while let Some((dir, from, member)) = queue.next() {
        let path = file(&dir, "pyproject.toml");
        let got = if dir.is_empty() { Ok(Some(root.clone())) } else { tree.toml(&path) };
        let Some(doc) = manifest(eco, got, &path, &dir, &from, problems) else { continue };
        let Some(name) = doc.get("project").and_then(|p| p.get("name")).and_then(toml::Value::as_str).map(str::to_owned) else {
            problems.push(Problem::new(eco, &path, "has no `[project]` `name`"));
            continue;
        };
        let own = uv_sources(&doc);
        let (mut dirs, mut names) = (Vec::new(), Vec::new());
        for req in uv_requirements(&doc) {
            // Its own source, else (for a member) the root's, from the folder that declares it.
            let source = match own.get(&req) {
                Some(s) => Some((*s, dir.as_str())),
                None if member => root_sources.get(&req).map(|s| (*s, "")),
                None => None,
            };
            let Some((spec, at)) = source else { continue };
            if uv_source(spec, "workspace").and_then(toml::Value::as_bool) == Some(true) {
                names.push(req.clone());
            }
            if let Some(d) = uv_source(spec, "path").and_then(toml::Value::as_str).and_then(|p| join(at, p)).filter(|d| *d != dir) {
                queue.add(false, d.clone(), &path);
                dirs.push(d);
            }
        }
        raws.push(Raw { package: Package { name, ecosystem: eco, dir, member, workspace: member.then(String::new), deps: BTreeSet::new() }, dirs, names });
    }
    link(eco, raws)
}

/// `[tool.uv.sources]`, by each project's normalized name.
fn uv_sources(doc: &toml::Table) -> BTreeMap<String, &toml::Value> {
    doc.get("tool").and_then(|t| t.get("uv")).and_then(|u| u.get("sources")).and_then(toml::Value::as_table).into_iter().flatten().map(|(k, v)| (pep503(k), v)).collect()
}

/// A source's `key`: in its table, or the first of its tables (a list, by marker) that has it.
fn uv_source<'a>(spec: &'a toml::Value, key: &str) -> Option<&'a toml::Value> {
    match spec {
        toml::Value::Array(alternatives) => alternatives.iter().find_map(|a| a.get(key)),
        other => other.get(key),
    }
}

/// Every requirement a pyproject.toml lists, by its project's normalized name: `[project]`'s
/// `dependencies` and `optional-dependencies`, `[dependency-groups]` and uv's `dev-dependencies`.
fn uv_requirements(doc: &toml::Table) -> BTreeSet<String> {
    let project = doc.get("project");
    let optional = project.and_then(|p| p.get("optional-dependencies")).and_then(toml::Value::as_table).into_iter().flat_map(|t| t.values());
    let groups = doc.get("dependency-groups").and_then(toml::Value::as_table).into_iter().flat_map(|t| t.values());
    let dev = doc.get("tool").and_then(|t| t.get("uv")).and_then(|u| u.get("dev-dependencies"));
    project.and_then(|p| p.get("dependencies")).into_iter().chain(optional).chain(groups).chain(dev).filter_map(toml::Value::as_array).flatten().filter_map(toml::Value::as_str).filter_map(requirement).collect()
}

/// A requirement's project, normalized (PEP 508, PEP 503): `Foo_Bar[x]>=1; python_version < '4'`
/// is `foo-bar`.
fn requirement(req: &str) -> Option<String> {
    let name: String = req.trim_start().chars().take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')).collect();
    (!name.is_empty()).then(|| pep503(&name))
}

// ------------------------------------------------------------------ Go

/// The root's Go workspace: `go.work`'s modules, else the root's module.
fn go(tree: &Tree<'_>, problems: &mut Vec<Problem>) -> Vec<Package> {
    let eco = Ecosystem::Go;
    let mut queue = Queue::default();
    // `go.work`'s local `replace`s, which every module of the workspace builds with.
    let mut shared = Vec::new();
    match root_manifest(eco, "go.work", tree.text("go.work"), problems) {
        Some(work) => {
            for (verb, args) in directives(&work) {
                match verb.as_str() {
                    "use" => {
                        if let Some(d) = args.first().and_then(|p| join("", p)) {
                            queue.add(true, d, "go.work");
                        }
                    }
                    "replace" => shared.extend(local_replacement(&args).and_then(|p| join("", p))),
                    _ => {}
                }
            }
        }
        None => {
            if root_manifest(eco, "go.mod", tree.text("go.mod"), problems).is_none() {
                return Vec::new();
            }
            queue.add(true, String::new(), "go.mod");
        }
    }
    let mut raws = Vec::new();
    while let Some((dir, from, member)) = queue.next() {
        let path = file(&dir, "go.mod");
        let Some(text) = manifest(eco, tree.text(&path), &path, &dir, &from, problems) else { continue };
        let (mut name, mut names, mut dirs) = (None, Vec::new(), Vec::new());
        for (verb, args) in directives(&text) {
            match verb.as_str() {
                "module" => name = args.into_iter().next(),
                "require" => names.extend(args.into_iter().next()),
                "replace" => dirs.extend(local_replacement(&args).and_then(|p| join(&dir, p))),
                _ => {}
            }
        }
        let Some(name) = name else {
            problems.push(Problem::new(eco, &path, "has no `module` directive"));
            continue;
        };
        if member {
            dirs.extend(shared.iter().cloned());
        }
        dirs.retain(|d| *d != dir);
        for d in &dirs {
            queue.add(false, d.clone(), &path);
        }
        raws.push(Raw { package: Package { name, ecosystem: eco, dir, member, workspace: member.then(String::new), deps: BTreeSet::new() }, dirs, names });
    }
    link(eco, raws)
}

/// A go.mod's or go.work's directives, in order: each verb with its arguments, and a block's
/// lines (`require ( … )`) each on its own.
fn directives(text: &str) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    let mut block: Option<String> = None;
    for line in text.lines() {
        let words = tokens(line);
        if words.is_empty() {
            continue;
        }
        if let Some(verb) = &block {
            if words.first().is_some_and(|w| w == ")") {
                block = None;
            } else {
                out.push((verb.clone(), words));
            }
            continue;
        }
        let mut words = words.into_iter();
        let Some(verb) = words.next() else { continue };
        let args: Vec<String> = words.collect();
        match args.first().map(String::as_str) {
            Some("(") if args.get(1).is_some_and(|a| a == ")") => {}
            Some("(") => block = Some(verb),
            _ => out.push((verb, args)),
        }
    }
    out
}

/// One line's words, up to a `//` comment: bare ones, quoted ones (`"…"`, `` `…` ``) unquoted,
/// `(`, `)` and `=>`.
fn tokens(line: &str) -> Vec<String> {
    let cs: Vec<char> = line.chars().collect();
    let at = |i: usize| cs.get(i).copied();
    let mut out = Vec::new();
    let mut i = 0usize;
    while let Some(c) = at(i) {
        let next = at(i + 1);
        if c.is_whitespace() {
            i += 1;
        } else if c == '/' && next == Some('/') {
            break;
        } else if c == '(' || c == ')' {
            out.push(c.to_string());
            i += 1;
        } else if c == '=' && next == Some('>') {
            out.push("=>".to_owned());
            i += 2;
        } else if c == '"' || c == '`' {
            let mut w = String::new();
            i += 1;
            while let Some(d) = at(i) {
                i += 1;
                if d == c {
                    break;
                }
                if c == '"' && d == '\\' {
                    w.extend(at(i));
                    i += 1;
                    continue;
                }
                w.push(d);
            }
            out.push(w);
        } else {
            let mut w = String::new();
            while let Some(d) = at(i) {
                let ends = d.is_whitespace() || matches!(d, '"' | '`' | '(' | ')') || (d == '/' && at(i + 1) == Some('/')) || (d == '=' && at(i + 1) == Some('>'));
                if ends {
                    break;
                }
                w.push(d);
                i += 1;
            }
            out.push(w);
        }
    }
    out
}

/// The folder a `replace` points at, when it's a local one (`old [v] => ./dir`): Go's rule is a
/// path that starts `./` or `../`, with no version after it.
fn local_replacement(args: &[String]) -> Option<&str> {
    let at = args.iter().position(|a| a == "=>")?;
    let mut rest = args.iter().skip(at + 1);
    let target = rest.next()?;
    let local = target == "." || target == ".." || target.starts_with("./") || target.starts_with("../");
    (local && rest.next().is_none()).then_some(target.as_str())
}

// ------------------------------------------------------------------ paths and globs

/// `name` in folder `dir`.
fn file(dir: &str, name: &str) -> String {
    if dir.is_empty() { name.to_owned() } else { format!("{dir}/{name}") }
}

/// A folder as a problem names it.
fn shown(dir: &str) -> &str {
    if dir.is_empty() { "." } else { dir }
}

/// Whether `path` is folder `dir` or inside it (everything is inside the root, `""`).
fn under(dir: &str, path: &str) -> bool {
    dir.is_empty() || path.strip_prefix(dir).is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// The folder above `dir`: `None` above the root.
fn parent(dir: &str) -> Option<&str> {
    (!dir.is_empty()).then(|| dir.rsplit_once('/').map_or("", |(up, _)| up))
}

/// `dir` from folder `up`, which holds it.
fn relative<'a>(up: &str, dir: &'a str) -> &'a str {
    if up.is_empty() { dir } else { dir.strip_prefix(up).and_then(|rest| rest.strip_prefix('/')).unwrap_or(dir) }
}

/// `rel` from folder `base`, with `.` and `..` resolved: `None` when it leaves the repository, or
/// is absolute.
fn join(base: &str, rel: &str) -> Option<String> {
    if rel.starts_with('/') {
        return None;
    }
    let mut parts: Vec<&str> = base.split('/').filter(|s| !s.is_empty()).collect();
    for seg in rel.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            s => parts.push(s),
        }
    }
    Some(parts.join("/"))
}

fn strings(v: Option<&toml::Value>) -> Vec<String> {
    v.and_then(toml::Value::as_array).into_iter().flatten().filter_map(toml::Value::as_str).map(str::to_owned).collect()
}

fn json_strings(v: Option<&Json>) -> Vec<String> {
    v.and_then(Json::as_array).into_iter().flatten().filter_map(Json::as_str).map(str::to_owned).collect()
}

fn is_glob(pattern: &str) -> bool {
    pattern.contains(['*', '?', '[', '{'])
}

/// Whether folder `dir` matches `pattern` as workspaces' member patterns match: `*` and `?` within
/// a name, `[...]` one of a set (`[!...]` or `[^...]` one outside it), `{a,b}` either, and `**` as
/// a whole name any number of folders, none included. A leading `./` and a trailing `/` don't count.
fn glob(pattern: &str, dir: &str) -> bool {
    let mut alternatives = Vec::new();
    braces(pattern, &mut alternatives);
    let names: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    alternatives.iter().any(|p| {
        let parts: Vec<&str> = p.split('/').filter(|s| !s.is_empty() && *s != ".").collect();
        folders(&parts, &names)
    })
}

/// `pattern` with its `{a,b}` alternatives spelled out, at most 64 of them.
fn braces(pattern: &str, out: &mut Vec<String>) {
    if out.len() >= 64 {
        return;
    }
    let Some(open) = pattern.find('{') else {
        out.push(pattern.to_owned());
        return;
    };
    let (mut depth, mut commas, mut close) = (0usize, Vec::new(), None);
    for (i, c) in pattern.char_indices().skip_while(|(i, _)| *i < open) {
        match c {
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    close = Some(i);
                    break;
                }
            }
            ',' if depth == 1 => commas.push(i),
            _ => {}
        }
    }
    let (Some(close), Some(before), false) = (close, pattern.get(..open), commas.is_empty()) else {
        out.push(pattern.to_owned());
        return;
    };
    let after = pattern.get(close + 1..).unwrap_or_default();
    let mut start = open + 1;
    for end in commas.into_iter().chain([close]) {
        braces(&format!("{before}{}{after}", pattern.get(start..end).unwrap_or_default()), out);
        start = end + 1;
    }
}

/// Whether a pattern's names match a folder's, `**` taking any number of them.
fn folders(pattern: &[&str], names: &[&str]) -> bool {
    match pattern.split_first() {
        None => names.is_empty(),
        Some((&"**", rest)) => (0..=names.len()).any(|skip| names.get(skip..).is_some_and(|tail| folders(rest, tail))),
        Some((first, rest)) => names.split_first().is_some_and(|(name, tail)| name_matches(first, name) && folders(rest, tail)),
    }
}

/// Whether one name matches one name of a pattern: `*` any run of characters, `?` any one,
/// `[...]` one of a set, `\` the next character itself.
fn name_matches(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let s: Vec<char> = name.chars().collect();
    let (mut i, mut j) = (0usize, 0usize);
    // After the last `*`: where the pattern goes on, and the first character that `*` hasn't taken.
    let mut star: Option<(usize, usize)> = None;
    while let Some(&c) = s.get(j) {
        if p.get(i) == Some(&'*') {
            i += 1;
            star = Some((i, j));
        } else if let Some(width) = one(&p, i, c) {
            i += width;
            j += 1;
        } else if let Some((resume, taken)) = star {
            i = resume;
            j = taken + 1;
            star = Some((resume, taken + 1));
        } else {
            return false;
        }
    }
    p.get(i..).is_some_and(|rest| rest.iter().all(|c| *c == '*'))
}

/// Whether the pattern's atom at `i` (one character's worth) matches `c`: its width if it does.
fn one(p: &[char], i: usize, c: char) -> Option<usize> {
    match *p.get(i)? {
        '?' => Some(1),
        '\\' if p.get(i + 1).is_some() => (p.get(i + 1) == Some(&c)).then_some(2),
        '[' => match class(p, i, c) {
            Some((hit, width)) => hit.then_some(width),
            None => (c == '[').then_some(1),
        },
        x => (x == c).then_some(1),
    }
}

/// The `[...]` set at `i`: whether it holds `c`, and its width; `None` when no `]` closes it.
fn class(p: &[char], i: usize, c: char) -> Option<(bool, usize)> {
    let mut k = i + 1;
    let negated = matches!(p.get(k), Some(&('!' | '^')));
    if negated {
        k += 1;
    }
    let (mut hit, mut first) = (false, true);
    loop {
        let x = *p.get(k)?;
        if x == ']' && !first {
            break;
        }
        first = false;
        match (p.get(k + 1), p.get(k + 2)) {
            (Some(&'-'), Some(&end)) if end != ']' => {
                hit |= x <= c && c <= end;
                k += 3;
            }
            _ => {
                hit |= x == c;
                k += 1;
            }
        }
    }
    Some((hit != negated, k + 1 - i))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The graph of a tree held in memory: (path, text) pairs.
    fn graph(files: &[(&str, &str)]) -> Graph {
        let tree: BTreeMap<String, String> = files.iter().map(|(p, t)| ((*p).to_owned(), (*t).to_owned())).collect();
        let paths: Vec<String> = tree.keys().cloned().collect();
        read(&|p| tree.get(p).cloned(), &paths)
    }

    fn members(g: &Graph, eco: Ecosystem) -> Vec<&str> {
        g.of(eco).filter(|p| p.member).map(|p| p.name.as_str()).collect::<BTreeSet<_>>().into_iter().collect()
    }

    fn package<'g>(g: &'g Graph, eco: Ecosystem, name: &str) -> Option<&'g Package> {
        g.of(eco).find(|p| p.name == name)
    }

    fn deps<'g>(g: &'g Graph, eco: Ecosystem, name: &str) -> Vec<&'g str> {
        package(g, eco, name).map(|p| p.deps.iter().map(String::as_str).collect()).unwrap_or_default()
    }

    fn hit(g: &Graph, eco: Ecosystem, changed: &[&str]) -> Vec<String> {
        affected(g, eco, &changed.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>()).into_iter().collect()
    }

    const CARGO: &[(&str, &str)] = &[
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/*\", \"tools/gen\"]\nexclude = [\"crates/legacy\"]\n\n[workspace.dependencies]\ncore = { path = \"crates/core\" }\nserde = \"1\"\n\n[patch.crates-io]\nvendored = { path = \"third_party/vendored\" }\n",
        ),
        ("Cargo.lock", "version = 4\n"),
        ("crates/core/Cargo.toml", "[package]\nname = \"core\"\n"),
        ("crates/core/src/lib.rs", ""),
        ("crates/api/Cargo.toml", "[package]\nname = \"api\"\n\n[dependencies]\ncore = { workspace = true }\nserde.workspace = true\n"),
        ("crates/cli/Cargo.toml", "[package]\nname = \"cli\"\n\n[dependencies]\napi = { path = \"../api\" }\n\n[dev-dependencies]\ntestkit = { path = \"../../testkit\" }\n\n[target.'cfg(unix)'.dependencies]\nvendored = \"1\"\n"),
        ("crates/legacy/Cargo.toml", "[package]\nname = \"legacy\"\n\n[dependencies]\ncore = { path = \"../core\" }\n"),
        ("tools/gen/Cargo.toml", "[package]\nname = \"gen\"\n\n[build-dependencies]\ncore.workspace = true\n"),
        ("testkit/Cargo.toml", "[package]\nname = \"testkit\"\n"),
        ("third_party/vendored/Cargo.toml", "[package]\nname = \"vendored\"\n\n[dev-dependencies]\nnot-vendored = { path = \"../not-vendored\" }\n"),
        ("docs/guide.md", ""),
    ];

    /// Members from globs less `exclude`, a member's path dependency joining them, and every kind
    /// of dependency: path, `workspace = true`, build, dev, a target's, and `[patch]`'s.
    #[test]
    fn w24_r16_cargo_members_and_dependencies() {
        let g = graph(CARGO);
        assert!(g.problems.is_empty(), "{:?}", g.problems);
        assert_eq!(members(&g, Ecosystem::Cargo), ["api", "cli", "core", "gen", "testkit"], "`exclude` keeps legacy out; testkit joins as cli's path dependency");
        assert!(package(&g, Ecosystem::Cargo, "legacy").is_none());
        let vendored = package(&g, Ecosystem::Cargo, "vendored");
        assert_eq!(vendored.map(|p| (p.member, p.dir.as_str())), Some((false, "third_party/vendored")), "a [patch]'s folder is local code, not a member");
        assert_eq!(deps(&g, Ecosystem::Cargo, "api"), ["core"], "through [workspace.dependencies]; serde is the registry's");
        assert_eq!(deps(&g, Ecosystem::Cargo, "cli"), ["api", "testkit", "vendored"]);
        assert_eq!(deps(&g, Ecosystem::Cargo, "gen"), ["core"]);
        assert_eq!(package(&g, Ecosystem::Cargo, "core").map(|p| p.dir.as_str()), Some("crates/core"));
    }

    /// Each changed file's package and everything that depends on it, transitively; members only.
    #[test]
    fn w24_r17_cargo_reverse_closure() {
        let g = graph(CARGO);
        assert_eq!(hit(&g, Ecosystem::Cargo, &["crates/core/src/lib.rs"]), ["api", "cli", "core", "gen"]);
        assert_eq!(hit(&g, Ecosystem::Cargo, &["crates/api/Cargo.toml"]), ["api", "cli"]);
        assert_eq!(hit(&g, Ecosystem::Cargo, &["./crates/cli/src/main.rs"]), ["cli"]);
        assert_eq!(hit(&g, Ecosystem::Cargo, &["third_party/vendored/src/lib.rs"]), ["cli"], "the [patch]'s code reaches its dependents, but isn't listed");
        assert_eq!(hit(&g, Ecosystem::Cargo, &["testkit/src/lib.rs", "tools/gen/build.rs"]), ["cli", "gen", "testkit"]);
        assert_eq!(hit(&g, Ecosystem::Cargo, &["crates/legacy/src/lib.rs", "docs/guide.md", ".kiln/ci.yaml"]), Vec::<String>::new(), "files in no package affect none");
        let all = ["api", "cli", "core", "gen", "testkit"];
        assert_eq!(hit(&g, Ecosystem::Cargo, &["Cargo.lock"]), all, "a lockfile: every package");
        assert_eq!(hit(&g, Ecosystem::Cargo, &["Cargo.toml"]), all, "the workspace's manifest: every package");
        assert_eq!(hit(&g, Ecosystem::Cargo, &[]), Vec::<String>::new());
        assert_eq!(hit(&g, Ecosystem::Npm, &["crates/core/src/lib.rs"]), Vec::<String>::new(), "another ecosystem's");
    }

    /// A vendored workspace of its own (kiln's `vendor/turso`): its `workspace = true`
    /// dependencies resolve against its root, its root manifest affects its packages, and its
    /// dev-dependencies, which Cargo doesn't build for it, aren't read.
    #[test]
    fn w24_r16_cargo_workspace_true_in_a_nested_workspace() {
        let g = graph(&[
            ("Cargo.toml", "[workspace]\nmembers = [\"app\"]\nexclude = [\"vendor/lib\"]\n"),
            ("app/Cargo.toml", "[package]\nname = \"app\"\n\n[dependencies]\nlib-core = { path = \"../vendor/lib/core\" }\n"),
            ("vendor/lib/Cargo.toml", "[workspace]\nmembers = [\"core\", \"util\"]\n\n[workspace.dependencies]\nlib-util = { path = \"util\" }\n"),
            ("vendor/lib/core/Cargo.toml", "[package]\nname = \"lib-core\"\n\n[dependencies]\nlib-util.workspace = true\n\n[dev-dependencies]\ngone = { path = \"../gone\" }\n"),
            ("vendor/lib/util/Cargo.toml", "[package]\nname = \"lib-util\"\n"),
        ]);
        assert!(g.problems.is_empty(), "{:?}", g.problems);
        assert_eq!(members(&g, Ecosystem::Cargo), ["app"], "a path dependency `exclude` keeps out isn't a member");
        assert_eq!(package(&g, Ecosystem::Cargo, "lib-core").map(|p| p.workspace.as_deref()), Some(Some("vendor/lib")));
        assert_eq!(deps(&g, Ecosystem::Cargo, "lib-core"), ["lib-util"]);
        assert_eq!(hit(&g, Ecosystem::Cargo, &["vendor/lib/util/src/lib.rs"]), ["app"]);
        assert_eq!(hit(&g, Ecosystem::Cargo, &["vendor/lib/Cargo.toml"]), ["app"], "its workspace's manifest affects lib-core");
        assert_eq!(hit(&g, Ecosystem::Cargo, &["vendor/lib/README.md"]), Vec::<String>::new());
    }

    /// The root as a package: with no `[workspace]`, alone; with one, beside its members, and
    /// holding every file no deeper package does.
    #[test]
    fn w24_r16_cargo_root_packages() {
        let single = graph(&[("Cargo.toml", "[package]\nname = \"one\"\n\n[dependencies]\nlocal = { path = \"local\" }\n"), ("local/Cargo.toml", "[package]\nname = \"local\"\n")]);
        assert!(single.problems.is_empty(), "{:?}", single.problems);
        assert_eq!(members(&single, Ecosystem::Cargo), ["one"], "with no [workspace], a path dependency is its own package");
        assert_eq!(hit(&single, Ecosystem::Cargo, &["local/src/lib.rs"]), ["one"]);
        let both = graph(&[("Cargo.toml", "[package]\nname = \"root\"\n\n[workspace]\nmembers = [\"sub\"]\n"), ("sub/Cargo.toml", "[package]\nname = \"sub\"\n")]);
        assert_eq!(members(&both, Ecosystem::Cargo), ["root", "sub"]);
        assert_eq!(hit(&both, Ecosystem::Cargo, &["docs/x.md"]), ["root"], "the root package's folder holds it");
        assert_eq!(hit(&both, Ecosystem::Cargo, &["sub/src/lib.rs"]), ["sub"]);
    }

    /// What can't be read is a problem, and the rest is still read.
    #[test]
    fn w24_r16_cargo_problems() {
        let g = graph(&[
            ("Cargo.toml", "[workspace]\nmembers = [\"crates/*\", \"missing\"]\n"),
            ("crates/bad/Cargo.toml", "[package\nname = \"bad\"\n"),
            ("crates/anon/Cargo.toml", "[dependencies]\n"),
            ("crates/ok/Cargo.toml", "[package]\nname = \"ok\"\n\n[dependencies]\nnope = { workspace = true }\nghost = { path = \"../ghost\" }\noutside = { path = \"../../../elsewhere\" }\n"),
        ]);
        assert_eq!(members(&g, Ecosystem::Cargo), ["ok"]);
        let said: Vec<String> = g.problems.iter().map(ToString::to_string).collect();
        assert_eq!(said.len(), 5, "{said:#?}");
        assert!(said.iter().any(|s| s.starts_with("crates/bad/Cargo.toml: isn't valid TOML (line ")), "{said:#?}");
        assert!(said.contains(&"crates/anon/Cargo.toml: has no `[package]` `name`".to_owned()), "{said:#?}");
        assert!(said.contains(&"Cargo.toml: names `missing`, which has no Cargo.toml".to_owned()), "{said:#?}");
        assert!(said.contains(&"crates/ok/Cargo.toml: `nope.workspace = true`, but Cargo.toml has no `[workspace.dependencies]` `nope`".to_owned()), "{said:#?}");
        assert!(said.contains(&"crates/ok/Cargo.toml: names `crates/ghost`, which has no Cargo.toml".to_owned()), "{said:#?}");
        assert!(g.problems.iter().all(|p| p.ecosystem == Ecosystem::Cargo));
        assert!(graph(&[("Cargo.toml", "[package\n")]).packages.is_empty());
    }

    /// npm's and yarn's `workspaces` (a list with `!` and `{a,b}`, or an object's `packages`):
    /// every kind of dependency that names a workspace package, by any specifier.
    #[test]
    fn w24_r16_npm_and_yarn_workspaces() {
        let packages = [
            ("packages/ui/package.json", "{\"name\": \"@acme/ui\", \"dependencies\": {\"react\": \"^18\", \"@acme/utils\": \"workspace:^\"}}"),
            ("packages/utils/package.json", "{\"name\": \"@acme/utils\"}"),
            ("packages/scratch/package.json", "{\"name\": \"scratch\", \"dependencies\": {\"@acme/ui\": \"*\"}}"),
            ("packages/ui/node_modules/react/package.json", "{\"name\": \"react\"}"),
            ("apps/web/package.json", "{\"name\": \"web\", \"dependencies\": {\"@acme/ui\": \"workspace:*\"}, \"devDependencies\": {\"@acme/utils\": \"1.0.0\"}}"),
            ("apps/docs/package.json", "{\"peerDependencies\": {\"@acme/ui\": \"^1\"}, \"optionalDependencies\": {\"web\": \"*\"}}"),
            ("apps/admin/package.json", "{\"name\": \"admin\", \"dependencies\": {\"@acme/ui\": \"*\"}}"),
        ];
        for root in ["{\"name\": \"monorepo\", \"private\": true, \"workspaces\": [\"packages/*\", \"./apps/{web,docs}/\", \"!packages/scratch\"]}", "{\"workspaces\": {\"packages\": [\"packages/*\", \"apps/{web,docs}\", \"!packages/scratch\"], \"nohoist\": []}}"] {
            let mut files = vec![("package.json", root), ("yarn.lock", "")];
            files.extend(packages);
            let g = graph(&files);
            assert!(g.problems.is_empty(), "{:?}", g.problems);
            assert_eq!(members(&g, Ecosystem::Npm), ["@acme/ui", "@acme/utils", "apps/docs", "web"], "the root and what the patterns leave out aren't packages; a package without a name goes by its folder");
            assert_eq!(deps(&g, Ecosystem::Npm, "web"), ["@acme/ui", "@acme/utils"]);
            assert_eq!(deps(&g, Ecosystem::Npm, "apps/docs"), ["@acme/ui", "web"]);
            assert_eq!(hit(&g, Ecosystem::Npm, &["packages/utils/index.js"]), ["@acme/ui", "@acme/utils", "apps/docs", "web"]);
            assert_eq!(hit(&g, Ecosystem::Npm, &["apps/web/src/app.tsx"]), ["apps/docs", "web"]);
            assert_eq!(hit(&g, Ecosystem::Npm, &["packages/scratch/x.js", "apps/admin/x.js", "README.md"]), Vec::<String>::new());
            assert_eq!(hit(&g, Ecosystem::Npm, &["yarn.lock"]).len(), 4);
            assert_eq!(hit(&g, Ecosystem::Npm, &["package.json"]).len(), 4);
        }
    }

    /// pnpm's `pnpm-workspace.yaml`, with `**` and a `!` pattern; a root `package.json` with no
    /// workspaces of its own is the workspace's, and a root with no workspace at all is the one
    /// package.
    #[test]
    fn w24_r16_pnpm_workspaces_and_lone_packages() {
        let g = graph(&[
            ("package.json", "{\"name\": \"root\", \"devDependencies\": {\"a\": \"workspace:*\"}}"),
            ("pnpm-workspace.yaml", "packages:\n  - 'packages/**'\n  - '!**/test/**'\ncatalog:\n  react: ^18\n"),
            ("pnpm-lock.yaml", ""),
            ("packages/a/package.json", "{\"name\": \"a\"}"),
            ("packages/group/b/package.json", "{\"name\": \"b\", \"dependencies\": {\"a\": \"workspace:*\"}}"),
            ("packages/a/test/fixture/package.json", "{\"name\": \"fixture\"}"),
        ]);
        assert!(g.problems.is_empty(), "{:?}", g.problems);
        assert_eq!(members(&g, Ecosystem::Npm), ["a", "b"]);
        assert_eq!(hit(&g, Ecosystem::Npm, &["packages/a/test/fixture/x.js"]), ["a", "b"], "the excluded fixture's files are a's");
        assert_eq!(hit(&g, Ecosystem::Npm, &["pnpm-lock.yaml"]), ["a", "b"]);
        assert_eq!(hit(&g, Ecosystem::Npm, &["pnpm-workspace.yaml"]), ["a", "b"]);
        let lone = graph(&[("package.json", "{\"name\": \"site\"}"), ("pnpm-workspace.yaml", "onlyBuiltDependencies:\n  - esbuild\n"), ("src/index.js", "")]);
        assert_eq!(members(&lone, Ecosystem::Npm), ["site"], "a pnpm-workspace.yaml of settings alone declares no workspace");
        assert_eq!(hit(&lone, Ecosystem::Npm, &["src/index.js"]), ["site"]);
        let broken = graph(&[("package.json", "{\"name\": "), ("pnpm-workspace.yaml", "packages: [")]);
        assert_eq!(broken.problems_of(Ecosystem::Npm).count(), 2, "{:?}", broken.problems);
    }

    /// uv's workspace: members less `exclude`, the root a package of its own, workspace sources
    /// (a member's own and the root's) and path sources, by normalized names.
    #[test]
    fn w24_r16_uv_workspace() {
        let g = graph(&[
            (
                "pyproject.toml",
                "[project]\nname = \"albatross\"\ndependencies = [\"bird-feeder\", \"tqdm>=4,<5\"]\n\n[tool.uv.sources]\nbird-feeder = { workspace = true }\n\n[tool.uv.workspace]\nmembers = [\"packages/*\"]\nexclude = [\"packages/seeds\"]\n",
            ),
            ("uv.lock", ""),
            ("packages/bird-feeder/pyproject.toml", "[project]\nname = \"Bird_Feeder\"\ndependencies = [\"Seeds.Lib>=1\", \"anyio\"]\n\n[tool.uv.sources]\nseeds-lib = { path = \"../seeds\", editable = true }\n"),
            ("packages/seeds/pyproject.toml", "[project]\nname = \"seeds-lib\"\n"),
            (
                "packages/feathers/pyproject.toml",
                "[project]\nname = \"feathers\"\n\n[project.optional-dependencies]\nviz = [\"bird-feeder[extra]; python_version >= '3.11'\"]\n\n[dependency-groups]\ndev = [\"albatross\", { include-group = \"lint\" }]\nlint = [\"ruff\"]\n\n[tool.uv.sources]\nalbatross = [{ workspace = true, marker = \"sys_platform == 'linux'\" }]\n",
            ),
            ("packages/tools/pyproject.toml", "[project]\nname = \"tools\"\n\n[tool.uv]\ndev-dependencies = [\"feathers\"]\n"),
        ]);
        assert!(g.problems.is_empty(), "{:?}", g.problems);
        assert_eq!(members(&g, Ecosystem::Uv), ["Bird_Feeder", "albatross", "feathers", "tools"]);
        assert_eq!(package(&g, Ecosystem::Uv, "seeds-lib").map(|p| p.member), Some(false), "a path source outside the members");
        assert_eq!(deps(&g, Ecosystem::Uv, "albatross"), ["Bird_Feeder"]);
        assert_eq!(deps(&g, Ecosystem::Uv, "Bird_Feeder"), ["seeds-lib"]);
        assert_eq!(deps(&g, Ecosystem::Uv, "feathers"), ["Bird_Feeder", "albatross"], "bird-feeder through the root's sources");
        assert_eq!(deps(&g, Ecosystem::Uv, "tools"), Vec::<&str>::new(), "feathers has no source: the index's");
        assert_eq!(hit(&g, Ecosystem::Uv, &["packages/seeds/seeds/__init__.py"]), ["Bird_Feeder", "albatross", "feathers"]);
        assert_eq!(hit(&g, Ecosystem::Uv, &["README.md"]), ["albatross", "feathers"], "the root project holds what no deeper package does");
        assert_eq!(hit(&g, Ecosystem::Uv, &["uv.lock"]).len(), 4);
    }

    /// Go's workspace: `go.work`'s `use` (one and a block, comments, a quoted path), each module's
    /// `require`s of the others and its local `replace`s, and `go.work`'s.
    #[test]
    fn w24_r16_go_workspace() {
        let g = graph(&[
            ("go.work", "go 1.22\n\nuse (\n\t./api\n\t./cmd/server // the binary\n)\nuse \"./lib\"\n\nreplace example.com/shared => ./third_party/shared\n"),
            ("go.work.sum", ""),
            ("api/go.mod", "module example.com/api\n\ngo 1.22\n\nrequire (\n\texample.com/lib v0.0.0\n\tgithub.com/x/y v1.2.3 // indirect\n)\n"),
            ("cmd/server/go.mod", "module example.com/server\n\nrequire example.com/api v0.1.0\n\nreplace example.com/tools v1.0.0 => ../../tools\nreplace example.com/net => example.com/fork v1.0.0\n"),
            ("lib/go.mod", "module `example.com/lib`\n"),
            ("tools/go.mod", "module example.com/tools\n\nrequire example.com/lib v0.0.0\n"),
            ("third_party/shared/go.mod", "module example.com/shared\n"),
        ]);
        assert!(g.problems.is_empty(), "{:?}", g.problems);
        assert_eq!(members(&g, Ecosystem::Go), ["example.com/api", "example.com/lib", "example.com/server"]);
        assert_eq!(package(&g, Ecosystem::Go, "example.com/tools").map(|p| (p.member, p.dir.as_str())), Some((false, "tools")));
        assert_eq!(deps(&g, Ecosystem::Go, "example.com/api"), ["example.com/lib", "example.com/shared"]);
        assert_eq!(deps(&g, Ecosystem::Go, "example.com/server"), ["example.com/api", "example.com/shared", "example.com/tools"]);
        assert_eq!(hit(&g, Ecosystem::Go, &["lib/lib.go"]), ["example.com/api", "example.com/lib", "example.com/server"], "tools needs lib too, and server needs tools");
        assert_eq!(hit(&g, Ecosystem::Go, &["tools/main.go"]), ["example.com/server"]);
        assert_eq!(hit(&g, Ecosystem::Go, &["third_party/shared/x.go"]).len(), 3, "go.work's replacement is every module's");
        assert_eq!(hit(&g, Ecosystem::Go, &["go.work"]).len(), 3);
        assert_eq!(hit(&g, Ecosystem::Go, &["README.md"]), Vec::<String>::new());
        let lone = graph(&[("go.mod", "module example.com/one\n"), ("main.go", "")]);
        assert_eq!(members(&lone, Ecosystem::Go), ["example.com/one"]);
        assert_eq!(hit(&lone, Ecosystem::Go, &["main.go", "internal/x/x.go"]), ["example.com/one"]);
        let broken = graph(&[("go.work", "use ./gone\nuse ./anon\n"), ("anon/go.mod", "go 1.22\n")]);
        let said: Vec<String> = broken.problems.iter().map(ToString::to_string).collect();
        assert_eq!(said, ["go.work: names `gone`, which has no go.mod", "anon/go.mod: has no `module` directive"]);
    }

    #[test]
    fn w24_r16_member_globs() {
        let yes: &[(&str, &str)] = &[
            ("crates/*", "crates/kiln"),
            ("crates/*/", "crates/kiln"),
            ("./crates/*", "crates/kiln"),
            ("crates/kiln-*", "crates/kiln-ci"),
            ("crates/k?ln", "crates/kiln"),
            ("crates/[jk]iln", "crates/kiln"),
            ("crates/[!a-c]iln", "crates/kiln"),
            ("crates/[a-z]*", "crates/kiln"),
            ("packages/**", "packages/a/b"),
            ("packages/**", "packages"),
            ("**/pkg", "a/b/pkg"),
            ("**", ""),
            ("apps/{web,docs}", "apps/docs"),
            ("{a,b/{c,d}}", "b/d"),
            ("a\\*", "a*"),
            (".", ""),
        ];
        let no: &[(&str, &str)] = &[
            ("crates/*", "crates/kiln/tests"),
            ("crates/*", "crates"),
            ("crates/kiln-*", "crates/kiln"),
            ("crates/k?ln", "crates/kln"),
            ("crates/[!jk]iln", "crates/kiln"),
            ("**/pkg", "a/pkg/b"),
            ("apps/{web,docs}", "apps/admin"),
            ("a\\*", "ab"),
            ("crates/[ab", "crates/a"),
        ];
        for (p, d) in yes {
            assert!(glob(p, d), "`{p}` should match `{d}`");
        }
        for (p, d) in no {
            assert!(!glob(p, d), "`{p}` should not match `{d}`");
        }
        assert!(glob("crates/[ab", "crates/[ab"), "an open `[` is itself");
        assert_eq!(join("crates/kiln", "../../vendor/turso/core").as_deref(), Some("vendor/turso/core"));
        assert_eq!(join("a", "../.."), None);
        assert_eq!(join("a", "/abs"), None);
        assert_eq!(join("", "./x/./y/"), Some("x/y".to_owned()));
    }

    #[test]
    fn w24_r16_go_tokens() {
        assert_eq!(tokens("replace a v1 => ../b // why"), ["replace", "a", "v1", "=>", "../b"]);
        assert_eq!(tokens("require \"a\\\"b\" v1"), ["require", "a\"b", "v1"]);
        assert_eq!(tokens("x=>y"), ["x", "=>", "y"]);
        assert_eq!(tokens("// only a comment"), Vec::<String>::new());
        assert_eq!(local_replacement(&tokens("a => ./b")), Some("./b"));
        assert_eq!(local_replacement(&tokens("a => b v1")), None);
        assert_eq!(directives("require ()\nmodule m\n"), [("module".to_owned(), vec!["m".to_owned()])]);
    }

    #[test]
    fn w24_r17_ecosystem_names() {
        assert_eq!(Ecosystem::parse("Cargo"), Some(Ecosystem::Cargo));
        assert_eq!(Ecosystem::parse("pnpm"), Some(Ecosystem::Npm));
        assert_eq!(Ecosystem::parse("yarn"), Some(Ecosystem::Npm));
        assert_eq!(Ecosystem::parse(" uv "), Some(Ecosystem::Uv));
        assert_eq!(Ecosystem::parse("go"), Some(Ecosystem::Go));
        assert_eq!(Ecosystem::parse("maven"), None);
        assert!(is_manifest("crates/kiln/Cargo.toml") && is_manifest("go.work") && !is_manifest("Cargo.lock"));
    }

    /// Every manifest under `root`'s folder `dir`, outside `.git`, `target` and `node_modules`.
    fn manifests(root: &std::path::Path, dir: &str, out: &mut Vec<String>) -> std::io::Result<()> {
        for entry in std::fs::read_dir(root.join(dir))? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = file(dir, &name);
            if entry.file_type()?.is_dir() {
                if !matches!(name.as_str(), ".git" | "target" | "node_modules") {
                    manifests(root, &path, out)?;
                }
            } else if is_manifest(&path) {
                out.push(path);
            }
        }
        Ok(())
    }

    /// kiln's own workspace, read from this checkout: its crates, how they depend on each other,
    /// and what a change affects, vendored code included (its `[patch.crates-io]` and
    /// `vendor/turso`, a workspace of its own that kiln's `exclude` keeps out).
    #[test]
    fn w24_r16_kilns_own_workspace() -> std::io::Result<()> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut paths = Vec::new();
        manifests(&root, "", &mut paths)?;
        let g = read(&|p| std::fs::read_to_string(root.join(p)).ok(), &paths);
        assert!(g.problems.is_empty(), "{:?}", g.problems);
        let crates = members(&g, Ecosystem::Cargo);
        for c in ["kiln", "kiln-cd", "kiln-ci", "kiln-http", "kiln-seams"] {
            assert!(crates.contains(&c), "{c} in {crates:?}");
        }
        assert!(g.of(Ecosystem::Cargo).filter(|p| p.member).all(|p| p.dir.starts_with("crates/")), "only `crates/*` are members");
        assert!(deps(&g, Ecosystem::Cargo, "kiln").contains(&"kiln-ci"));
        let ci = hit(&g, Ecosystem::Cargo, &["crates/kiln-ci/src/work/graph.rs"]);
        assert!(ci.contains(&"kiln-ci".to_owned()) && ci.contains(&"kiln".to_owned()), "{ci:?}");
        assert!(!ci.contains(&"kiln-seams".to_owned()), "{ci:?}");
        assert_eq!(hit(&g, Ecosystem::Cargo, &["docs/ci.md", ".kiln/ci.yaml"]), Vec::<String>::new());
        assert_eq!(hit(&g, Ecosystem::Cargo, &["Cargo.lock"]), crates);
        assert!(hit(&g, Ecosystem::Cargo, &["vendor/turso/core/lib.rs"]).contains(&"kiln".to_owned()));
        assert!(hit(&g, Ecosystem::Cargo, &["vendor/hyper-raft/crates/hyper-rt/src/lib.rs"]).contains(&"kiln-http".to_owned()));
        Ok(())
    }
}
