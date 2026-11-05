//! Code workflows at plan time (docs/ci.md W29-R27). A `.kiln/<name>.ts` or `.js` runs in code
//! mode's QuickJS ([`crate::mcp::code::engine`]) with plan-time limits. Its default export, a
//! kiln-dialect workflow or a function of the event that returns one, is what W02 checks and W04
//! plans, like any `.kiln/*.yaml`.
//!
//! - TypeScript's types are erased first ([`kiln_ci::wf::code::erase`]), so each line and column is
//!   the TypeScript's own; what isn't erasable is `KCI1033`.
//! - It imports `@kiln/sdk` ([`kiln_ci::wf::sdk::RUNTIME`]) and nothing else, and reads the run's
//!   event and the commit's tree ([`Tree`]) and nothing else. `Date.now()`, `new Date()`,
//!   `Math.random()` and the engine's other clocks and entropy are refused, so one commit always
//!   plans the same: `KCI1032`, as is another import.
//! - A throw fails the plan (`KCI1030`), as does a limit passed (`KCI1031`: time, memory or
//!   output), naming the line or the limit.
//! - W02's diagnostics on what it made point at the SDK call (`workflow`, `job`, `step`) that made
//!   each part, else at its `export default`.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use kiln_ci::expr::Dialect;
use kiln_ci::wf::diag::{Pos, Range};
use kiln_ci::wf::{self, Diag, Workflow};
use rquickjs::{Ctx, Function, Module, Object};
use serde_json::Value;

use crate::gitops::{self, LS_REMOTE_TIMEOUT};
use crate::hub::Hub;
use crate::mcp::code::{Engine, Limits, engine};
use crate::store::Repo;

/// What a program reads of its commit's tree.
pub trait Tree: Send + Sync {
    /// A file's text, or `None` when there's no file at `path`.
    fn read(&self, path: &str) -> Option<String>;
    /// The entries of the directory `dir`, from the tree's root, a directory's ending in `/`.
    fn list(&self, dir: &str) -> Vec<String>;
}

/// A tree with nothing in it: where a program is checked with no commit at hand.
pub struct NoTree;

impl Tree for NoTree {
    fn read(&self, _path: &str) -> Option<String> {
        None
    }

    fn list(&self, _dir: &str) -> Vec<String> {
        Vec::new()
    }
}

/// A commit's tree in a repository's mirror: its paths listed once, each file read when the
/// program asks, with git on the program's own thread (the blocking pool's).
pub struct MirrorTree {
    dir: std::path::PathBuf,
    sha: String,
    paths: Vec<String>,
}

/// The largest file a program reads: its result's bound.
const READ_MAX: u64 = 4 << 20;

impl MirrorTree {
    /// The tree of `sha` in the mirror at `dir`.
    pub async fn of(hub: &Hub, dir: &std::path::Path, sha: &str) -> anyhow::Result<Arc<dyn Tree>> {
        let d = dir.to_string_lossy().into_owned();
        let out = gitops::run_git(hub, &["-C", &d, "ls-tree", "-r", "-z", "--name-only", sha], LS_REMOTE_TIMEOUT).await?;
        let mut paths: Vec<String> = out.split(|b| *b == 0).filter(|p| !p.is_empty()).map(|p| String::from_utf8_lossy(p).into_owned()).collect();
        paths.sort();
        Ok(Arc::new(Self { dir: dir.to_owned(), sha: sha.to_owned(), paths }))
    }

    fn git(&self, args: &[&str]) -> Option<Vec<u8>> {
        let mut c = std::process::Command::new("git");
        crate::os::git_no_prompts(&mut c).stdin(std::process::Stdio::null()).arg("-C").arg(&self.dir).args(args);
        let out = c.output().ok()?;
        out.status.success().then_some(out.stdout)
    }
}

impl Tree for MirrorTree {
    fn read(&self, path: &str) -> Option<String> {
        let p = in_tree(path)?;
        self.paths.binary_search(&p).ok()?;
        let at = format!("{}:{p}", self.sha);
        let size: u64 = String::from_utf8_lossy(&self.git(&["cat-file", "-s", &at])?).trim().parse().ok()?;
        if size > READ_MAX {
            return None;
        }
        Some(String::from_utf8_lossy(&self.git(&["cat-file", "blob", &at])?).into_owned())
    }

    fn list(&self, dir: &str) -> Vec<String> {
        entries(&self.paths, dir)
    }
}

/// A tree with a draft's edits over it: a file edited reads as edited, one deleted (`None`) as
/// none, and the rest as the tree has them.
pub struct Edited {
    pub base: Arc<dyn Tree>,
    pub edits: std::collections::BTreeMap<String, Option<String>>,
}

impl Tree for Edited {
    fn read(&self, path: &str) -> Option<String> {
        let p = in_tree(path)?;
        match self.edits.get(&p) {
            Some(edited) => edited.clone(),
            None => self.base.read(&p),
        }
    }

    fn list(&self, dir: &str) -> Vec<String> {
        if self.edits.is_empty() {
            return self.base.list(dir);
        }
        // Every file the tree has, less the deleted, with the added, listed as the edited tree is.
        let mut all = all_files(self.base.as_ref(), "");
        for (p, text) in &self.edits {
            if text.is_some() {
                all.insert(p.clone());
            } else {
                all.remove(p);
            }
        }
        entries(&all.into_iter().collect::<Vec<_>>(), dir)
    }
}

/// Every file of `tree` under `dir`.
fn all_files(tree: &dyn Tree, dir: &str) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    for e in tree.list(dir) {
        match e.strip_suffix('/') {
            Some(sub) => out.extend(all_files(tree, sub)),
            None => {
                out.insert(e);
            }
        }
    }
    out
}

/// The tree of commit `sha` of `repo`, fetched into its mirror when it isn't there yet.
pub async fn tree_at(hub: &Hub, repo: &Repo, sha: &str) -> anyhow::Result<Arc<dyn Tree>> {
    let dir = gitops::ensure_commit(hub, repo, sha).await?;
    MirrorTree::of(hub, &dir, sha).await
}

/// `path` as a tree names it (no `./`, no empty parts), or `None` for one outside it: absolute,
/// or with `..`.
fn in_tree(path: &str) -> Option<String> {
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty() && *p != ".").collect();
    (!path.starts_with('/') && !parts.contains(&"..")).then(|| parts.join("/"))
}

/// The entries of `dir` among the sorted `paths`, one level down, a directory's ending in `/`.
pub fn entries(paths: &[String], dir: &str) -> Vec<String> {
    let Some(d) = in_tree(dir) else { return Vec::new() };
    let prefix = if d.is_empty() { String::new() } else { format!("{d}/") };
    let mut out: Vec<String> = Vec::new();
    for rest in paths.iter().filter_map(|p| p.strip_prefix(prefix.as_str())) {
        let e = rest.split_once('/').map_or_else(|| format!("{prefix}{rest}"), |(sub, _)| format!("{prefix}{sub}/"));
        if out.last() != Some(&e) {
            out.push(e);
        }
    }
    out
}

/// Plan time's limits (W29-R27): a 64 MiB heap, 1 s of wall clock, no `kiln` operations and a
/// 4 MiB result.
pub fn limits() -> Limits {
    Limits { heap: 64 << 20, wall: Duration::from_secs(1), calls: 0, result: 4 << 20, ..Limits::default() }
}

/// What a program made: its workflow as JSON, and where each part of it was written.
#[derive(Debug, Clone)]
pub struct Made {
    pub json: String,
    /// The line and column of each SDK call, by the path of the part it made (`jobs.build`,
    /// `jobs.build.steps[0]`).
    pub at: Vec<(String, Pos)>,
    /// Its `export default`'s.
    pub export: Pos,
}

/// The workflow the program at `path` makes for `event`, as W02 reads it, with W02's diagnostics
/// at the program's lines. Blocks: call it on the blocking pool.
pub fn workflow(path: &str, text: &str, event: &Value, tree: Arc<dyn Tree>) -> (Option<Workflow>, Vec<Diag>) {
    let (w, diags, _) = checked(path, text, event, tree);
    (w, diags)
}

/// [`workflow`], and what the program made, to [`place`] what else is said of it. Blocks.
pub fn checked(path: &str, text: &str, event: &Value, tree: Arc<dyn Tree>) -> (Option<Workflow>, Vec<Diag>, Option<Made>) {
    match evaluate(path, text, event, tree, limits()) {
        Err(diags) => (None, diags, None),
        Ok(made) => {
            let (w, diags) = wf::parse_workflow(path, &made.json, Dialect::Kiln);
            (w, diags.into_iter().map(|d| place(d, &made)).collect(), Some(made))
        }
    }
}

/// A diagnostic on what a program made, moved to the SDK call that made its part (by its path).
pub fn place(mut d: Diag, made: &Made) -> Diag {
    let within = |p: &str| p.is_empty() || d.path == p || d.path.strip_prefix(p).is_some_and(|rest| rest.starts_with(['.', '[']));
    let at = made.at.iter().filter(|(p, _)| within(p)).max_by_key(|(p, _)| p.len()).map_or(made.export, |(_, at)| *at);
    d.range = Range { start: at, end: at };
    d.fix = None;
    d.lock = None;
    d
}

/// Plan time's own module, evaluated before the program: the clocks and entropy refused, each
/// refusal told to the server with where the program asked (`refused`), so one caught still fails
/// the plan.
const PLAN: &str = r#"const report = globalThis.__kiln.refused;
const refuse = (what) => {
  const frame = String(new Error().stack ?? "").split("\n").find((l) => l.includes(":") && !l.includes("@kiln/"));
  const m = frame === undefined ? null : /:(\d+):(\d+)\)?\s*$/.exec(frame);
  report(what, m === null ? 0 : Number(m[1]), m === null ? 0 : Number(m[2]));
  throw new Error(`${what} isn't available at plan time: one commit always plans the same`);
};
const RealDate = Date;
const PlanDate = new Proxy(RealDate, {
  apply: () => refuse("Date()"),
  construct: (target, args, newTarget) => (args.length === 0 ? refuse("new Date()") : Reflect.construct(target, args, newTarget)),
});
Object.defineProperty(RealDate, "now", { value: () => refuse("Date.now()"), writable: false, configurable: false });
Object.defineProperty(RealDate.prototype, "constructor", { value: PlanDate, writable: false, configurable: false });
Object.defineProperty(globalThis, "Date", { value: PlanDate, writable: false, configurable: false });
Object.defineProperty(Math, "random", { value: () => refuse("Math.random()"), writable: false, configurable: false });
// QuickJS's `performance` holds a clock in `timeOrigin` too, and its `now` can't be redefined.
Object.defineProperty(globalThis, "performance", { value: Object.freeze({ now: () => refuse("performance.now()") }), writable: false, configurable: false });
delete globalThis.WeakRef;
delete globalThis.FinalizationRegistry;
"#;

/// Why a program made nothing: a diagnostic at a line, or a limit.
fn at(code: &'static str, message: impl Into<String>, line: u32, col: u32) -> Diag {
    let p = Pos { line: line.max(1), col: col.max(1), offset: 0 };
    Diag::error(code, message, Range { start: p, end: p })
}

/// The line and column of the first frame of `stack` in the program at `path`.
fn frame(stack: &str, path: &str) -> Option<(u32, u32)> {
    stack.lines().find_map(|l| {
        let rest = l.split_once(&format!("{path}:"))?.1;
        let mut n = rest.split(|c: char| !c.is_ascii_digit()).filter(|s| !s.is_empty());
        Some((n.next()?.parse().ok()?, n.next()?.parse().ok()?))
    })
}

/// What the program at `path` makes for `event`, or why it makes nothing. Blocks.
pub fn evaluate(path: &str, text: &str, event: &Value, tree: Arc<dyn Tree>, limits: Limits) -> Result<Made, Vec<Diag>> {
    let js = if wf::code::typed(path) { wf::code::erase(text)? } else { text.to_owned() };
    let foreign: Vec<Diag> = wf::code::imports(text)
        .into_iter()
        .filter(|(name, _)| name.as_deref() != Some("@kiln/sdk"))
        .map(|(name, r)| {
            let what = name.map_or_else(|| "an `import()` of a name made as it runs".to_owned(), |n| format!("`{n}`"));
            Diag::error("KCI1032", format!("{what} can't be imported: a code workflow imports `@kiln/sdk` alone"), r)
        })
        .collect();
    if !foreign.is_empty() {
        return Err(foreign);
    }
    let export = wf::code::default_export(text).unwrap_or(Pos { line: 1, col: 1, offset: 0 });
    let Engine { rt: _rt, ctx, timed_out, .. } = engine(&limits).map_err(|e| vec![at(if e.code == "heap" { "KCI1031" } else { "KCI1030" }, e.message, export.line, export.col)])?;
    let refused: Rc<RefCell<Vec<(String, u32, u32)>>> = Rc::default();
    let ran = ctx.with(|ctx| {
        let run = || -> rquickjs::Result<(Option<String>, Option<String>)> {
            let host = Object::new(ctx.clone())?;
            host.set("event", ctx.json_parse(event.to_string())?)?;
            let t = Arc::clone(&tree);
            host.set("read", Function::new(ctx.clone(), move |p: String| t.read(&p))?)?;
            let t = Arc::clone(&tree);
            host.set("list", Function::new(ctx.clone(), move |d: String| t.list(&d))?)?;
            let r = Rc::clone(&refused);
            host.set("refused", Function::new(ctx.clone(), move |what: String, line: u32, col: u32| r.borrow_mut().push((what, line, col)))?)?;
            ctx.globals().set("__kiln", host.clone())?;
            Module::declare(ctx.clone(), "@kiln/plan", PLAN)?.eval()?.1.finish::<()>()?;
            Module::declare(ctx.clone(), "@kiln/sdk", wf::sdk::RUNTIME)?;
            let (m, done) = Module::declare(ctx.clone(), path, js.as_str())?.eval()?;
            done.finish::<()>()?;
            let mut made: rquickjs::Value = m.get("default")?;
            if let Some(f) = made.as_function().cloned() {
                made = f.call((host.get::<_, rquickjs::Value>("event")?,))?;
            }
            if let Some(p) = made.as_promise().cloned() {
                made = p.finish()?;
            }
            let json = ctx.json_stringify(made.clone())?.map(|s| s.to_string()).transpose()?;
            let positions = match host.get::<_, Option<Function>>("positions")? {
                Some(f) => ctx.json_stringify(f.call::<_, rquickjs::Value>((made,))?)?.map(|s| s.to_string()).transpose()?,
                None => None,
            };
            Ok((json, positions))
        };
        run().map_err(|e| thrown(&ctx, &e, path))
    });
    let refusals: Vec<Diag> = refused.borrow().iter().map(|(what, line, col)| at("KCI1032", format!("`{what}` isn't available at plan time: one commit always plans the same"), *line, *col)).collect();
    if !refusals.is_empty() {
        return Err(refusals);
    }
    let (json, positions) = ran.map_err(|(message, line, col)| {
        if timed_out.load(Ordering::Relaxed) {
            vec![at("KCI1031", format!("the program ran past its {} s", limits.wall.as_secs()), line, col)]
        } else if message.contains("out of memory") {
            vec![at("KCI1031", format!("the program used more than its {} MiB", limits.heap >> 20), line, col)]
        } else if message.contains("call stack size") || message.contains("stack overflow") {
            vec![at("KCI1031", format!("the program's calls went past its {} KiB of stack", limits.stack >> 10), line, col)]
        } else {
            vec![at("KCI1030", message, line, col)]
        }
    })?;
    let Some(json) = json else {
        return Err(vec![at("KCI1030", "the program's default export is no workflow: a workflow object, or a function of the event that returns one", export.line, export.col)]);
    };
    if json.len() > limits.result {
        return Err(vec![at("KCI1031", format!("the workflow the program made is {} bytes, more than its {} MiB", json.len(), limits.result >> 20), export.line, export.col)]);
    }
    let at = positions.and_then(|p| serde_json::from_str::<Vec<(String, u32, u32)>>(&p).ok()).unwrap_or_default();
    Ok(Made { json, at: at.into_iter().map(|(p, line, col)| (p, Pos { line, col, offset: 0 })).collect(), export })
}

/// What a program threw, with the line and column of the program it threw at (0 where it was
/// none's).
fn thrown(ctx: &Ctx<'_>, e: &rquickjs::Error, path: &str) -> (String, u32, u32) {
    match e {
        rquickjs::Error::Exception => {}
        rquickjs::Error::Allocation => return ("out of memory".into(), 0, 0),
        _ => return (e.to_string(), 0, 0),
    }
    let caught = ctx.catch();
    let (message, stack) = match caught.as_exception() {
        Some(ex) => (format!("{}: {}", ex.get::<_, String>("name").unwrap_or_else(|_| "Error".into()), ex.message().unwrap_or_default()), ex.stack().unwrap_or_default()),
        None => (ctx.json_stringify(caught).ok().flatten().and_then(|s| s.to_string().ok()).map_or_else(|| "the program threw".into(), |v| format!("the program threw {v}")), String::new()),
    };
    let (line, col) = frame(&stack, path).unwrap_or((0, 0));
    (message, line, col)
}

#[cfg(test)]
mod tests;
