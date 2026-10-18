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
//!     (`contents: write`), and `refs/for/*` waits for diffs (W14-R03); kiln's own refs are kiln's.
//!
//! A native repository's row says so (`mode`), and its clone URL is its bare repository, so the
//! mirror, polling and events of upstream repositories serve it too; a push wakes the poller at
//! once.

use std::path::{Path, PathBuf};

use http::StatusCode;
use serde::Deserialize;

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
    // Every ref a push brings is kept; nothing is pruned behind kiln's back.
    crate::gitops::run_git(hub, &["-C", &p, "config", "receive.denyNonFastForwards", "false"], crate::gitops::LS_REMOTE_TIMEOUT).await?;
    crate::gitops::run_git(hub, &["-C", &p, "config", "gc.auto", "0"], crate::gitops::LS_REMOTE_TIMEOUT).await?;
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

fn pkt(payload: &[u8]) -> Vec<u8> {
    let mut out = format!("{:04x}", payload.len() + 4).into_bytes();
    out.extend_from_slice(payload);
    out
}

/// A receive-pack answer refusing `updates`, `why` each (and said once to the person pushing),
/// in the bands the client asked for.
pub fn refusal(updates: &[(Update, String)], caps: &[String], message: &str) -> Vec<u8> {
    let mut report = pkt(b"unpack ok\n");
    for (u, why) in updates {
        report.extend(pkt(format!("ng {} {why}\n", u.name).as_bytes()));
    }
    report.extend_from_slice(b"0000");
    if !caps.iter().any(|c| c == "side-band-64k" || c == "side-band") {
        return report;
    }
    let mut out = Vec::new();
    for line in message.lines() {
        out.extend(pkt(format!("\x02kiln: {line}\n").as_bytes()));
    }
    for chunk in report.chunks(65_000) {
        let mut band = vec![1u8];
        band.extend_from_slice(chunk);
        out.extend(pkt(&band));
    }
    out.extend_from_slice(b"0000");
    out
}

/// `.kiln/review.yaml` as a push reads it (W14-R09): the trunks, and who may push to them.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Review {
    pub trunks: Vec<String>,
    pub direct_push: Vec<String>,
}

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
/// branch yet); `None` when it may.
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
    if name.starts_with("refs/for/") {
        return Some("diffs (refs/for/<trunk>) come with W14-R03; push a branch for now".into());
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
        .and_then(|b| serde_yaml::from_slice(&b).ok())
        .unwrap_or_default();
    let mut list = review.trunks.clone();
    if !list.contains(&head) {
        list.push(head);
    }
    (list, review)
}

/// `/<owner>/<name>.git/info/refs` and `…/git-upload-pack`, `…/git-receive-pack` of a native
/// repository.
pub async fn serve(hub: &Hub, repo: &Repo, path: &Path, tail: &[&str], req: Request) -> Result<Response, Fail> {
    let Some(who) = pusher(hub, req.headers()).await else { return Ok(unauthorized()) };
    let read = match &who {
        Pusher::Caller(c) => c.is_owner() || c.can("diffs.read", &Scope::Repo { repo: repo.name.clone() }),
        Pusher::Job(j) => j.repo == repo.id && j.may("contents", kiln_ci::plan::perms::Level::Read),
    };
    if !read {
        return Err((StatusCode::FORBIDDEN, format!("{} may not read {}", who.name(), repo.name)));
    }
    let q = srv::query(&req);
    let pushing = matches!(tail, ["git-receive-pack"]) || (matches!(tail, ["info", "refs"]) && q.get("service").map(String::as_str) == Some("git-receive-pack"));
    if pushing && matches!(&who, Pusher::Job(j) if j.repo != repo.id) {
        return Err((StatusCode::FORBIDDEN, format!("this job's token pushes to its own repository only, not {}", repo.name)));
    }
    let (Some(root), Some(dir)) = (path.parent(), path.file_name()) else { return Err((StatusCode::INTERNAL_SERVER_ERROR, "the repository's path".into())) };
    let path_info = format!("/{}/{}", dir.to_string_lossy(), tail.join("/"));
    let user = who.name();
    let configs: &[&str] = if pushing { &["http.receivepack=true"] } else { &["uploadpack.allowAnySHA1InWant=true", "uploadpack.allowFilter=true"] };
    let cgi = super::facade::Cgi { root, path_info: &path_info, configs, user: &user, method: req.method().as_str().to_owned(), query: req.uri().query().unwrap_or_default().to_owned(), headers: req.headers().clone() };
    if !matches!(tail, ["git-receive-pack"]) {
        let body = srv::body_bytes(req.into_body(), 64 << 20).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
        return super::facade::backend(&cgi, body.to_vec(), None).await;
    }
    if req.headers().get(http::header::CONTENT_ENCODING).is_some_and(|e| e.as_bytes() != b"identity") {
        return Err((StatusCode::UNSUPPORTED_MEDIA_TYPE, "a push's request body isn't compressed".into()));
    }
    // The ref updates, read before git sees any of the push.
    let mut body = req.into_body();
    let mut head: Vec<u8> = Vec::new();
    let (updates, caps) = loop {
        match commands(&head) {
            Ok(Some((u, c, _))) => break (u, c),
            Ok(None) => {}
            Err(e) => return Err((StatusCode::BAD_REQUEST, e)),
        }
        if head.len() >= 1 << 20 {
            return Err((StatusCode::PAYLOAD_TOO_LARGE, "too many ref updates in one push".into()));
        }
        match srv::next_chunk(&mut body).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))? {
            Some(chunk) => head.extend_from_slice(&chunk),
            None => return Err((StatusCode::BAD_REQUEST, "the push ended before its ref updates did".into())),
        }
    };
    let p = path.to_string_lossy().into_owned();
    let (trunk_list, review) = trunks(hub, &p).await;
    let branches = crate::gitops::run_git(hub, &["-C", &p, "for-each-ref", "--count=1", "--format=%(refname)", "refs/heads/"], crate::gitops::LS_REMOTE_TIMEOUT).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    let empty = branches.iter().all(u8::is_ascii_whitespace);
    let no: Vec<(Update, String)> = updates.iter().filter_map(|u| refused(&who, &repo.name, &review, &trunk_list, u, empty).map(|why| (u.clone(), why))).collect();
    if !no.is_empty() {
        // Refused whole: what was allowed waits for the rest.
        let mut all: Vec<(Update, String)> = no.clone();
        for u in updates.iter().filter(|u| !no.iter().any(|(r, _)| r.name == u.name)) {
            all.push((u.clone(), "push refused: another of its updates was".into()));
        }
        let message = no.iter().map(|(u, why)| format!("{}: {why}", u.name)).collect::<Vec<_>>().join("\n");
        hub.store.audit(user.clone(), "repo.push".into(), format!("{} refused: {}", repo.name, message.replace('\n', "; "))).await;
        return Ok(srv::response(StatusCode::OK, "application/x-git-receive-pack-result", srv::full(bytes::Bytes::from(refusal(&all, &caps, &message)))));
    }
    let r = super::facade::backend(&cgi, head, Some(body)).await;
    hub.store.audit(user, "repo.push".into(), format!("{}: {}", repo.name, updates.iter().map(|u| format!("{} {}..{}", u.name, short(&u.old), short(&u.new))).collect::<Vec<_>>().join(", "))).await;
    // The poller takes the new refs now: events, runs.
    hub.gitops_kick.notify();
    r
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
        assert!(refused(&owner, "acme/app", &review, &trunks, &up("refs/for/main", ZERO, &b), false).is_some());
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
    fn w14_r02_refusal() {
        let u = Update { old: ZERO.into(), new: "2".repeat(40), name: "refs/heads/main".into() };
        let plain = refusal(&[(u.clone(), "a trunk".into())], &["report-status".into()], "refs/heads/main: a trunk");
        assert_eq!(String::from_utf8_lossy(&plain), "000eunpack ok\n001fng refs/heads/main a trunk\n0000");
        let banded = refusal(&[(u, "a trunk".into())], &["report-status".into(), "side-band-64k".into()], "refs/heads/main: a trunk");
        let text = String::from_utf8_lossy(&banded);
        assert!(text.contains("\u{2}kiln: refs/heads/main: a trunk\n") && text.contains("\u{1}000eunpack ok") && text.ends_with("0000"), "{text}");
    }
}
