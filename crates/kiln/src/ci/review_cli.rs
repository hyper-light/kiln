//! Diffs from a working copy (docs/ci.md W14-R03, §4.3): `kiln clone` (with git's credential
//! helper and the commit-msg hook that gives each commit a `Change-Id`), `kiln submit` (adding the
//! Change-Ids commits lack, then `git push <remote> HEAD:refs/for/<trunk>`), `kiln sync` (the
//! trunk fetched, landed diffs dropped, the rest rebased), `kiln diffs`, and `kiln checkout
//! D12[@v2]`. git does the work; kiln only says what to run.

use std::io::{Read as _, Write as _};
use std::process::{Command, Stdio};

use anyhow::{Context as _, Result, bail};
use serde_json::Value;

use super::cli::Conn;
use crate::config::Config;
use crate::http::client::Method;

/// The commit-msg hook: a `Change-Id` for every commit that lacks one (Gerrit's trailer, so
/// existing tools read it). Plain sh, as git runs hooks on every platform.
pub const COMMIT_MSG_HOOK: &str = r#"#!/bin/sh
# kiln: every commit gets a Change-Id, so its diff keeps its identity across amends and rebases
# (docs/ci.md W14-R03). Installed by `kiln clone` or `kiln hook install`.
msg="$1"
grep -qi '^Change-Id: I[0-9a-f]\{40\}[[:space:]]*$' "$msg" && exit 0
# An empty message aborts the commit by itself.
grep -qv '^[[:space:]]*\(#.*\)\{0,1\}$' "$msg" || exit 0
id=$( { git var GIT_COMMITTER_IDENT; git var GIT_AUTHOR_IDENT; cat "$msg"; date; } | git hash-object --stdin | cut -c1-40)
git interpret-trailers --in-place --if-exists doNothing --trailer "Change-Id: I$id" "$msg"
"#;

fn git() -> Command {
    let mut c = Command::new("git");
    c.stdin(Stdio::null());
    c
}

/// What `git args` prints, trimmed; an error saying what git said when it fails.
fn out(args: &[&str]) -> Result<String> {
    let o = git().args(args).output().with_context(|| format!("git {}", args.first().unwrap_or(&"")))?;
    if !o.status.success() {
        bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&o.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&o.stdout).trim().to_owned())
}

/// `git args` with the terminal's output: whether it succeeded.
fn run(args: &[&str]) -> Result<()> {
    let st = Command::new("git").args(args).status().with_context(|| format!("git {}", args.first().unwrap_or(&"")))?;
    if !st.success() {
        return Err(anyhow::Error::new(crate::reach::cli::ExitWith(u8::try_from(st.code().unwrap_or(1)).unwrap_or(1))));
    }
    Ok(())
}

/// The remote that is a kiln server (`--remote`, else the one whose URL has `/git/`, else `origin`).
fn remote_of(given: Option<&str>) -> Result<String> {
    if let Some(r) = given {
        return Ok(r.to_owned());
    }
    let list = out(&["remote", "-v"])?;
    let kiln = list.lines().find(|l| l.contains("/git/") && l.ends_with("(push)")).and_then(|l| l.split_whitespace().next());
    Ok(kiln.unwrap_or("origin").to_owned())
}

/// The trunk to submit to: `--trunk`, else the remote's default branch, else `main`.
fn trunk_of(remote: &str, given: Option<&str>) -> String {
    if let Some(t) = given {
        return t.to_owned();
    }
    out(&["symbolic-ref", "--short", &format!("refs/remotes/{remote}/HEAD")]).ok().and_then(|r| r.strip_prefix(&format!("{remote}/")).map(str::to_owned)).unwrap_or_else(|| "main".to_owned())
}

/// `owner/name` of a kiln remote's URL (`…/git/<owner>/<name>.git`).
fn repo_of_remote(remote: &str) -> Option<String> {
    let url = out(&["remote", "get-url", remote]).ok()?;
    let path = url.split("/git/").nth(1)?.trim_end_matches('/').trim_end_matches(".git");
    let (owner, name) = path.split_once('/')?;
    (!owner.is_empty() && !name.is_empty() && !name.contains('/')).then(|| format!("{owner}/{name}"))
}

/// Installs the commit-msg hook in the repository at `dir` (the current one when `None`).
fn install_hook(dir: Option<&str>) -> Result<std::path::PathBuf> {
    let mut args = Vec::new();
    if let Some(d) = dir {
        args.extend(["-C", d]);
    }
    args.extend(["rev-parse", "--git-path", "hooks"]);
    let hooks = std::path::PathBuf::from(out(&args)?);
    let hooks = match dir {
        Some(d) if hooks.is_relative() => std::path::Path::new(d).join(hooks),
        _ => hooks,
    };
    std::fs::create_dir_all(&hooks)?;
    let path = hooks.join("commit-msg");
    if path.exists() && !std::fs::read_to_string(&path).unwrap_or_default().contains("kiln: every commit gets a Change-Id") {
        bail!("{} is another hook: add kiln's Change-Id step to it (see `kiln hook print`)", path.display());
    }
    std::fs::write(&path, COMMIT_MSG_HOOK)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(path)
}

#[derive(clap::Args)]
pub struct CloneArgs {
    #[command(flatten)]
    conn: Conn,
    /// `owner/name`.
    repo: String,
    dir: Option<String>,
}

/// `kiln clone owner/name [dir]`: git's clone of `<server>/git/owner/name.git`, credentials from
/// kiln (`kiln git-credential`), and the commit-msg hook.
pub async fn clone(args: CloneArgs, cfg: &Config) -> Result<()> {
    let api = args.conn.api(cfg).await?;
    let base = api.url().trim_end_matches('/').to_owned();
    let url = format!("{base}/git/{}.git", args.repo.trim_matches('/').trim_end_matches(".git"));
    let exe = std::env::current_exe().context("this kiln's path")?;
    let helper = format!("credential.{base}.helper=!'{}' git-credential", exe.display());
    let dir = args.dir.clone().unwrap_or_else(|| args.repo.rsplit('/').next().unwrap_or("repo").trim_end_matches(".git").to_owned());
    run(&["clone", "-c", &helper, &url, &dir])?;
    let hook = install_hook(Some(&dir))?;
    eprintln!("[kiln] {dir}: commits get a Change-Id ({}); `kiln submit` sends them for review", hook.display());
    Ok(())
}

#[derive(clap::Args)]
pub struct GitCredentialArgs {
    /// git's operation: `get` (the others are ignored).
    op: String,
}

/// git's credential helper (`credential.<server>.helper = !kiln git-credential`): this machine's
/// kiln token for the server git asks about.
pub async fn git_credential(args: GitCredentialArgs, cfg: &Config) -> Result<()> {
    if args.op != "get" {
        return Ok(());
    }
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let field = |k: &str| input.lines().find_map(|l| l.strip_prefix(&format!("{k}="))).map(str::to_owned);
    let (Some(protocol), Some(host)) = (field("protocol"), field("host")) else { return Ok(()) };
    let url = format!("{protocol}://{host}");
    // Only a kiln server's own token, for that server: `KILN_TOKEN` (a personal or agent token)
    // or `KILN_ADMIN_TOKEN` when set, else what this machine has.
    let given = std::env::var("KILN_TOKEN").ok().or_else(|| std::env::var("KILN_ADMIN_TOKEN").ok()).filter(|t| !t.trim().is_empty());
    let Ok(t) = crate::access::resolve(cfg, Some(&url), given.as_deref()).await else { return Ok(()) };
    let mut o = std::io::stdout().lock();
    writeln!(o, "username=kiln")?;
    writeln!(o, "password={}", t.token)?;
    Ok(())
}

#[derive(clap::Args)]
pub struct HookArgs {
    /// `install` (the commit-msg hook, in this repository) or `print`.
    #[arg(default_value = "install")]
    op: String,
}

pub fn hook(args: HookArgs) -> Result<()> {
    match args.op.as_str() {
        "print" => print!("{COMMIT_MSG_HOOK}"),
        "install" => println!("installed {}", install_hook(None)?.display()),
        other => bail!("`{other}`: install or print"),
    }
    Ok(())
}

#[derive(clap::Args)]
pub struct SubmitArgs {
    /// The trunk the diffs are for (default: the remote's default branch).
    #[arg(long)]
    trunk: Option<String>,
    /// The kiln remote (default: the one whose URL has /git/, else origin).
    #[arg(long)]
    remote: Option<String>,
    /// New diffs are drafts: visible, not asking for review.
    #[arg(long)]
    draft: bool,
    /// Drafts in the stack ask for review.
    #[arg(long)]
    ready: bool,
    /// Reviewers (`ada`, `agent:review`), for every diff the push changes.
    #[arg(short = 'r', long = "reviewer")]
    reviewers: Vec<String>,
    /// A note on the new versions ("addressed comments").
    #[arg(short, long)]
    message: Option<String>,
    #[arg(long)]
    topic: Option<String>,
}

/// The commits between `base` and HEAD, bottom first: (sha, parents, message).
fn stack(base: &str) -> Result<Vec<(String, Vec<String>, String)>> {
    let log = out(&["log", "--reverse", "--topo-order", "--format=%H%x1f%P%x1f%B%x1e", &format!("{base}..HEAD")])?;
    Ok(log
        .split('\x1e')
        .filter_map(|rec| {
            let mut f = rec.trim_start_matches('\n').splitn(3, '\x1f');
            let (sha, parents, message) = (f.next()?.trim().to_owned(), f.next()?.split_whitespace().map(str::to_owned).collect(), f.next().unwrap_or_default().to_owned());
            (!sha.is_empty()).then_some((sha, parents, message))
        })
        .collect())
}

/// A new Change-Id: `I` and 40 hex digits no other commit has.
fn new_change_id(seed: &str) -> String {
    let h = crate::util::sha256_hex(format!("{seed}{}", crate::util::random_token()).as_bytes());
    format!("I{}", h.get(..40).unwrap_or(&h))
}

/// Gives every commit of the stack that lacks one a Change-Id: the commits from the first such
/// one up are rewritten (their trees, authors and dates kept), and HEAD moves to the new tip.
/// How many were given one.
fn add_change_ids(list: &[(String, Vec<String>, String)]) -> Result<usize> {
    let Some(first) = list.iter().position(|(_, _, m)| kiln_ci::review::change_id(m).is_none()) else { return Ok(0) };
    let Some(mut parent) = list.get(first).and_then(|(_, p, _)| p.first().cloned()) else { bail!("the first commit without a Change-Id has no parent") };
    let mut given = 0;
    for (sha, parents, message) in list.iter().skip(first) {
        if parents.len() != 1 {
            bail!("{} is a merge: a diff is one commit on another", sha.get(..12).unwrap_or(sha));
        }
        let msg = match kiln_ci::review::change_id(message) {
            Some(_) => message.clone(),
            None => {
                given += 1;
                kiln_ci::review::with_change_id(message, &new_change_id(sha))
            }
        };
        let who = out(&["log", "-1", "--format=%an%x00%ae%x00%ad%x00%cn%x00%ce%x00%cd", "--date=raw", sha])?;
        let f: Vec<&str> = who.split('\0').collect();
        let get = |i: usize| f.get(i).copied().unwrap_or_default();
        let tree = out(&["rev-parse", &format!("{sha}^{{tree}}")])?;
        let mut cmd = git();
        cmd.args(["commit-tree", &tree, "-p", &parent, "-F", "-"])
            .env("GIT_AUTHOR_NAME", get(0))
            .env("GIT_AUTHOR_EMAIL", get(1))
            .env("GIT_AUTHOR_DATE", get(2))
            .env("GIT_COMMITTER_NAME", get(3))
            .env("GIT_COMMITTER_EMAIL", get(4))
            .env("GIT_COMMITTER_DATE", get(5))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        let mut child = cmd.spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(msg.as_bytes())?;
        }
        let o = child.wait_with_output()?;
        if !o.status.success() {
            bail!("git commit-tree: {}", String::from_utf8_lossy(&o.stderr).trim());
        }
        parent = String::from_utf8_lossy(&o.stdout).trim().to_owned();
    }
    let old = out(&["rev-parse", "HEAD"])?;
    match out(&["symbolic-ref", "-q", "HEAD"]) {
        Ok(branch) if !branch.is_empty() => run(&["update-ref", "-m", "kiln submit: Change-Ids", &branch, &parent, &old])?,
        _ => run(&["update-ref", "--no-deref", "-m", "kiln submit: Change-Ids", "HEAD", &parent, &old])?,
    }
    Ok(given)
}

/// `kiln submit`: every commit between the trunk and HEAD as a diff (or a new version of its
/// diff), through `git push <remote> HEAD:refs/for/<trunk>`.
pub fn submit(args: SubmitArgs) -> Result<()> {
    let remote = remote_of(args.remote.as_deref())?;
    let trunk = trunk_of(&remote, args.trunk.as_deref());
    run(&["fetch", "--quiet", &remote, &trunk])?;
    let base = out(&["merge-base", "FETCH_HEAD", "HEAD"])?;
    let list = stack(&base)?;
    if list.is_empty() {
        bail!("nothing to submit: HEAD is on {remote}/{trunk}");
    }
    let given = add_change_ids(&list)?;
    if given > 0 {
        eprintln!("[kiln] gave {given} commit{} a Change-Id", if given == 1 { "" } else { "s" });
    }
    let mut push: Vec<String> = vec!["push".into(), remote.clone(), format!("HEAD:refs/for/{trunk}")];
    let mut opt = |o: String| push.extend(["-o".to_owned(), o]);
    if args.draft {
        opt("draft".into());
    }
    if args.ready {
        opt("ready".into());
    }
    if !args.reviewers.is_empty() {
        opt(format!("reviewer={}", args.reviewers.join(",")));
    }
    if let Some(m) = &args.message {
        opt(format!("message={m}"));
    }
    if let Some(t) = &args.topic {
        opt(format!("topic={t}"));
    }
    run(&push.iter().map(String::as_str).collect::<Vec<_>>())
}

#[derive(clap::Args)]
pub struct SyncArgs {
    #[arg(long)]
    trunk: Option<String>,
    #[arg(long)]
    remote: Option<String>,
}

/// `kiln sync`: the trunk fetched, and the stack rebased onto it; diffs that landed drop out (git
/// sees their patches upstream).
pub fn sync(args: SyncArgs) -> Result<()> {
    let remote = remote_of(args.remote.as_deref())?;
    let trunk = trunk_of(&remote, args.trunk.as_deref());
    run(&["fetch", "--quiet", &remote])?;
    run(&["rebase", &format!("{remote}/{trunk}")])
}

#[derive(clap::Args)]
pub struct AbsorbArgs {
    /// Where the stack starts (default: where HEAD left the remote's trunk, as last fetched).
    #[arg(long)]
    base: Option<String>,
    #[arg(long)]
    trunk: Option<String>,
    #[arg(long)]
    remote: Option<String>,
    /// Say where each edit would go, and change nothing.
    #[arg(short = 'n', long)]
    dry_run: bool,
}

/// `kiln absorb`: each edit of the working copy (to a file HEAD has, staged or not) amended into
/// the commit of the stack that last touched its lines; an insertion goes where the lines on both
/// sides of it come from one commit. The commits from the lowest one up are rewritten (authors,
/// dates and messages kept) and HEAD moves to the new top; the rest stays in the working copy.
pub fn absorb(args: AbsorbArgs) -> Result<()> {
    let top = out(&["rev-parse", "--show-toplevel"])?;
    let base = match &args.base {
        Some(b) => out(&["rev-parse", "--verify", &format!("{b}^{{commit}}")])?,
        None => {
            let remote = remote_of(args.remote.as_deref())?;
            let trunk = trunk_of(&remote, args.trunk.as_deref());
            out(&["merge-base", &format!("refs/remotes/{remote}/{trunk}"), "HEAD"]).with_context(|| format!("where does the stack start? `git fetch {remote}`, or pass --base"))?
        }
    };
    let r = absorb_in(std::path::Path::new(&top), &base, args.dry_run)?;
    if r.edits.is_empty() {
        println!("nothing to absorb: no edits to files HEAD has");
        return Ok(());
    }
    let mut subjects: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for (e, into) in &r.edits {
        match into {
            Ok(sha) => {
                let s = subjects.entry(sha.clone()).or_insert_with(|| out(&["log", "-1", "--format=%s", sha]).unwrap_or_default());
                println!("{}:{}  into {} {s}", e.path, e.line, sha.get(..12).unwrap_or(sha));
            }
            Err(why) => println!("{}:{}  stays: {why}", e.path, e.line),
        }
    }
    let n = r.edits.iter().filter(|(_, into)| into.is_ok()).count();
    let edits = if r.edits.len() == 1 { "edit" } else { "edits" };
    match &r.now {
        _ if args.dry_run => eprintln!("[kiln] {n} of {} {edits} would be absorbed (--dry-run: nothing changed)", r.edits.len()),
        Some(_) => eprintln!("[kiln] absorbed {n} of {} {edits}; the stack was {} (`kiln submit` sends the new versions)", r.edits.len(), r.was),
        None => eprintln!("[kiln] nothing absorbed"),
    }
    Ok(())
}

/// One edit of the working copy: HEAD's lines `start..start + old.len()` (from 1; after line
/// `start` when `old` is empty) become `new`, at `line` in the working copy.
struct Edit {
    path: String,
    start: usize,
    line: usize,
    old: Vec<Vec<u8>>,
    new: Vec<Vec<u8>>,
}

/// What `kiln absorb` did: each edit with the commit it went into (or why it stayed), the stack's
/// top before, and its new top (none when nothing moved).
struct Absorbed {
    edits: Vec<(Edit, Result<String, String>)>,
    was: String,
    now: Option<String>,
}

/// `git -C dir args` (pathspecs literal): its output, or an error saying what git said.
fn git_in(dir: &std::path::Path, args: &[&str], env: &[(&str, &std::path::Path)], input: Option<&[u8]>) -> Result<Vec<u8>> {
    let mut c = git();
    c.arg("-C").arg(dir).arg("--literal-pathspecs").args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    for (k, v) in env {
        c.env(k, v);
    }
    if input.is_some() {
        c.stdin(Stdio::piped());
    }
    let mut child = c.spawn().with_context(|| format!("git {}", args.first().unwrap_or(&"")))?;
    if let (Some(data), Some(mut stdin)) = (input, child.stdin.take()) {
        stdin.write_all(data)?;
    }
    let o = child.wait_with_output()?;
    if !o.status.success() {
        bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&o.stderr).trim());
    }
    Ok(o.stdout)
}

fn text_in(dir: &std::path::Path, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8_lossy(&git_in(dir, args, &[], None)?).trim().to_owned())
}

/// `path`'s mode and blob in `rev`.
fn entry(dir: &std::path::Path, rev: &str, path: &str) -> Result<Option<(String, String)>> {
    let listed = git_in(dir, &["ls-tree", "-z", rev, "--", path], &[], None)?;
    let first = listed.split(|b| *b == 0).next().unwrap_or_default();
    let meta = String::from_utf8_lossy(first.split(|b| *b == b'\t').next().unwrap_or_default()).into_owned();
    let mut f = meta.split(' ');
    Ok(match (f.next(), f.next(), f.next()) {
        (Some(mode), Some("blob"), Some(sha)) => Some((mode.to_owned(), sha.to_owned())),
        _ => None,
    })
}

/// The edits in one file's `git diff -U0`.
fn edits_of(path: &str, patch: &[u8]) -> Vec<Edit> {
    let mut list: Vec<Edit> = Vec::new();
    let mut into_new = false;
    for line in patch.split_inclusive(|b| *b == b'\n') {
        if let Some(rest) = line.strip_prefix(b"@@ -") {
            let head = String::from_utf8_lossy(rest);
            let mut f = head.split_whitespace();
            let mut num = || f.next().and_then(|r| r.trim_start_matches('+').split(',').next()).and_then(|n| n.parse().ok()).unwrap_or(0);
            let (start, line) = (num(), num());
            list.push(Edit { path: path.to_owned(), start, line, old: Vec::new(), new: Vec::new() });
            continue;
        }
        let Some(e) = list.last_mut() else { continue };
        match line.split_first() {
            Some((b'-', rest)) => {
                e.old.push(rest.to_vec());
                into_new = false;
            }
            Some((b'+', rest)) => {
                e.new.push(rest.to_vec());
                into_new = true;
            }
            // "\ No newline at end of file": the line before it ends without one.
            Some((b'\\', _)) => {
                if let Some(l) = if into_new { e.new.last_mut() } else { e.old.last_mut() }
                    && l.last() == Some(&b'\n')
                {
                    l.pop();
                }
            }
            _ => {}
        }
    }
    list
}

/// `path` at `rev`, blamed within the stack (`base..rev`): each line's `(commit, its line
/// there)`; lines from before the stack are the boundary's.
fn blame(dir: &std::path::Path, base: &str, rev: &str, path: &str) -> Result<Vec<(String, usize)>> {
    let text = git_in(dir, &["blame", "--porcelain", "--ignore-revs-file", "", &format!("{base}..{rev}"), "--", path], &[], None)?;
    let mut lines: Vec<Option<(String, usize)>> = Vec::new();
    for l in text.split(|b| *b == b'\n').filter(|l| l.first() != Some(&b'\t')) {
        let s = String::from_utf8_lossy(l);
        let mut f = s.split(' ');
        let (Some(sha), Some(Ok(orig)), Some(Ok(at))) = (f.next(), f.next().map(str::parse::<usize>), f.next().map(str::parse::<usize>)) else { continue };
        if sha.len() < 40 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) || at == 0 {
            continue;
        }
        if lines.len() < at {
            lines.resize(at, None);
        }
        if let Some(slot) = lines.get_mut(at - 1) {
            *slot = Some((sha.to_owned(), orig));
        }
    }
    lines.into_iter().collect::<Option<Vec<_>>>().context("git blame left a line out")
}

/// The commit of the stack (its index) an edit belongs in, or why it has none: the one every line
/// it changes comes from, or for an insertion, the lines on both sides of it.
fn target(e: &Edit, head: &[(String, usize)], at: &std::collections::HashMap<&str, usize>) -> Result<usize, String> {
    let insert = e.old.is_empty();
    let lines: Vec<&(String, usize)> = if insert {
        [e.start.checked_sub(1), Some(e.start)].into_iter().flatten().filter_map(|i| head.get(i)).collect()
    } else {
        head.get(e.start.saturating_sub(1)..e.start.saturating_sub(1) + e.old.len()).map(|s| s.iter().collect()).unwrap_or_default()
    };
    let mut into = None;
    for (sha, _) in lines {
        let Some(&i) = at.get(sha.as_str()) else {
            return Err(if insert { "next to lines from before the stack" } else { "changes lines from before the stack" }.to_owned());
        };
        if into.is_some_and(|j| j != i) {
            return Err(if insert { "between lines of two commits" } else { "changes lines of more than one commit" }.to_owned());
        }
        into = Some(i);
    }
    into.ok_or_else(|| "no lines around it".to_owned())
}

/// Where `e` goes in a commit whose file is `lines` and whose blame `pos` finds each `(commit,
/// line there)` at: the index of its first line (or where it inserts), when its lines are there,
/// in one piece and unchanged.
fn place(e: &Edit, head: &[(String, usize)], pos: &std::collections::HashMap<(&str, usize), usize>, lines: &[&[u8]]) -> Option<usize> {
    let find = |i: usize| head.get(i).and_then(|(sha, o)| pos.get(&(sha.as_str(), *o)).copied());
    let p = if e.old.is_empty() {
        let above = match e.start {
            0 => None,
            s => Some(find(s - 1)?),
        };
        let below = if e.start < head.len() { Some(find(e.start)?) } else { None };
        let p = match (above, below) {
            (Some(a), Some(b)) if b == a + 1 => b,
            (Some(a), None) => a + 1,
            (None, Some(b)) => b,
            _ => return None,
        };
        // Not after a last line without a newline.
        if p > 0 && lines.get(p - 1).is_some_and(|l| l.last() != Some(&b'\n')) {
            return None;
        }
        p
    } else {
        let first = find(e.start.checked_sub(1)?)?;
        for i in 1..e.old.len() {
            if find(e.start - 1 + i)? != first + i {
                return None;
            }
        }
        first
    };
    let here = lines.get(p..p + e.old.len())?;
    if here.iter().zip(&e.old).any(|(a, b)| *a != b.as_slice()) {
        return None;
    }
    // A new last line without a newline stays the file's last.
    if e.new.last().is_some_and(|l| l.last() != Some(&b'\n')) && p + e.old.len() != lines.len() {
        return None;
    }
    Some(p)
}

/// Removes a file when dropped.
struct Gone(std::path::PathBuf);

impl Drop for Gone {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// `sha` again with `tree` and `parent`: its author, committer, dates and message kept (a
/// signature dropped: it no longer matches).
fn recommit(dir: &std::path::Path, sha: &str, tree: &str, parent: &str) -> Result<String> {
    let raw = git_in(dir, &["cat-file", "commit", sha], &[], None)?;
    let end = raw.windows(2).position(|w| w == b"\n\n").map_or(raw.len(), |i| i + 1);
    let (head, body) = raw.split_at(end);
    let mut commit = Vec::with_capacity(raw.len());
    let mut signature = false;
    for line in head.split_inclusive(|b| *b == b'\n') {
        if line.starts_with(b" ") {
            if !signature {
                commit.extend_from_slice(line);
            }
            continue;
        }
        signature = line.starts_with(b"gpgsig");
        if line.starts_with(b"tree ") {
            commit.extend_from_slice(format!("tree {tree}\n").as_bytes());
        } else if line.starts_with(b"parent ") {
            commit.extend_from_slice(format!("parent {parent}\n").as_bytes());
        } else if !signature {
            commit.extend_from_slice(line);
        }
    }
    commit.extend_from_slice(body);
    Ok(String::from_utf8_lossy(&git_in(dir, &["hash-object", "-t", "commit", "-w", "--stdin"], &[], Some(&commit))?).trim().to_owned())
}

/// `kiln absorb` in the working copy at `dir` (its top), for the stack above `base`.
fn absorb_in(dir: &std::path::Path, base: &str, dry_run: bool) -> Result<Absorbed> {
    use std::collections::{BTreeSet, HashMap};
    for busy in ["MERGE_HEAD", "CHERRY_PICK_HEAD", "REVERT_HEAD", "rebase-merge", "rebase-apply"] {
        if dir.join(text_in(dir, &["rev-parse", "--git-path", busy])?).exists() {
            bail!("a merge, rebase, cherry-pick or revert is in progress: finish it first");
        }
    }
    let was = text_in(dir, &["rev-parse", "--verify", "HEAD"])?;
    // The stack, bottom first, with its parents.
    let mut stack: Vec<(String, String)> = Vec::new();
    for l in text_in(dir, &["rev-list", "--reverse", "--topo-order", "--parents", &format!("{base}..HEAD")])?.lines() {
        let f: Vec<&str> = l.split_whitespace().collect();
        match f.as_slice() {
            [sha, parent] => stack.push(((*sha).to_owned(), (*parent).to_owned())),
            [sha, ..] => bail!("{} is a merge: a stack is single commits", sha.get(..12).unwrap_or(sha)),
            [] => {}
        }
    }
    if stack.is_empty() {
        bail!("nothing to absorb into: HEAD is the stack's base");
    }
    let at: HashMap<&str, usize> = stack.iter().enumerate().map(|(i, (sha, _))| (sha.as_str(), i)).collect();
    let status = git_in(dir, &["diff", "--no-renames", "--name-status", "-z", "HEAD"], &[], None)?;
    let mut fields = status.split(|b| *b == 0).filter(|f| !f.is_empty());
    let mut files = Vec::new();
    while let (Some(st), Some(path)) = (fields.next(), fields.next()) {
        if let (b"M", Ok(p)) = (st, std::str::from_utf8(path)) {
            files.push(p.to_owned());
        }
    }
    // Each edit and the commit it belongs in.
    let mut edits: Vec<(Edit, Result<usize, String>)> = Vec::new();
    let mut heads: HashMap<String, Vec<(String, usize)>> = HashMap::new();
    for path in &files {
        let patch = git_in(dir, &["diff", "--no-ext-diff", "--no-color", "--no-textconv", "--no-renames", "-U0", "HEAD", "--", path], &[], None)?;
        let list = edits_of(path, &patch);
        let mode = entry(dir, "HEAD", path)?.map(|(m, _)| m).unwrap_or_default();
        let whole = |why: &str| (Edit { path: path.clone(), start: 0, line: 0, old: Vec::new(), new: Vec::new() }, Err(why.to_owned()));
        if list.is_empty() {
            edits.push(whole(if patch.windows(12).any(|w| w == b"Binary files") { "a binary file" } else { "no lines changed" }));
            continue;
        }
        if mode != "100644" && mode != "100755" {
            edits.push(whole("not a regular file"));
            continue;
        }
        let head = blame(dir, base, "HEAD", path)?;
        for e in list {
            let t = target(&e, &head, &at);
            edits.push((e, t));
        }
        heads.insert(path.clone(), head);
    }
    // Where each edit goes in every commit from its own up: those it can't be placed in stay.
    let mut places: HashMap<(usize, usize), usize> = HashMap::new();
    for path in &files {
        let Some(head) = heads.get(path) else { continue };
        let mine: Vec<usize> = edits.iter().enumerate().filter(|(_, (e, t))| e.path == *path && t.is_ok()).map(|(i, _)| i).collect();
        let Some(lowest) = mine.iter().filter_map(|&i| edits.get(i).and_then(|(_, t)| t.as_ref().ok().copied())).min() else { continue };
        for (k, (sha, _)) in stack.iter().enumerate().skip(lowest) {
            let here = match (blame(dir, base, sha, path), entry(dir, sha, path)?) {
                (Ok(b), Some((_, blob))) => Some((b, git_in(dir, &["cat-file", "blob", &blob], &[], None)?)),
                _ => None,
            };
            let found: Option<(HashMap<(&str, usize), usize>, Vec<&[u8]>)> = here.as_ref().map(|(b, data)| (b.iter().enumerate().map(|(n, (s, o))| ((s.as_str(), *o), n)).collect(), data.split_inclusive(|c| *c == b'\n').collect()));
            for &i in &mine {
                let Some((e, t)) = edits.get_mut(i) else { continue };
                if !t.as_ref().is_ok_and(|&c| c <= k) {
                    continue;
                }
                match found.as_ref().and_then(|(pos, lines)| place(e, head, pos, lines)) {
                    Some(p) => {
                        places.insert((i, k), p);
                    }
                    None => *t = Err(format!("the commits above {} move or change its lines", sha.get(..12).unwrap_or(sha))),
                }
            }
        }
    }
    let lowest = edits.iter().filter_map(|(_, t)| t.as_ref().ok().copied()).min();
    let named = |edits: Vec<(Edit, Result<usize, String>)>, shas: &[String]| edits.into_iter().map(|(e, t)| (e, t.map(|i| shas.get(i).cloned().unwrap_or_default()))).collect();
    let old: Vec<String> = stack.iter().map(|(sha, _)| sha.clone()).collect();
    let Some(lowest) = lowest.filter(|_| !dry_run) else { return Ok(Absorbed { edits: named(edits, &old), was, now: None }) };
    // The commits from the lowest up, each with the edits at or below it.
    let git_dir = std::path::PathBuf::from(text_in(dir, &["rev-parse", "--absolute-git-dir"])?);
    let mut shas = old.clone();
    let mut parent = stack.get(lowest).map(|(_, p)| p.clone()).unwrap_or_default();
    let mut changed: BTreeSet<&str> = BTreeSet::new();
    for (k, (sha, _)) in stack.iter().enumerate().skip(lowest) {
        let mut by_file: HashMap<&str, Vec<(usize, &Edit)>> = HashMap::new();
        for (i, (e, t)) in edits.iter().enumerate() {
            if let (Ok(c), Some(&p)) = (t, places.get(&(i, k)))
                && *c <= k
            {
                by_file.entry(e.path.as_str()).or_default().push((p, e));
            }
        }
        let tree = if by_file.is_empty() {
            text_in(dir, &["rev-parse", &format!("{sha}^{{tree}}")])?
        } else {
            let index = Gone(git_dir.join(format!("kiln-absorb-{}.index", crate::util::random_token())));
            let env = [("GIT_INDEX_FILE", index.0.as_path())];
            git_in(dir, &["read-tree", sha], &env, None)?;
            for (path, mut list) in by_file {
                let (mode, blob) = entry(dir, sha, path)?.with_context(|| format!("{path} isn't in {sha}"))?;
                let data = git_in(dir, &["cat-file", "blob", &blob], &[], None)?;
                let mut lines: Vec<&[u8]> = data.split_inclusive(|c| *c == b'\n').collect();
                list.sort_by_key(|(p, _)| std::cmp::Reverse(*p));
                for (p, e) in list {
                    if lines.get(p..p + e.old.len()).is_none() {
                        bail!("{path}: an edit's lines aren't in {sha}; nothing changed");
                    }
                    let rest = lines.split_off(p + e.old.len());
                    lines.truncate(p);
                    lines.extend(e.new.iter().map(Vec::as_slice));
                    lines.extend(rest);
                }
                let new_blob = String::from_utf8_lossy(&git_in(dir, &["hash-object", "-w", "--stdin"], &[], Some(&lines.concat()))?).trim().to_owned();
                git_in(dir, &["update-index", "--cacheinfo", &mode, &new_blob, path], &env, None)?;
                changed.insert(path);
            }
            String::from_utf8_lossy(&git_in(dir, &["write-tree"], &env, None)?).trim().to_owned()
        };
        parent = recommit(dir, sha, &tree, &parent)?;
        if let Some(s) = shas.get_mut(k) {
            s.clone_from(&parent);
        }
    }
    // A file all of whose edits were absorbed is now the working copy's, exactly.
    for path in &changed {
        let all = edits.iter().filter(|(e, _)| e.path == *path).all(|(_, t)| t.is_ok());
        let now = entry(dir, &parent, path)?.map(|(_, b)| b);
        if all && now.as_deref() != Some(text_in(dir, &["hash-object", "--", path])?.as_str()) {
            bail!("the new {path} isn't the working copy's: nothing changed (HEAD is still {was})");
        }
    }
    match text_in(dir, &["symbolic-ref", "-q", "HEAD"]) {
        Ok(branch) if !branch.is_empty() => git_in(dir, &["update-ref", "-m", "kiln absorb", &branch, &parent, &was], &[], None)?,
        _ => git_in(dir, &["update-ref", "--no-deref", "-m", "kiln absorb", "HEAD", &parent, &was], &[], None)?,
    };
    // The files it changed are staged as the new top has them; what stays in them is unstaged.
    let mut reset = vec!["reset", "-q", "--"];
    reset.extend(changed.iter().copied());
    git_in(dir, &reset, &[], None)?;
    Ok(Absorbed { edits: named(edits, &shas), was, now: Some(parent) })
}

#[derive(clap::Args)]
pub struct DiffsArgs {
    #[command(flatten)]
    conn: Conn,
    /// `owner/name` (default: this working copy's kiln remote's).
    #[arg(long)]
    repo: Option<String>,
    /// `open` (the default), a state (`draft`, `needs-review`, …), or `all`.
    #[arg(long, default_value = "open")]
    state: String,
    #[arg(long)]
    author: Option<String>,
}

pub async fn diffs(args: DiffsArgs, cfg: &Config) -> Result<()> {
    let repo = match args.repo.clone() {
        Some(r) => r,
        None => repo_of_remote(&remote_of(None)?).context("which repository? pass --repo owner/name")?,
    };
    let api = args.conn.api(cfg).await?;
    let mut path = format!("/api/repos/{}/diffs?limit=200", super::cli::enc(&repo));
    if args.state != "all" {
        path.push_str(&format!("&state={}", super::cli::enc(&args.state)));
    }
    if let Some(a) = &args.author {
        path.push_str(&format!("&author={}", super::cli::enc(a)));
    }
    let v = api.call_api(Method::Get, &path, None).await?;
    let list = v.get("diffs").and_then(Value::as_array).cloned().unwrap_or_default();
    if list.is_empty() {
        println!("no {} diffs in {repo}", if args.state == "all" { "" } else { args.state.as_str() });
    }
    let s = |d: &Value, k: &str| d.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    for d in list.iter().rev() {
        let below = d.get("parent").and_then(Value::as_str).map(|p| format!(" (on {p})")).unwrap_or_default();
        println!("{:<6} {:<18} {:<14} {}{below}", s(d, "name"), s(d, "state"), s(d, "author"), s(d, "title"));
    }
    Ok(())
}

#[derive(clap::Args)]
pub struct CheckoutArgs {
    /// `D12` (its latest version) or `D12@v2`.
    diff: String,
    #[arg(long)]
    remote: Option<String>,
}

/// `kiln checkout D12[@v2]`: a diff's version, fetched from its ref and checked out (detached).
pub fn checkout(args: CheckoutArgs) -> Result<()> {
    let remote = remote_of(args.remote.as_deref())?;
    let (name, version) = args.diff.split_once('@').map_or((args.diff.as_str(), None), |(d, v)| (d, Some(v.trim_start_matches('v'))));
    let name = if name.starts_with(['D', 'd']) { format!("D{}", name.trim_start_matches(['D', 'd'])) } else { format!("D{name}") };
    let listed = out(&["ls-remote", &remote, &format!("refs/kiln/diffs/{name}/*")])?;
    let mut versions: Vec<(u32, String)> = listed.lines().filter_map(|l| l.split_once('\t')).filter_map(|(_, r)| r.rsplit('/').next().and_then(|v| v.strip_prefix('v')).and_then(|n| n.parse().ok()).map(|n| (n, r.to_owned()))).collect();
    versions.sort();
    let pick = match version {
        Some(v) => {
            let n: u32 = v.parse().with_context(|| format!("`{v}` isn't a version"))?;
            versions.into_iter().find(|(k, _)| *k == n).with_context(|| format!("{name} has no version {n}"))?
        }
        None => versions.pop().with_context(|| format!("{remote} has no diff {name}"))?,
    };
    run(&["fetch", "--quiet", &remote, &pick.1])?;
    run(&["checkout", "--detach", "FETCH_HEAD"])?;
    eprintln!("[kiln] {name} v{} checked out (detached)", pick.0);
    Ok(())
}

#[derive(clap::Args)]
pub struct CrArgs {
    #[command(subcommand)]
    cmd: CrCmd,
}

#[derive(clap::Subcommand)]
enum CrCmd {
    /// A change request for a branch (default: this one), reviewed against the trunk as a whole;
    /// each push of the branch after it is a new version.
    Open {
        branch: Option<String>,
        #[arg(long)]
        trunk: Option<String>,
        #[arg(long)]
        remote: Option<String>,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        draft: bool,
        #[arg(short = 'r', long = "reviewer")]
        reviewers: Vec<String>,
    },
}

/// `kiln cr open [branch]`: `git push <remote> <branch>:refs/for/<trunk>/branch/<branch>`, and
/// the branch then tracks the remote's, so `git push` adds versions.
pub fn cr(args: CrArgs) -> Result<()> {
    let CrCmd::Open { branch, trunk, remote, title, draft, reviewers } = args.cmd;
    let remote = remote_of(remote.as_deref())?;
    let trunk = trunk_of(&remote, trunk.as_deref());
    let branch = match branch {
        Some(b) => b,
        None => out(&["symbolic-ref", "--short", "HEAD"]).context("which branch? name it")?,
    };
    if branch == trunk {
        bail!("{branch} is the trunk: a change request reviews another branch");
    }
    let mut push: Vec<String> = vec!["push".into(), remote.clone(), format!("{branch}:refs/for/{trunk}/branch/{branch}")];
    let mut opt = |o: String| push.extend(["-o".to_owned(), o]);
    if let Some(t) = &title {
        opt(format!("title={t}"));
    }
    if draft {
        opt("draft".into());
    }
    if !reviewers.is_empty() {
        opt(format!("reviewer={}", reviewers.join(",")));
    }
    run(&push.iter().map(String::as_str).collect::<Vec<_>>())?;
    // The branch follows the server's from here (best effort).
    if out(&["fetch", "--quiet", &remote, &format!("{branch}:refs/remotes/{remote}/{branch}")]).is_ok() {
        let _ = out(&["branch", "--quiet", &format!("--set-upstream-to={remote}/{branch}"), &branch]);
    }
    Ok(())
}

#[derive(clap::Args)]
pub struct LandArgs {
    #[command(flatten)]
    conn: Conn,
    /// The diffs to land (each with those below it): `D12`.
    #[arg(required = true)]
    diffs: Vec<String>,
    /// `stack`, `stack-squash`, `ff-only` (default: the trunk's `land.default`).
    #[arg(long)]
    strategy: Option<String>,
}

/// `kiln land D12 [--strategy S]`: into the land queue, with the diffs below it.
pub async fn land(args: LandArgs, cfg: &Config) -> Result<()> {
    let api = args.conn.api(cfg).await?;
    for d in &args.diffs {
        let body = match &args.strategy {
            Some(s) => serde_json::json!({ "strategy": s }),
            None => serde_json::json!({}),
        };
        let v = api.call_api(Method::Post, &format!("/api/diffs/{}/land", super::cli::enc(d)), Some(body)).await?;
        let names: Vec<&str> = v.get("diffs").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
        println!("{} queued to land on {} ({}): entry {}", names.join(" "), v.get("trunk").and_then(Value::as_str).unwrap_or_default(), v.get("strategy").and_then(Value::as_str).unwrap_or_default(), v.get("entry").and_then(Value::as_str).unwrap_or_default());
    }
    Ok(())
}

#[derive(clap::Args)]
pub struct ReviewArgs {
    #[command(flatten)]
    conn: Conn,
    /// `D12`.
    diff: String,
    #[arg(long, conflicts_with_all = ["request_changes", "comment", "resign"])]
    accept: bool,
    #[arg(long, conflicts_with_all = ["comment", "resign"])]
    request_changes: bool,
    #[arg(long, conflicts_with = "resign")]
    comment: bool,
    #[arg(long)]
    resign: bool,
    /// What the review says.
    #[arg(short, long)]
    message: Option<String>,
}

/// `kiln review D12`: the diff, what keeps it from landing, and its open threads; with
/// `--accept`, `--request-changes`, `--comment` or `--resign`, a review (publishing your drafts).
pub async fn review(args: ReviewArgs, cfg: &Config) -> Result<()> {
    let api = args.conn.api(cfg).await?;
    let id = super::cli::enc(&args.diff);
    let action = if args.accept {
        Some("accept")
    } else if args.request_changes {
        Some("request_changes")
    } else if args.comment || args.message.is_some() {
        Some("comment")
    } else if args.resign {
        Some("resign")
    } else {
        None
    };
    if let Some(a) = action {
        let v = api.call_api(Method::Post, &format!("/api/diffs/{id}/review"), Some(serde_json::json!({ "action": a, "body": args.message.clone().unwrap_or_default() }))).await?;
        println!("{} v{}: {} ({} draft comment{} published); it is {}", args.diff, v.get("version").and_then(Value::as_u64).unwrap_or(0), a, v.get("published").and_then(Value::as_u64).unwrap_or(0), if v.get("published").and_then(Value::as_u64) == Some(1) { "" } else { "s" }, v.get("state").and_then(Value::as_str).unwrap_or_default());
        return Ok(());
    }
    let d = api.call_api(Method::Get, &format!("/api/diffs/{id}"), None).await?;
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    let versions = d.get("versions").and_then(Value::as_array).map_or(0, Vec::len);
    println!("{} {} ({}, v{versions}, by {})", s(&d, "name"), s(&d, "title"), s(&d, "state"), s(&d, "author"));
    if !s(&d, "summary").is_empty() {
        println!("\n{}\n", s(&d, "summary"));
    }
    let land = api.call_api(Method::Get, &format!("/api/diffs/{id}/land"), None).await?;
    let blockers: Vec<&str> = land.get("blockers").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
    if blockers.is_empty() {
        println!("ready to land: `kiln land {}`", s(&d, "name"));
    } else {
        println!("not ready to land:");
        for b in blockers {
            println!("  - {b}");
        }
    }
    let threads = api.call_api(Method::Get, &format!("/api/diffs/{id}/comments"), None).await?;
    for t in threads.get("threads").and_then(Value::as_array).into_iter().flatten().filter(|t| s(t, "state") == "open") {
        let at = match (t.get("file").and_then(Value::as_str), t.pointer("/now/start_line").and_then(Value::as_u64)) {
            (Some(f), Some(l)) => format!("{f}:{l}"),
            (Some(f), None) => format!("{f} (outdated)"),
            _ => "the diff".to_owned(),
        };
        println!("\nthread on {at}:");
        for c in t.get("comments").and_then(Value::as_array).into_iter().flatten() {
            let draft = if c.get("draft") == Some(&Value::Bool(true)) { " (draft)" } else { "" };
            println!("  {}{draft}: {}", s(c, "author"), s(c, "body"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w14_r03_change_ids() {
        let id = new_change_id("abc");
        assert!(kiln_ci::review::is_change_id(&id), "{id}");
        assert_ne!(id, new_change_id("abc"));
    }

    /// The hook gives a commit a Change-Id once, and leaves one that has it alone.
    #[cfg(unix)]
    #[test]
    fn w14_r03_commit_msg_hook() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let d = dir.path().to_string_lossy().into_owned();
        let g = |args: &[&str]| -> Result<String> {
            let o = Command::new("git").args(["-C", &d, "-c", "user.name=T", "-c", "user.email=t@x", "-c", "commit.gpgsign=false"]).args(args).env("GIT_CONFIG_NOSYSTEM", "1").env("HOME", &d).output()?;
            anyhow::ensure!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
            Ok(String::from_utf8_lossy(&o.stdout).into_owned())
        };
        g(&["init", "-q"])?;
        install_hook(Some(&d))?;
        std::fs::write(dir.path().join("a"), "1")?;
        g(&["add", "a"])?;
        g(&["commit", "-qm", "one: a"])?;
        let m = g(&["log", "-1", "--format=%B"])?;
        let id = kiln_ci::review::change_id(&m);
        assert!(id.is_some(), "{m}");
        g(&["commit", "-q", "--amend", "--no-edit"])?;
        assert_eq!(kiln_ci::review::change_id(&g(&["log", "-1", "--format=%B"])?), id, "an amend keeps it");
        Ok(())
    }

    /// Each edit goes into the commit that last touched its lines (an insertion, into the one both
    /// its neighbours come from); edits of the trunk's lines stay in the working copy.
    #[cfg(unix)]
    #[test]
    fn w14_r03_absorb() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let d = dir.path().to_string_lossy().into_owned();
        let g = |args: &[&str]| -> Result<String> {
            let o = Command::new("git").args(["-C", &d, "-c", "commit.gpgsign=false"]).args(args).env("GIT_CONFIG_NOSYSTEM", "1").env("HOME", &d).output()?;
            anyhow::ensure!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
            Ok(String::from_utf8_lossy(&o.stdout).into_owned())
        };
        let write = |name: &str, body: &str| std::fs::write(dir.path().join(name), body);
        g(&["init", "-q"])?;
        g(&["config", "user.name", "T"])?;
        g(&["config", "user.email", "t@x"])?;
        write("a", "1\n2\n3\n")?;
        g(&["add", "a"])?;
        g(&["commit", "-qm", "trunk"])?;
        let base = g(&["rev-parse", "HEAD"])?.trim().to_owned();
        write("a", "1\n2\n3\nfour\nfive\n")?;
        g(&["commit", "-qam", "one: four and five"])?;
        write("a", "1\n2\n3\nfour\nfive\nsix\n")?;
        write("b", "x\ny\n")?;
        g(&["add", "b"])?;
        g(&["commit", "-qam", "two: six and b"])?;
        // `2` is the trunk's, `four` one's, `six` and the line between x and y two's.
        write("a", "1\nTWO\n3\nFOUR\nfive\nSIX\n")?;
        write("b", "x\nbetween\ny\n")?;
        let was = g(&["rev-parse", "HEAD"])?.trim().to_owned();

        let plan = absorb_in(dir.path(), &base, true)?;
        assert!(plan.now.is_none());
        assert_eq!(g(&["rev-parse", "HEAD"])?.trim(), was, "a dry run changes nothing");
        let r = absorb_in(dir.path(), &base, false)?;
        let went: Vec<(String, usize, Option<String>)> = r.edits.iter().map(|(e, into)| (e.path.clone(), e.line, into.as_ref().ok().map(|sha| g(&["log", "-1", "--format=%s", sha]).unwrap_or_default().trim().to_owned()))).collect();
        assert_eq!(
            went,
            vec![("a".into(), 2, None), ("a".into(), 4, Some("one: four and five".into())), ("a".into(), 6, Some("two: six and b".into())), ("b".into(), 2, Some("two: six and b".into()))],
            "{:?}",
            r.edits.iter().map(|(_, into)| into.clone()).collect::<Vec<_>>()
        );
        assert_eq!(r.edits.first().and_then(|(_, into)| into.clone().err()).as_deref(), Some("changes lines from before the stack"));
        assert_eq!(g(&["log", "--format=%s", &format!("{base}..HEAD")])?, "two: six and b\none: four and five\n", "the messages and the stack's shape are kept");
        assert_eq!(g(&["show", "HEAD~1:a"])?, "1\n2\n3\nFOUR\nfive\n");
        assert_eq!(g(&["show", "HEAD:a"])?, "1\n2\n3\nFOUR\nfive\nSIX\n");
        assert_eq!(g(&["show", "HEAD:b"])?, "x\nbetween\ny\n");
        assert_eq!(g(&["show", "HEAD~1:b"]).ok(), None, "b is two's");
        assert_eq!(g(&["diff", "HEAD", "-U0", "--no-color"])?.lines().filter(|l| l.starts_with(['-', '+']) && !l.starts_with("---") && !l.starts_with("+++")).collect::<Vec<_>>(), vec!["-2", "+TWO"], "only the trunk's edit stays");
        assert_eq!(g(&["diff", "--cached", "--name-only"])?, "", "nothing is left staged");
        assert_eq!(std::fs::read_to_string(dir.path().join("a"))?, "1\nTWO\n3\nFOUR\nfive\nSIX\n", "the working copy is untouched");
        Ok(())
    }
}
