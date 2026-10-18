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
}
