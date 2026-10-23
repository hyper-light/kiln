//! Code hosting (docs/ci.md W14-R01, R02): native repositories. kiln keeps each one as a bare
//! repository under `<data_dir>/hosted/<owner>/<name>.git` (apart from the mirrors of upstream
//! ones) and serves it over HTTPS at `/git/<owner>/<name>.git` (and `/<owner>/<name>.git`, for
//! legs) through `git http-backend`, to a person's or an agent's token (as git's password, or a
//! bearer) and to a leg's `GITHUB_TOKEN`:
//! - a fetch needs `diffs.read` on the repository (a leg: `contents: read`, its own repository);
//! - a push is read before git sees it, and refused whole, as git's own report says, when policy
//!   forbids one of its updates:
//!   - a trunk (the default branch, and `.kiln/review.yaml`'s `trunks`) is written only by the land
//!     queue, unless `direct-push` there names the pusher; an empty repository takes its first
//!     trunk from anyone who may push;
//!   - other branches need `diffs.submit` (a leg: `contents: write`), tags `diffs.land`
//!     (`contents: write`); kiln's own refs are kiln's;
//!   - `refs/for/<trunk>` (`diffs.submit`) takes the commits between the trunk and the one pushed
//!     as diffs (W14-R03, [`super::diffs`]): git stores them, kiln makes the diffs, git's report
//!     says how it went with a link to each, and the ref itself is never kept.
//!
//! A native repository's row says so (`mode`), and its clone URL is its bare repository, so the
//! mirror, polling and events of upstream repositories serve it too; a push wakes the poller at
//! once.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use http::StatusCode;

use super::Fail;
use crate::authz::{Caller, Scope};
use crate::http::server::{self as srv, Request, Response};
use crate::hub::Hub;
use crate::store::Repo;

/// Where native repositories live.
pub fn root(hub: &Hub) -> PathBuf {
    hub.cfg.data("hosted")
}

/// A native repository's bare repository: `<root>/<owner>/<name>.git`.
pub fn path_of(hub: &Hub, name: &str) -> Option<PathBuf> {
    let (owner, repo) = name.split_once('/')?;
    let ok = |s: &str| !s.is_empty() && s != "." && s != ".." && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    (ok(owner) && ok(repo)).then(|| root(hub).join(owner).join(format!("{repo}.git")))
}

/// The bare repository of `repo` when kiln hosts it.
pub fn native_path(hub: &Hub, repo: &Repo) -> Option<PathBuf> {
    (repo.mode == "native").then(|| path_of(hub, &repo.name)).flatten()
}

/// The clone URL a native repository's row carries.
pub fn clone_url(path: &Path) -> String {
    format!("file://{}", path.display())
}

/// Whether `url` is one an import may copy: a network repository (`https://`, `ssh://`, `git://`,
/// `user@host:path`), never a path on the server's own disks or a helper transport.
pub fn importable(url: &str) -> Result<(), String> {
    let url = url.trim();
    let scheme = url.split_once("://").map(|(s, _)| s.to_ascii_lowercase());
    let scp = scheme.is_none() && url.split_once(':').is_some_and(|(host, _)| host.contains('@') && !host.contains('/'));
    let ok = matches!(scheme.as_deref(), Some("https" | "http" | "ssh" | "git")) || scp;
    (ok && !url.starts_with('-')).then_some(()).ok_or_else(|| format!("`{url}`: an import copies from https://, ssh://, git:// or user@host:path"))
}

/// Makes `name`'s bare repository: empty on `branch`, or a copy of `import`.
pub async fn create(hub: &Hub, name: &str, branch: &str, import: Option<&str>) -> anyhow::Result<PathBuf> {
    let path = path_of(hub, name).ok_or_else(|| anyhow::anyhow!("`{name}`: a repository is `owner/name`, of letters, digits, `-`, `_` and `.`"))?;
    if path.join("HEAD").is_file() {
        return Ok(path);
    }
    let parent = path.parent().map(Path::to_path_buf).unwrap_or_else(|| root(hub));
    std::fs::create_dir_all(&parent)?;
    let p = path.to_string_lossy().into_owned();
    match import {
        Some(url) => {
            importable(url).map_err(anyhow::Error::msg)?;
            crate::gitops::run_git(hub, &["clone", "--quiet", "--mirror", "--", url, &p], std::time::Duration::from_secs(1800)).await?;
        }
        None => {
            crate::gitops::run_git(hub, &["init", "--quiet", "--bare", &format!("--initial-branch={branch}"), &p], crate::gitops::LS_REMOTE_TIMEOUT).await?;
        }
    }
    // Every ref a push brings is kept; nothing is pruned behind kiln's back. The mirror fetches
    // a diff's versions by their commits.
    for (k, v) in [("receive.denyNonFastForwards", "false"), ("gc.auto", "0"), ("uploadpack.allowAnySHA1InWant", "true"), ("receive.advertisePushOptions", "true")] {
        crate::gitops::run_git(hub, &["-C", &p, "config", k, v], crate::gitops::LS_REMOTE_TIMEOUT).await?;
    }
    Ok(path)
}

/// Pushes `trunk` to the repository's mirror, if it has one (W14-R01 `native + mirror`), in the
/// background; a refusal there is logged and audited, and the next land tries again.
pub fn mirror_trunk(hub: &Arc<Hub>, repo: &Repo, path: &str, trunk: &str) {
    let Some(url) = repo.mirror.clone() else { return };
    let (hub, name, path, trunk) = (hub.clone(), repo.name.clone(), path.to_owned(), trunk.to_owned());
    crate::rt::spawn(async move {
        let spec = format!("refs/heads/{trunk}:refs/heads/{trunk}");
        match crate::gitops::run_git(&hub, &["-C", &path, "push", "--quiet", "--", &url, &spec], std::time::Duration::from_secs(600)).await {
            Ok(_) => tracing::info!(repo = %name, %trunk, "mirrored the trunk upstream"),
            Err(e) => {
                tracing::warn!(repo = %name, %trunk, error = %format!("{e:#}"), "the trunk's mirror refused it");
                hub.store.audit("kiln".into(), "repo.mirror".into(), format!("{name} {trunk} not mirrored: {}", crate::util::truncate(&format!("{e:#}"), 300))).await;
            }
        }
    });
}

/// Makes `name`'s bare repository a copy of the bare repository at `from` (a template kiln
/// hosts, docs/deploy-api.md §3.1).
pub async fn create_from(hub: &Hub, name: &str, from: &Path) -> anyhow::Result<PathBuf> {
    let path = path_of(hub, name).ok_or_else(|| anyhow::anyhow!("`{name}`: a repository is `owner/name`"))?;
    anyhow::ensure!(!path.join("HEAD").is_file(), "{name} is there already");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let (f, p) = (from.to_string_lossy().into_owned(), path.to_string_lossy().into_owned());
    crate::gitops::run_git(hub, &["clone", "--quiet", "--bare", "--no-local", "--", &f, &p], std::time::Duration::from_secs(1800)).await?;
    for (k, v) in [("receive.denyNonFastForwards", "false"), ("gc.auto", "0"), ("uploadpack.allowAnySHA1InWant", "true"), ("receive.advertisePushOptions", "true")] {
        crate::gitops::run_git(hub, &["-C", &p, "config", k, v], crate::gitops::LS_REMOTE_TIMEOUT).await?;
    }
    Ok(path)
}

/// A ref update a push asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Update {
    pub old: String,
    pub new: String,
    pub name: String,
}

const ZERO: &str = "0000000000000000000000000000000000000000";

impl Update {
    pub fn creates(&self) -> bool {
        self.old.chars().all(|c| c == '0')
    }

    pub fn deletes(&self) -> bool {
        self.new.chars().all(|c| c == '0')
    }
}

/// The updates a receive-pack request starts with, the client's capabilities, and how many bytes
/// they took (the pack follows); `None` until the whole command list (to its flush) is there.
pub fn commands(body: &[u8]) -> Result<Option<(Vec<Update>, Vec<String>, usize)>, String> {
    let (mut at, mut updates, mut caps) = (0usize, Vec::new(), Vec::new());
    loop {
        let Some(len) = body.get(at..at + 4) else { return Ok(None) };
        let len = std::str::from_utf8(len).ok().and_then(|h| usize::from_str_radix(h, 16).ok()).ok_or("a malformed pkt-line")?;
        if len == 0 {
            return Ok(Some((updates, caps, at + 4)));
        }
        if len < 4 {
            return Err("a malformed pkt-line".into());
        }
        let Some(line) = body.get(at + 4..at + len) else { return Ok(None) };
        at += len;
        let line = String::from_utf8_lossy(line);
        let line = line.trim_end_matches('\n');
        let (cmd, rest) = line.split_once('\0').unwrap_or((line, ""));
        if !rest.is_empty() {
            caps = rest.split(' ').filter(|c| !c.is_empty()).map(str::to_owned).collect();
        }
        if cmd.starts_with("shallow ") {
            continue;
        }
        if cmd.starts_with("push-cert") {
            return Err("signed pushes aren't taken".into());
        }
        let mut parts = cmd.split(' ');
        let (Some(old), Some(new), Some(name)) = (parts.next(), parts.next(), parts.next()) else { return Err(format!("a ref update that doesn't read: {cmd}")) };
        updates.push(Update { old: old.to_owned(), new: new.to_owned(), name: name.to_owned() });
    }
}

/// The push options that follow a receive-pack request's commands (when the client sends them,
/// `push-options`): pkt-lines to a flush, and how many bytes they took; `None` until all there.
pub fn push_options(body: &[u8]) -> Result<Option<(Vec<String>, usize)>, String> {
    let (mut at, mut out) = (0usize, Vec::new());
    loop {
        let Some(len) = body.get(at..at + 4) else { return Ok(None) };
        let len = std::str::from_utf8(len).ok().and_then(|h| usize::from_str_radix(h, 16).ok()).ok_or("a malformed pkt-line")?;
        if len == 0 {
            return Ok(Some((out, at + 4)));
        }
        if len < 4 {
            return Err("a malformed pkt-line".into());
        }
        let Some(line) = body.get(at + 4..at + len) else { return Ok(None) };
        out.push(String::from_utf8_lossy(line).trim_end_matches('\n').to_owned());
        at += len;
    }
}

fn pkt(payload: &[u8]) -> Vec<u8> {
    let mut out = format!("{:04x}", payload.len() + 4).into_bytes();
    out.extend_from_slice(payload);
    out
}

/// The payloads of `buf`'s pkt-lines, `None` for a flush (or a delimiter); to the end, or to
/// framing that doesn't read.
fn pkt_lines(buf: &[u8]) -> Vec<Option<&[u8]>> {
    let (mut out, mut at) = (Vec::new(), 0usize);
    while let Some(h) = buf.get(at..at + 4) {
        let Some(len) = std::str::from_utf8(h).ok().and_then(|h| usize::from_str_radix(h, 16).ok()) else { break };
        if len < 4 {
            out.push(None);
            at += 4;
            continue;
        }
        let Some(p) = buf.get(at + 4..at + len) else { break };
        out.push(Some(p));
        at += len;
    }
    out
}

/// The most data a side-band packet carries for the client's capabilities (`side-band-64k`,
/// `side-band`); `None`: the client takes the report alone.
pub fn band_of(caps: &[String]) -> Option<usize> {
    if caps.iter().any(|c| c == "side-band-64k") {
        Some(65_515)
    } else if caps.iter().any(|c| c == "side-band") {
        Some(995)
    } else {
        None
    }
}

/// A receive-pack answer, read: what it said along the way (its band 2 and 3 packets, as they
/// came), and its report's lines (`unpack ok`, `ok <ref>`, `ng <ref> <why>`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub said: Vec<Vec<u8>>,
    pub lines: Vec<String>,
}

pub fn read_report(body: &[u8], banded: bool) -> Report {
    let mut r = Report::default();
    let inner: Vec<u8> = if banded {
        let mut inner = Vec::new();
        for p in pkt_lines(body).into_iter().flatten() {
            match p.first() {
                Some(1) => inner.extend_from_slice(p.get(1..).unwrap_or_default()),
                Some(2 | 3) => r.said.push(p.to_vec()),
                _ => {}
            }
        }
        inner
    } else {
        body.to_vec()
    };
    for p in pkt_lines(&inner) {
        match p {
            Some(l) => r.lines.push(String::from_utf8_lossy(l).trim_end_matches('\n').to_owned()),
            None => break,
        }
    }
    r
}

/// `r` as the client reads it, `extra` said after what git said (as `kiln: …` lines), in the
/// bands it asked for (`band`, from [`band_of`]).
pub fn write_report(r: &Report, extra: &[String], band: Option<usize>) -> Vec<u8> {
    let mut report = Vec::new();
    for l in &r.lines {
        report.extend(pkt(format!("{l}\n").as_bytes()));
    }
    report.extend_from_slice(b"0000");
    let Some(max) = band else { return report };
    let mut out = Vec::new();
    for s in &r.said {
        out.extend(pkt(s));
    }
    for l in extra {
        let line = format!("\x02kiln: {l}\n");
        out.extend(pkt(line.as_bytes().get(..max + 1).unwrap_or(line.as_bytes())));
    }
    for chunk in report.chunks(max) {
        let mut b = vec![1u8];
        b.extend_from_slice(chunk);
        out.extend(pkt(&b));
    }
    out.extend_from_slice(b"0000");
    out
}

/// A receive-pack answer refusing `updates`, `why` each (and said once to the person pushing),
/// in the bands the client asked for.
pub fn refusal(updates: &[(Update, String)], caps: &[String], message: &str) -> Vec<u8> {
    let mut lines = vec!["unpack ok".to_owned()];
    lines.extend(updates.iter().map(|(u, why)| format!("ng {} {why}", u.name)));
    let said: Vec<String> = message.lines().map(str::to_owned).collect();
    write_report(&Report { said: Vec::new(), lines }, &said, band_of(caps))
}

/// `refs/for/<trunk>[%opt,opt=v]`: the trunk, and Gerrit's options after `%`.
pub fn for_ref(name: &str) -> Option<(&str, Vec<String>)> {
    let rest = name.strip_prefix("refs/for/")?;
    let (trunk, opts) = rest.split_once('%').unwrap_or((rest, ""));
    Some((trunk, opts.split(',').map(str::trim).filter(|o| !o.is_empty()).map(str::to_owned).collect()))
}

/// `.kiln/review.yaml` as a push reads it (W14-R09): the trunks, and who may push to them.
pub use kiln_ci::land::Policy as Review;

/// Who is pushing or fetching.
pub enum Pusher {
    Caller(Caller),
    /// A leg's `GITHUB_TOKEN`: its run's repository and permissions.
    Job(super::token::Job),
}

impl Pusher {
    fn name(&self) -> String {
        match self {
            Pusher::Caller(c) => c.name.clone(),
            Pusher::Job(j) => format!("job:{}", j.leg),
        }
    }

    /// Whether a `direct-push` entry names it: as the audit does (`user:ada`, `agent:review`), or
    /// by the user it is or acts for (`ada`).
    fn named(&self, entry: &str) -> bool {
        let entry = entry.trim();
        let name = self.name();
        // `user:<name>`, `pat:<id> of user:<name>`, `<agent> for user:<name>`.
        let user = name.rsplit(' ').next().and_then(|w| w.strip_prefix("user:")).filter(|_| matches!(self, Pusher::Caller(_)));
        let agent = name.split(' ').next().filter(|_| matches!(self, Pusher::Caller(c) if c.kind == crate::authz::gate::Kind::Agent));
        entry.eq_ignore_ascii_case(&name)
            || user.is_some_and(|u| entry.eq_ignore_ascii_case(u) || entry.strip_prefix("user:").is_some_and(|e| e.eq_ignore_ascii_case(u)))
            || agent.is_some_and(|a| entry.eq_ignore_ascii_case(a))
    }
}

/// Why `pusher` may not make `u` on `repo` (whose trunks are `trunks`; `empty`: it has no
/// branch yet); `None` when it may. (A protected branch's rewrites are git's to refuse, once it
/// has the pushed commits: see [`serve`].)
pub fn refused(pusher: &Pusher, repo: &str, review: &Review, trunks: &[String], u: &Update, empty: bool) -> Option<String> {
    let name = &u.name;
    let may = |cap: &str, level: kiln_ci::plan::perms::Level| match pusher {
        Pusher::Caller(c) => c.is_owner() || c.can(cap, &Scope::Repo { repo: repo.to_owned() }),
        Pusher::Job(j) => j.may("contents", level),
    };
    let write = kiln_ci::plan::perms::Level::Write;
    if name.starts_with("refs/kiln/") || name.starts_with("refs/pull/") {
        return Some("kiln's own refs aren't pushed".into());
    }
    if let Some((target, _)) = for_ref(name) {
        if u.deletes() {
            return Some("refs/for/<trunk> takes commits to review; there is nothing to delete".into());
        }
        let (trunk, branch) = target.split_once("/branch/").map_or((target, None), |(t, b)| (t, Some(b)));
        if !trunks.iter().any(|t| t == trunk) {
            return Some(format!("{trunk} isn't a trunk: submit to refs/for/<trunk> ({})", trunks.join(", ")));
        }
        if let Some(b) = branch
            && let Err(e) = super::cr::branch_ok(b, trunks)
        {
            return Some(e);
        }
        return (!may("diffs.submit", write)).then(|| "submitting diffs needs `diffs.submit` (a job's token: `contents: write`)".to_owned());
    }
    if let Some(branch) = name.strip_prefix("refs/heads/") {
        // A trunk in another case is the trunk itself where the disk ignores case (macOS, Windows).
        if let Some(t) = trunks.iter().find(|t| t.eq_ignore_ascii_case(branch) && *t != branch) {
            return Some(format!("{branch} is the trunk {t} in another case"));
        }
        if trunks.iter().any(|t| t == branch) {
            if u.deletes() {
                return Some(format!("{branch} is a trunk: it isn't deleted"));
            }
            let direct = matches!(pusher, Pusher::Caller(c) if c.is_owner()) || review.direct_push.iter().any(|p| pusher.named(p));
            // An empty repository takes its first trunk from anyone who may push.
            let first = u.creates() && empty && may("diffs.submit", write);
            return (!direct && !first).then(|| format!("{branch} is a trunk: only the land queue writes it (or `direct-push` in .kiln/review.yaml)"));
        }
        if review.protects(branch) {
            if u.deletes() {
                return Some(format!("{branch} is protected: it isn't deleted"));
            }
            return (!may("diffs.land", write)).then(|| format!("{branch} is protected: pushing it needs `diffs.land` (a job's token: `contents: write`)"));
        }
        return (!may("diffs.submit", write)).then(|| "pushing branches needs `diffs.submit` (a job's token: `contents: write`)".to_owned());
    }
    if name.starts_with("refs/tags/") {
        return (!may("diffs.land", write)).then(|| "pushing tags needs `diffs.land` (a job's token: `contents: write`)".to_owned());
    }
    Some(format!("{name}: only branches and tags are pushed"))
}

fn unauthorized() -> Response {
    let r = srv::text(StatusCode::UNAUTHORIZED, "kiln needs a token: a personal or agent token as the password, or a job's GITHUB_TOKEN");
    srv::with_header(r, http::header::WWW_AUTHENTICATE, "Basic realm=\"kiln\"")
}

/// Who a request is, from its token.
async fn pusher(hub: &Hub, headers: &http::HeaderMap) -> Option<Pusher> {
    let token = super::token::from_headers(headers)?;
    if token.starts_with(super::token::JOB_PREFIX) {
        return super::token::job_of(hub, &token).await.map(Pusher::Job);
    }
    if crate::util::ct_eq(token.as_bytes(), hub.admin_token.as_bytes()) {
        return Some(Pusher::Caller(Caller::owner()));
    }
    crate::authz::gate::caller(hub, Some(&token), None).await.map(Pusher::Caller)
}

/// The trunks of a native repository: `.kiln/review.yaml`'s, or its default branch.
async fn trunks(hub: &Hub, path: &str) -> (Vec<String>, Review) {
    let head = crate::gitops::run_git(hub, &["-C", path, "symbolic-ref", "--short", "HEAD"], crate::gitops::LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).trim().to_owned()).unwrap_or_else(|_| "main".into());
    let review: Review = crate::gitops::run_git(hub, &["-C", path, "show", &format!("refs/heads/{head}:.kiln/review.yaml")], crate::gitops::LS_REMOTE_TIMEOUT)
        .await
        .ok()
        .and_then(|b| Review::parse(&String::from_utf8_lossy(&b)).ok())
        .unwrap_or_default();
    let mut list = review.trunks.clone();
    if !list.contains(&head) {
        list.push(head);
    }
    (list, review)
}

/// Reads what's left of a refused push and drops it: at most 4 GiB, for ten minutes, and nothing
/// once the client pauses for a minute.
async fn drain(body: &mut kiln_http::Incoming) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(600);
    let mut read = 0u64;
    while read < 4 << 30 && std::time::Instant::now() < deadline {
        match crate::rt::timeout(std::time::Duration::from_secs(60), srv::next_chunk(body)).await {
            Ok(Ok(Some(chunk))) => read += chunk.len() as u64,
            _ => break,
        }
    }
}

/// More of a push's start: its next chunk, into `head`.
async fn more(head: &mut Vec<u8>, body: &mut kiln_http::Incoming) -> Result<(), Fail> {
    if head.len() >= 1 << 20 {
        return Err((StatusCode::PAYLOAD_TOO_LARGE, "too many ref updates in one push".into()));
    }
    match srv::next_chunk(body).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))? {
        Some(chunk) => {
            head.extend_from_slice(&chunk);
            Ok(())
        }
        None => Err((StatusCode::BAD_REQUEST, "the push ended before its ref updates did".into())),
    }
}

/// One push at a time per repository takes `refs/for/*` (the ref is the push's way in, cleared
/// before and after).
fn lock_of(repo: &str) -> Arc<async_lock::Mutex<()>> {
    static LOCKS: std::sync::OnceLock<parking_lot::Mutex<HashMap<String, Arc<async_lock::Mutex<()>>>>> = std::sync::OnceLock::new();
    LOCKS.get_or_init(Default::default).lock().entry(repo.to_owned()).or_default().clone()
}

/// Removes `refs/for/*` left by a push that didn't finish.
async fn clear_for_refs(hub: &Hub, path: &str) {
    let Ok(out) = crate::gitops::run_git(hub, &["-C", path, "for-each-ref", "--format=%(refname)", "refs/for/"], crate::gitops::LS_REMOTE_TIMEOUT).await else { return };
    for r in String::from_utf8_lossy(&out).lines().filter(|l| !l.is_empty()) {
        let _ = crate::gitops::run_git(hub, &["-C", path, "update-ref", "-d", r], crate::gitops::LS_REMOTE_TIMEOUT).await;
    }
}

/// `/<owner>/<name>.git/info/refs` and `…/git-upload-pack`, `…/git-receive-pack` of a native
/// repository.
pub async fn serve(hub: &Arc<Hub>, repo: &Repo, path: &Path, tail: &[&str], req: Request) -> Result<Response, Fail> {
    let q = srv::query(&req);
    let pushing = matches!(tail, ["git-receive-pack"]) || (matches!(tail, ["info", "refs"]) && q.get("service").map(String::as_str) == Some("git-receive-pack"));
    let who = match pusher(hub, req.headers()).await {
        Some(w) => Some(w),
        // A public repository is anyone's to read, without a token, as a public GitHub
        // repository is (a step that fetches after `actions/checkout`'s `persist-credentials:
        // false`). A token that isn't kiln's is refused all the same, and a push needs one.
        None if !pushing && super::token::from_headers(req.headers()).is_none() && hub.store.repo_public(repo.id.clone()).await.unwrap_or(false) => None,
        None => return Ok(unauthorized()),
    };
    if let Some(who) = &who {
        let read = match who {
            // `git.read`: a reader's token (Argo CD's, docs/deploy-api.md §7), git and nothing else.
            Pusher::Caller(c) => c.is_owner() || c.can("diffs.read", &Scope::Repo { repo: repo.name.clone() }) || c.can("git.read", &Scope::Repo { repo: repo.name.clone() }),
            Pusher::Job(j) => j.repo == repo.id && j.may("contents", kiln_ci::plan::perms::Level::Read),
        };
        if !read {
            return Err((StatusCode::FORBIDDEN, format!("{} may not read {}", who.name(), repo.name)));
        }
        if pushing && matches!(who, Pusher::Job(j) if j.repo != repo.id) {
            return Err((StatusCode::FORBIDDEN, format!("this job's token pushes to its own repository only, not {}", repo.name)));
        }
    }
    let (Some(root), Some(dir)) = (path.parent(), path.file_name()) else { return Err((StatusCode::INTERNAL_SERVER_ERROR, "the repository's path".into())) };
    let path_info = format!("/{}/{}", dir.to_string_lossy(), tail.join("/"));
    let user = who.as_ref().map_or_else(|| "anonymous".to_owned(), Pusher::name);
    let configs: &[&str] = if pushing { &["http.receivepack=true", "receive.advertisePushOptions=true"] } else { &["uploadpack.allowAnySHA1InWant=true", "uploadpack.allowFilter=true"] };
    let cgi = super::facade::Cgi { root, path_info: &path_info, configs, user: &user, method: req.method().as_str().to_owned(), query: req.uri().query().unwrap_or_default().to_owned(), headers: req.headers().clone() };
    let p = path.to_string_lossy().into_owned();
    if !matches!(tail, ["git-receive-pack"]) {
        let body = srv::body_bytes(req.into_body(), 64 << 20).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
        if !pushing {
            return super::facade::backend(&cgi, body.to_vec(), None).await;
        }
        // A push's advertisement never shows a `refs/for/*` (a client would take it for a branch).
        let lock = lock_of(&repo.id);
        let _held = lock.lock().await;
        clear_for_refs(hub, &p).await;
        return super::facade::backend(&cgi, body.to_vec(), None).await;
    }
    let Some(who) = who else { return Ok(unauthorized()) };
    if req.headers().get(http::header::CONTENT_ENCODING).is_some_and(|e| e.as_bytes() != b"identity") {
        return Err((StatusCode::UNSUPPORTED_MEDIA_TYPE, "a push's request body isn't compressed".into()));
    }
    // The ref updates, read before git sees any of the push.
    let mut body = req.into_body();
    let mut head: Vec<u8> = Vec::new();
    let (updates, caps, used) = loop {
        match commands(&head) {
            Ok(Some(found)) => break found,
            Ok(None) => more(&mut head, &mut body).await?,
            Err(e) => return Err((StatusCode::BAD_REQUEST, e)),
        }
    };
    let (trunk_list, review) = trunks(hub, &p).await;
    let branches = crate::gitops::run_git(hub, &["-C", &p, "for-each-ref", "--count=1", "--format=%(refname)", "refs/heads/"], crate::gitops::LS_REMOTE_TIMEOUT).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    let empty = branches.iter().all(u8::is_ascii_whitespace);
    let mut no: Vec<(Update, String)> = updates.iter().filter_map(|u| refused(&who, &repo.name, &review, &trunk_list, u, empty).map(|why| (u.clone(), why))).collect();
    // A protected branch isn't rewritten: git refuses this push's non-fast-forwards once it has
    // the commits (before, they aren't there to compare).
    let guarded: Vec<String> = updates.iter().filter(|u| !u.creates() && !u.deletes()).filter_map(|u| u.name.strip_prefix("refs/heads/").filter(|b| review.protects(b)).map(str::to_owned)).collect();
    let submitted: Vec<&Update> = updates.iter().filter(|u| u.name.starts_with("refs/for/")).collect();
    if submitted.len() > 1 {
        no.extend(submitted.iter().skip(1).map(|u| ((*u).clone(), "one refs/for/<trunk> a push".to_owned())));
    }
    if !no.is_empty() {
        // Refused whole: what was allowed waits for the rest.
        let mut all: Vec<(Update, String)> = no.clone();
        for u in updates.iter().filter(|u| !no.iter().any(|(r, _)| r.name == u.name)) {
            all.push((u.clone(), "push refused: another of its updates was".into()));
        }
        let message = no.iter().map(|(u, why)| format!("{}: {why}", u.name)).collect::<Vec<_>>().join("\n");
        hub.store.audit(user.clone(), "repo.push".into(), format!("{} refused: {}", repo.name, message.replace('\n', "; "))).await;
        // The rest of the push (its pack) is read and dropped first: answered while it still
        // sends, git sees the connection close, not why.
        drain(&mut body).await;
        return Ok(srv::response(StatusCode::OK, "application/x-git-receive-pack-result", srv::full(bytes::Bytes::from(refusal(&all, &caps, &message)))));
    }
    let target = submitted.first().map(|u| (*u).clone());
    // The push's options follow its commands.
    let mut given: Vec<String> = Vec::new();
    if caps.iter().any(|c| c == "push-options") {
        loop {
            match push_options(head.get(used..).unwrap_or_default()) {
                Ok(Some((o, _))) => {
                    given = o;
                    break;
                }
                Ok(None) => more(&mut head, &mut body).await?,
                Err(e) => return Err((StatusCode::BAD_REQUEST, e)),
            }
        }
    }
    // One submission at a time per repository (its `refs/for/*` is the way in).
    let lock = lock_of(&repo.id);
    let _held = match target {
        Some(_) => {
            let g = lock.lock().await;
            clear_for_refs(hub, &p).await;
            Some(g)
        }
        None => None,
    };
    // Who pushes to each branch and tag (W10-R02's `secrets.use`), kept before git takes it: the
    // poller makes the push's events, and their runs are given only the secrets the pusher may
    // use. A push kiln can't keep this for is refused.
    let started_by = match &who {
        Pusher::Caller(c) => c.started_by(),
        Pusher::Job(_) => None,
    };
    keep_pushers(hub, repo, &updates, started_by.as_ref()).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("who pushed couldn't be kept, so the push isn't taken: {e:#}")))?;
    // git takes the push; its answer (a report, and its progress) is read before it's passed on,
    // so kiln can say what the push made.
    let mut configs: Vec<&str> = cgi.configs.to_vec();
    if !guarded.is_empty() {
        configs.push("receive.denyNonFastForwards=true");
    }
    let cgi = super::facade::Cgi { configs: &configs, ..cgi };
    let answer = super::facade::run(&cgi, head, Some(body)).await?;
    if answer.status != StatusCode::OK {
        let status = answer.status;
        let ctype = answer.content_type.clone();
        let text = answer.bytes(1 << 20).await.unwrap_or_default();
        return Ok(srv::response(status, &ctype, srv::full(bytes::Bytes::from(text))));
    }
    let ctype = answer.content_type.clone();
    let raw = answer.bytes(16 << 20).await?;
    let band = band_of(&caps);
    let mut report = read_report(&raw, band.is_some());
    let mut said: Vec<String> = Vec::new();
    let author = match &who {
        Pusher::Caller(c) => super::diffs::author_of(&c.name, Some(c.kind)),
        Pusher::Job(j) => super::diffs::author_of(&format!("job:{}", j.leg), None),
    };
    for b in &guarded {
        if report.lines.iter().any(|l| l.starts_with(&format!("ng refs/heads/{b} ")) && l.contains("fast-forward")) {
            said.push(format!("{b} is protected: it isn't rewritten (a push that isn't a fast-forward)"));
        }
    }
    // What git took (`ok <ref>`), before kiln says more.
    let took: HashSet<String> = report.lines.iter().filter_map(|l| l.strip_prefix("ok ")).map(|r| r.split_whitespace().next().unwrap_or(r).to_owned()).collect();
    // The first branch pushed to an empty repository is its default, as on GitHub, when the one
    // it was made on (its HEAD) isn't among those pushed.
    if empty
        && let Some(first) = updates.iter().filter(|u| u.creates() && took.contains(&u.name)).find_map(|u| u.name.strip_prefix("refs/heads/"))
        && trunk_list.last().is_some_and(|head| !updates.iter().any(|u| u.name == format!("refs/heads/{head}")))
        && crate::gitops::run_git(hub, &["-C", &p, "symbolic-ref", "HEAD", &format!("refs/heads/{first}")], crate::gitops::LS_REMOTE_TIMEOUT).await.is_ok()
    {
        let (id, branch) = (repo.id.clone(), first.to_owned());
        if let Err(e) = hub.store.call(move |c| Ok(c.execute("UPDATE repos SET default_branch = ?2 WHERE id = ?1", rusqlite::params![id, branch])?)).await {
            tracing::warn!(repo = %repo.name, error = %format!("{e:#}"), "the default branch could not be recorded");
        }
        said.push(format!("{first} is the repository's default branch: the first pushed"));
    }
    if let Some(target) = &target {
        let at = report.lines.iter().position(|l| *l == format!("ok {}", target.name));
        if let (Some(at), Some((spec, suffix))) = (at, for_ref(&target.name)) {
            given.extend(suffix);
            let options = kiln_ci::review::options(&given);
            said.extend(options.unknown.iter().map(|o| format!("push option `{o}` isn't one kiln takes (draft, ready, reviewer=, message=, topic=, title=); ignored")));
            let result = match spec.split_once("/branch/") {
                // A change request (W14-R04): the branch is the commit pushed, then reviewed whole.
                Some((trunk, branch)) => match crate::gitops::run_git(hub, &["-C", &p, "update-ref", &format!("refs/heads/{branch}"), &target.new], crate::gitops::LS_REMOTE_TIMEOUT).await {
                    Ok(_) => {
                        let by = super::cr::By { pushed_by: user.clone(), author: author.clone(), started_by: started_by.clone() };
                        super::cr::submit(hub, repo, &p, trunk, branch, &target.new, &by, &options).await.map(|o| vec![o])
                    }
                    Err(e) => Err(format!("the branch {branch}: {e:#}")),
                },
                None => super::diffs::submit(hub, super::diffs::Submission { repo, path: &p, trunk: spec, sha: &target.new, pushed_by: &user, started_by: started_by.clone(), author: author.clone(), options }).await,
            };
            match result {
                Ok(outcomes) => {
                    for o in outcomes {
                        let what = match o.kind {
                            "new" => "new",
                            "rebased" => "rebased, nothing else changed",
                            "reordered" => "moved in the stack",
                            "unchanged" => "unchanged",
                            _ => "updated",
                        };
                        said.push(format!("{} v{} ({what})  {}", o.name, o.version, o.title));
                        said.push(format!("  {}", o.url));
                    }
                }
                Err(why) => {
                    if let Some(l) = report.lines.get_mut(at) {
                        *l = format!("ng {} {}", target.name, why.replace('\n', "; "));
                    }
                    said.push(why);
                }
            }
        }
        // The ref was only the push's way in.
        let _ = crate::gitops::run_git(hub, &["-C", &p, "update-ref", "-d", &target.name], crate::gitops::LS_REMOTE_TIMEOUT).await;
    }
    let by = super::cr::By { pushed_by: user, author, started_by };
    said.extend(after_push(hub, repo, &p, &by, &trunk_list, &updates, &took).await);
    Ok(srv::response(StatusCode::OK, &ctype, srv::full(bytes::Bytes::from(write_report(&report, &said, band)))))
}

/// Who moves each branch and tag (W10-R02's `secrets.use`), kept before the refs move: the poller
/// makes the push's events, and their runs are given only the secrets the pusher may use.
async fn keep_pushers(hub: &Hub, repo: &Repo, updates: &[Update], started_by: Option<&serde_json::Value>) -> anyhow::Result<()> {
    let moved: Vec<(String, String)> = updates.iter().filter(|u| u.name.starts_with("refs/heads/") || u.name.starts_with("refs/tags/")).map(|u| (u.name.clone(), u.new.clone())).collect();
    if moved.is_empty() {
        return Ok(());
    }
    let (id, by) = (repo.id.clone(), started_by.map(serde_json::Value::to_string));
    hub.store
        .call(move |c| {
            let now = crate::util::now_unix();
            let tx = c.transaction()?;
            for (r, sha) in moved {
                tx.execute("DELETE FROM ci_pushes WHERE repo_id = ?1 AND ref = ?2", rusqlite::params![id, r])?;
                // Whoever may use every secret leaves nothing to hold its runs to.
                if let Some(b) = &by
                    && sha != ZERO
                {
                    tx.execute("INSERT OR REPLACE INTO ci_pushes (repo_id, ref, sha, started_by, ts) VALUES (?1, ?2, ?3, ?4, ?5)", rusqlite::params![id, r, sha, b, now])?;
                }
            }
            tx.execute("DELETE FROM ci_pushes WHERE ts < ?1", [now - 30 * 86_400])?;
            tx.commit()?;
            Ok(())
        })
        .await
}

/// What follows the ref updates git took (`took`, of `updates`): what it says of them.
async fn after_push(hub: &Arc<Hub>, repo: &Repo, p: &str, by: &super::cr::By, trunk_list: &[String], updates: &[Update], took: &HashSet<String>) -> Vec<String> {
    let mut said = Vec::new();
    // Branches pushed (not trunks): their open change requests get the new heads as versions.
    let branches: Vec<(String, String)> = updates
        .iter()
        .filter(|u| !u.deletes() && took.contains(&u.name))
        .filter_map(|u| u.name.strip_prefix("refs/heads/").map(|b| (b.to_owned(), u.new.clone())))
        .filter(|(b, _)| !trunk_list.iter().any(|t| t == b))
        .collect();
    if !branches.is_empty() {
        said.extend(super::cr::pushed(hub, repo, p, &branches, by).await);
    }
    // A trunk written directly goes to the mirror too.
    for t in trunk_list.iter().filter(|t| took.contains(&format!("refs/heads/{t}"))) {
        mirror_trunk(hub, repo, p, t);
    }
    // What a job pushes with its token starts no run, as GitHub starts none for what a
    // `GITHUB_TOKEN` does (but `workflow_dispatch` and `repository_dispatch`): a job doesn't start
    // itself again. Its moves are recorded first, so the poller finds them taken.
    if super::diffs::by_job(&by.pushed_by) {
        for u in updates.iter().filter(|u| !u.deletes() && took.contains(&u.name) && !u.name.starts_with("refs/for/")) {
            let _ = hub.store.advance_ref(repo.id.clone(), u.name.clone(), u.new.clone()).await;
        }
    }
    let summary = updates.iter().map(|u| format!("{} {}..{}", u.name, short(&u.old), short(&u.new))).collect::<Vec<_>>().join(", ");
    hub.store.audit(by.pushed_by.clone(), "repo.push".into(), format!("{}: {summary}", repo.name)).await;
    // The poller takes the new refs now: events, runs.
    if updates.iter().any(|u| !u.name.starts_with("refs/for/")) {
        hub.gitops_kick.notify();
    }
    said
}

/// A commit of `file` to `branch` of a repository kiln hosts, made through the API (a workflow
/// saved from its editor, W21-R06) as a push of it would be: on the branch's head (a new branch on
/// the default branch's), refused for what a push is refused for, its pusher kept for its runs'
/// secrets, and what follows a push. The commit, and what kiln says of it.
pub(crate) async fn commit_branch(hub: &Arc<Hub>, repo: &Repo, caller: &Caller, branch: &str, file: &str, content: Vec<u8>, message: &str) -> Result<(String, Vec<String>), Fail> {
    let path = native_path(hub, repo).ok_or((StatusCode::CONFLICT, format!("{} isn't hosted here", repo.name)))?;
    let p = path.to_string_lossy().into_owned();
    let git = |args: Vec<String>| {
        let p = p.clone();
        async move {
            let mut a: Vec<&str> = vec!["-C", &p];
            a.extend(args.iter().map(String::as_str));
            crate::gitops::run_git(hub, &a, crate::gitops::LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).trim().to_owned())
        }
    };
    if branch.starts_with('-') || git(vec!["check-ref-format".into(), format!("refs/heads/{branch}")]).await.is_err() {
        return Err((StatusCode::BAD_REQUEST, format!("`{branch}` isn't a branch name")));
    }
    let (trunk_list, review) = trunks(hub, &p).await;
    let head = git(vec!["rev-parse".into(), "--verify".into(), "--quiet".into(), format!("refs/heads/{branch}^{{commit}}")]).await.ok().filter(|s| s.len() == 40);
    let default = git(vec!["symbolic-ref".into(), "--short".into(), "HEAD".into()]).await.unwrap_or_else(|_| "main".into());
    let base = match &head {
        Some(h) => h.clone(),
        None => git(vec!["rev-parse".into(), "--verify".into(), format!("refs/heads/{default}^{{commit}}")]).await.map_err(|_| (StatusCode::UNPROCESSABLE_ENTITY, format!("{default} has no commits yet")))?,
    };
    let mode = git(vec!["ls-tree".into(), base.clone(), "--".into(), file.to_owned()]).await.ok().and_then(|l| l.split_whitespace().next().map(str::to_owned)).unwrap_or_else(|| "100644".into());
    let tree = super::diffs::tree_with(hub, &p, &base, super::diffs::Edit::Put { file, mode: &mode, content }).await.map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
    if git(vec!["rev-parse".into(), format!("{base}^{{tree}}")]).await.is_ok_and(|t| t == tree) {
        return Err((StatusCode::CONFLICT, format!("{file} is already so on {branch}: nothing to commit")));
    }
    let author = super::diffs::author_of(&caller.name, Some(caller.kind));
    let me = author.0.strip_prefix("user:").unwrap_or(&author.0).to_owned();
    let sha = super::diffs::make_commit(hub, &p, &tree, &base, (&me, None), &me, &format!("{}\n", message.trim_end())).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let u = Update { old: head.clone().unwrap_or_else(|| ZERO.to_owned()), new: sha.clone(), name: format!("refs/heads/{branch}") };
    let who = Pusher::Caller(caller.clone());
    // Not an empty repository: the commit is on one of its own.
    if let Some(why) = refused(&who, &repo.name, &review, &trunk_list, &u, false) {
        hub.store.audit(who.name(), "repo.push".into(), format!("{} refused: {}: {why}", repo.name, u.name)).await;
        return Err((StatusCode::FORBIDDEN, format!("{branch}: {why}")));
    }
    let started_by = caller.started_by();
    keep_pushers(hub, repo, std::slice::from_ref(&u), started_by.as_ref())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("who committed couldn't be kept, so the commit isn't taken: {e:#}")))?;
    if git(vec!["update-ref".into(), u.name.clone(), sha.clone(), u.old.clone()]).await.is_err() {
        return Err((StatusCode::CONFLICT, format!("{branch} moved while this was saved: reload it and save again")));
    }
    let took: HashSet<String> = [u.name.clone()].into();
    let by = super::cr::By { pushed_by: who.name(), author, started_by };
    let said = after_push(hub, repo, &p, &by, &trunk_list, std::slice::from_ref(&u), &took).await;
    Ok((sha, said))
}

fn short(oid: &str) -> &str {
    if oid == ZERO { "0" } else { oid.get(..12).unwrap_or(oid) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(s: &str) -> Vec<u8> {
        pkt(s.as_bytes())
    }

    #[test]
    fn w14_r02_commands() -> Result<(), String> {
        let (a, b) = ("1".repeat(40), "2".repeat(40));
        let mut body = line(&format!("{ZERO} {a} refs/heads/main\0report-status side-band-64k agent=git/2.47\n"));
        body.extend(line(&format!("{a} {b} refs/heads/topic\n")));
        body.extend_from_slice(b"0000PACK...");
        let (updates, caps, used) = commands(&body)?.ok_or("incomplete")?;
        assert_eq!(updates.len(), 2);
        assert!(updates.first().is_some_and(Update::creates) && updates.get(1).is_some_and(|u| u.name == "refs/heads/topic" && !u.creates()));
        assert!(caps.contains(&"side-band-64k".to_owned()));
        assert_eq!(body.get(used..), Some(&b"PACK..."[..]));
        assert_eq!(commands(body.get(..10).unwrap_or_default())?, None, "a part of it waits for the rest");
        assert!(commands(b"zzzz").is_err());
        Ok(())
    }

    #[test]
    fn w14_r02_policy() {
        let (a, b) = ("1".repeat(40), "2".repeat(40));
        let up = |name: &str, old: &str, new: &str| Update { old: old.into(), new: new.into(), name: name.into() };
        let trunks = vec!["main".to_owned()];
        let owner = Pusher::Caller(Caller::owner());
        let review = Review::default();
        // The owner: anything but kiln's own refs.
        assert!(refused(&owner, "acme/app", &review, &trunks, &up("refs/heads/main", &a, &b), false).is_none());
        assert!(refused(&owner, "acme/app", &review, &trunks, &up("refs/kiln/diffs/D1/v1", ZERO, &b), false).is_some());
        assert!(refused(&owner, "acme/app", &review, &trunks, &up("refs/heads/main", &a, ZERO), false).is_some(), "trunks aren't deleted");
        // Submitting diffs: to a trunk, not deleted, not a change request yet.
        assert!(refused(&owner, "acme/app", &review, &trunks, &up("refs/for/main", ZERO, &b), false).is_none());
        assert!(refused(&owner, "acme/app", &review, &trunks, &up("refs/for/main%topic=x,wip", ZERO, &b), false).is_none());
        assert!(refused(&owner, "acme/app", &review, &trunks, &up("refs/for/release", ZERO, &b), false).is_some_and(|w| w.contains("isn't a trunk")));
        assert!(refused(&owner, "acme/app", &review, &trunks, &up("refs/for/main", &b, ZERO), false).is_some());
        assert!(refused(&owner, "acme/app", &review, &trunks, &up("refs/for/main/branch/topic", ZERO, &b), false).is_none(), "a change request");
        assert!(refused(&owner, "acme/app", &review, &trunks, &up("refs/for/main/branch/main", ZERO, &b), false).is_some_and(|w| w.contains("is a trunk")));
        assert!(refused(&owner, "acme/app", &review, &trunks, &up("refs/for/main/branch/a..b", ZERO, &b), false).is_some());
        // Someone with no rights.
        let nobody = Pusher::Caller(Caller::with(crate::authz::gate::Kind::Pat, "pat:x of user:ada", Some(crate::authz::Rights::default()), None));
        assert!(refused(&nobody, "acme/app", &review, &trunks, &up("refs/heads/topic", ZERO, &b), false).is_some());
        assert!(refused(&nobody, "acme/app", &review, &trunks, &up("refs/heads/main", ZERO, &b), true).is_some(), "even the first trunk needs push rights");
        for entry in ["ada", "user:ada", "pat:x of user:ada"] {
            let review = Review { direct_push: vec![entry.into()], ..Review::default() };
            assert!(refused(&nobody, "acme/app", &review, &trunks, &up("refs/heads/main", &a, &b), false).is_none(), "direct-push names it as {entry}");
        }
        let review = Review { direct_push: vec!["bob".into()], ..Review::default() };
        assert!(refused(&nobody, "acme/app", &review, &trunks, &up("refs/heads/main", &a, &b), false).is_some());
        // A developer: branches, not trunks, not tags.
        let grant = crate::authz::Grant { what: "developer".into(), scope: Scope::Repo { repo: "acme/app".into() }, deny: false };
        let dev = Pusher::Caller(Caller::with(crate::authz::gate::Kind::User, "user:ada", Some(crate::authz::Rights { grants: vec![grant], ..Default::default() }), None));
        assert!(refused(&dev, "acme/app", &review, &trunks, &up("refs/heads/topic", ZERO, &b), false).is_none());
        assert!(refused(&dev, "acme/app", &review, &trunks, &up("refs/for/main", ZERO, &b), false).is_none());
        assert!(refused(&nobody, "acme/app", &review, &trunks, &up("refs/for/main", ZERO, &b), false).is_some_and(|w| w.contains("diffs.submit")));
        assert!(refused(&dev, "acme/app", &review, &trunks, &up("refs/heads/main", &a, &b), false).is_some());
        assert!(refused(&dev, "acme/app", &review, &trunks, &up("refs/heads/main", ZERO, &b), true).is_none(), "an empty repository's first trunk");
        assert!(refused(&dev, "acme/app", &review, &trunks, &up("refs/tags/v1", ZERO, &b), false).is_some());
        assert!(refused(&dev, "acme/other", &review, &trunks, &up("refs/heads/topic", ZERO, &b), false).is_some(), "another repository's grant");
        // A leg's token: `contents: write`.
        let job = |level| Pusher::Job(crate::ci::token::Job { leg: "L1".into(), run: "R1".into(), repo: "r1".into(), permissions: [("contents".to_owned(), level)].into() });
        assert!(refused(&job(kiln_ci::plan::perms::Level::Write), "acme/app", &review, &trunks, &up("refs/tags/v1", ZERO, &b), false).is_none());
        assert!(refused(&job(kiln_ci::plan::perms::Level::Read), "acme/app", &review, &trunks, &up("refs/heads/topic", ZERO, &b), false).is_some());
        assert!(refused(&job(kiln_ci::plan::perms::Level::Write), "acme/app", &review, &trunks, &up("refs/heads/main", &a, &b), false).is_some(), "a leg doesn't write trunk");
        // Only an empty repository's: a trunk `.kiln/review.yaml` names that isn't there yet is
        // still the land queue's.
        let two = vec!["main".to_owned(), "release".to_owned()];
        assert!(refused(&dev, "acme/app", &review, &two, &up("refs/heads/release", ZERO, &b), false).is_some());
        // Protected branches: not deleted, not rewritten, pushed by those who land.
        let guarded = Review { protected: vec!["release/*".into()], ..Review::default() };
        assert!(refused(&dev, "acme/app", &guarded, &trunks, &up("refs/heads/release/1", &a, &b), false).is_some_and(|w| w.contains("diffs.land")));
        assert!(refused(&owner, "acme/app", &guarded, &trunks, &up("refs/heads/release/1", &a, &b), false).is_none());
        assert!(refused(&owner, "acme/app", &guarded, &trunks, &up("refs/heads/release/1", &a, ZERO), false).is_some_and(|w| w.contains("deleted")));
        assert!(refused(&dev, "acme/app", &guarded, &trunks, &up("refs/heads/topic", &a, &b), false).is_none(), "others are anyone's");
        assert!(guarded.protects("release/1") && !guarded.protects("topic"));
        // Another case of a trunk's name is the trunk, where the disk ignores case.
        assert!(refused(&owner, "acme/app", &Review::default(), &trunks, &up("refs/heads/MAIN", &a, &b), false).is_some());
        assert!(refused(&dev, "acme/app", &review, &trunks, &up("refs/heads/Main", ZERO, &b), true).is_some());
        assert!(refused(&dev, "acme/app", &review, &trunks, &up("refs/HEADS/main", ZERO, &b), true).is_some());
    }

    #[test]
    fn w14_r01_imports() {
        for ok in ["https://github.com/acme/app.git", "ssh://git@host:2222/acme/app", "git@github.com:acme/app.git", "git://host/app"] {
            assert!(importable(ok).is_ok(), "{ok}");
        }
        for no in ["/etc", "file:///srv/other.git", "../other", "ext::sh -c touch% /tmp/x", "-uhack", "fd::3", "C:\\repos\\app"] {
            assert!(importable(no).is_err(), "{no}");
        }
    }

    #[test]
    fn w14_r03_reports() {
        assert_eq!(for_ref("refs/for/main%topic=x, wip"), Some(("main", vec!["topic=x".to_owned(), "wip".to_owned()])));
        assert_eq!(for_ref("refs/heads/main"), None);
        let mut opts = line("draft");
        opts.extend(line("reviewer=ada"));
        opts.extend_from_slice(b"0000PACK");
        assert_eq!(push_options(&opts), Ok(Some((vec!["draft".to_owned(), "reviewer=ada".to_owned()], opts.len() - 4))));
        assert_eq!(push_options(opts.get(..6).unwrap_or_default()), Ok(None));
        // git's answer, in side-band: progress, then the report in band 1.
        let mut inner = line("unpack ok\n");
        inner.extend(line("ok refs/for/main\n"));
        inner.extend_from_slice(b"0000");
        let mut banded = pkt(b"\x02Resolving deltas: 100%\n");
        let mut b1 = vec![1u8];
        b1.extend_from_slice(&inner);
        banded.extend(pkt(&b1));
        banded.extend_from_slice(b"0000");
        let mut r = read_report(&banded, true);
        assert_eq!(r.lines, ["unpack ok", "ok refs/for/main"]);
        assert_eq!(r.said.len(), 1);
        // Rewritten: refused, saying why after git's own progress.
        if let Some(l) = r.lines.get_mut(1) {
            *l = "ng refs/for/main no Change-Id".into();
        }
        let out = write_report(&r, &["no Change-Id".to_owned()], Some(65_515));
        let again = read_report(&out, true);
        assert_eq!(again.lines, ["unpack ok", "ng refs/for/main no Change-Id"]);
        assert_eq!(again.said.len(), 2);
        assert!(String::from_utf8_lossy(&out).contains("kiln: no Change-Id"));
        // Without side-band, the report alone.
        assert_eq!(read_report(&inner, false).lines, ["unpack ok", "ok refs/for/main"]);
        assert_eq!(write_report(&read_report(&inner, false), &["x".to_owned()], None), inner);
        // `side-band` packets stay under 1000 bytes.
        let long = Report { said: Vec::new(), lines: vec!["unpack ok".into(), format!("ng refs/for/main {}", "x".repeat(3000))] };
        assert!(pkt_lines(&write_report(&long, &[], Some(995))).into_iter().flatten().all(|p| p.len() <= 996));
    }

    #[test]
    fn w14_r02_refusal() {
        let u = Update { old: ZERO.into(), new: "2".repeat(40), name: "refs/heads/main".into() };
        let plain = refusal(&[(u.clone(), "a trunk".into())], &["report-status".into()], "refs/heads/main: a trunk");
        assert_eq!(String::from_utf8_lossy(&plain), "000eunpack ok\n001fng refs/heads/main a trunk\n0000");
        let banded = refusal(&[(u, "a trunk".into())], &["report-status".into(), "side-band-64k".into()], "refs/heads/main: a trunk");
        let text = String::from_utf8_lossy(&banded);
        assert!(text.contains("\u{2}kiln: refs/heads/main: a trunk\n") && text.contains("\u{1}000eunpack ok") && text.ends_with("0000"), "{text}");
    }
}
