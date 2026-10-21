//! W14-R11 (docs/ci.md): `gh` against the facade, end to end. GitHub's CLI 2.102.0 (the release
//! kiln's runner image installs), fetched once into the build's temporary directory and checked
//! against its release's checksums, runs in a pull request's job on the host executor, given the
//! job's token as GitHub Enterprise Server's (`GH_HOST`, `GH_ENTERPRISE_TOKEN`) over HTTPS. Skipped
//! when the release can't be fetched.

#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use common::{Options, Server, http};
use serde_json::{Value, json};

const GH: &str = "2.102.0";

fn git(home: &Path, dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let o = Command::new("git")
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com", "-c", "init.defaultBranch=main", "-c", "credential.helper="])
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()?;
    let out = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    anyhow::ensure!(o.status.success(), "git {args:?}: {out}");
    Ok(out)
}

fn until(s: &Server, secs: u64, what: &str, mut f: impl FnMut() -> anyhow::Result<Option<String>>) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        match f()? {
            None => return Ok(()),
            Some(last) => anyhow::ensure!(Instant::now() < deadline, "{what}: {last}\n{}", s.log()),
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// `gh`, fetched once (its release's tarball, checked against the release's checksums), or `None`
/// when it can't be.
fn gh() -> Option<PathBuf> {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("gh-{GH}"));
    let bin = dir.join("gh");
    if bin.is_file() {
        return Some(bin);
    }
    let base = format!("https://github.com/cli/cli/releases/download/v{GH}");
    let file = format!("gh_{GH}_linux_amd64.tar.gz");
    let fetch = |url: &str| Command::new("curl").args(["-fsSL", "--max-time", "120", url]).output().ok().filter(|o| o.status.success()).map(|o| o.stdout);
    let sums = String::from_utf8(fetch(&format!("{base}/gh_{GH}_checksums.txt"))?).ok()?;
    let want = sums.lines().find(|l| l.ends_with(&format!("  {file}")))?.split_whitespace().next()?.to_owned();
    let tarball = fetch(&format!("{base}/{file}"))?;
    let got = {
        use sha2::Digest as _;
        hex::encode(sha2::Sha256::digest(&tarball))
    };
    if got != want {
        return None;
    }
    // Unpacked in a directory of its own, then only the binary kept.
    let unpack = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).ok()?;
    let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(tarball.as_slice()));
    ar.unpack(unpack.path()).ok()?;
    std::fs::create_dir_all(&dir).ok()?;
    std::fs::copy(unpack.path().join(format!("gh_{GH}_linux_amd64/bin/gh")), &bin).ok()?;
    Some(bin)
}

/// The job's script: each answer on a line of its own, `name: …`.
const SCRIPT: &str = r#"set -e
R="--repo $GITHUB_REPOSITORY"
echo "version: $(gh --version | head -1)"
echo "repo: $(gh api "repos/$GITHUB_REPOSITORY" --jq .full_name)"
echo "view: $(gh pr view "$N" $R --json number,title,state --jq '[.number, .title, .state] | join(",")')"
echo "list: $(gh pr list $R --state open --json number --jq 'map(.number) | join(",")')"
echo "search: $(gh pr list $R --search 'author:dev' --json number --jq 'map(.number) | join(",")')"
gh pr comment "$N" $R --body "from gh" > /dev/null
echo "comments: $(gh pr view "$N" $R --json comments --jq '.comments | map(.body) | join(",")')"
gh api -X POST "repos/$GITHUB_REPOSITORY/issues/$N/labels" -f 'labels[]=bug' -f 'labels[]=docs' > /dev/null
gh pr edit "$N" $R --remove-label docs > /dev/null
gh pr edit "$N" $R --add-label bug > /dev/null
echo "labels: $(gh pr view "$N" $R --json labels --jq '.labels | map(.name) | join(",")')"
echo "status: $(gh pr checks "$N" $R 2>&1 | head -1 || true)"
echo done
"#;

/// `gh` asking the facade what workflows ask it: the repository, a pull request (by number, in a
/// list, by search), a comment on it and a label, all as the job's token.
#[test]
fn w14_r11_gh_against_the_facade() -> anyhow::Result<()> {
    let Some(gh) = gh() else {
        eprintln!("skipped: GitHub's CLI {GH} couldn't be fetched");
        return Ok(());
    };
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))?;
    let s = common::start_with(dir.path(), &Options { agent: true, ..Options::default() }, true)?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    // Set up over plain HTTP (the same port answers both); the job's `gh` goes over HTTPS.
    let url = |token: &str| format!("http://x:{token}@127.0.0.1:{}/git/acme/app.git", s.port);
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "native repository: {st} {body}");
    let (st, body) = http(s.port, "POST", "/api/users", &s.admin, Some(&json!({ "name": "dev", "grants": ["developer@repo:acme/app"] })))?;
    anyhow::ensure!((200..300).contains(&st), "{st} {body}");
    let (st, body) = http(s.port, "POST", "/api/tokens", &s.admin, Some(&json!({ "kind": "pat", "user": "dev", "grants": ["owner"] })))?;
    anyhow::ensure!((200..300).contains(&st), "{st} {body}");
    let dev = serde_json::from_str::<Value>(&body)?.get("token").and_then(Value::as_str).unwrap_or_default().to_owned();
    git(&home, dir.path(), &["clone", "-q", &url(&dev), "work"])?;
    let work = dir.path().join("work");
    std::fs::create_dir_all(work.join(".github/workflows"))?;
    let workflow = format!(
        "on: pull_request\npermissions:\n  contents: read\n  pull-requests: write\n  issues: write\n  checks: read\n  statuses: read\njobs:\n  gh:\n    runs-on: local\n    steps:\n      - env:\n          GH_TOKEN: ${{{{ github.token }}}}\n          N: ${{{{ github.event.pull_request.number }}}}\n        run: |\n          export PATH=\"{}:$PATH\"\n{}",
        gh.parent().map(Path::display).map(|d| d.to_string()).unwrap_or_default(),
        SCRIPT.lines().map(|l| format!("          {l}\n")).collect::<String>()
    );
    std::fs::write(work.join(".github/workflows/gh.yml"), workflow)?;
    std::fs::write(work.join("README.md"), "app\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", "origin", "main"])?;
    until(&s, 60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(body.contains("\"last_polled_at\":null").then_some(body))
    })?;
    std::fs::write(work.join("a.txt"), "one\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "one: a\n\nChange-Id: I1111111111111111111111111111111111111111"])?;
    git(&home, &work, &["push", "-q", "origin", "HEAD:refs/for/main"])?;
    let mut leg = String::new();
    until(&s, 300, "the pull request's run", || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/app&limit=10", &s.admin, None)?;
        let runs = serde_json::from_str::<Value>(&body)?.get("runs").and_then(Value::as_array).cloned().unwrap_or_default();
        let Some(run) = runs.iter().find(|r| r.get("event").and_then(Value::as_str) == Some("pull_request") && r.get("finished_at").is_some_and(|f| !f.is_null())) else { return Ok(Some(body)) };
        let id = run.get("id").and_then(Value::as_str).unwrap_or_default();
        let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{id}"), &s.admin, None)?;
        leg = serde_json::from_str::<Value>(&detail)?.pointer("/legs/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
        Ok(leg.is_empty().then_some(detail))
    })?;
    let (_, about) = http(s.port, "GET", &format!("/api/ci/legs/{leg}"), &s.admin, None)?;
    let (_, log) = http(s.port, "GET", &format!("/api/ci/legs/{leg}/logs?step=0"), &s.admin, None)?;
    let said = |name: &str| log.lines().find_map(|l| l.split_once(&format!("{name}: ")).map(|(_, v)| v.trim().to_owned())).unwrap_or_default();
    assert!(log.contains("done"), "every command answered: {about}\n{log}\n{}", s.log());
    assert!(said("version").contains(GH), "{log}");
    assert_eq!(said("repo"), "acme/app", "{log}");
    assert_eq!(said("view"), "1,one: a,OPEN", "{log}");
    assert_eq!(said("list"), "1", "{log}");
    assert_eq!(said("search"), "1", "{log}");
    assert_eq!(said("comments"), "from gh", "{log}");
    assert_eq!(said("labels"), "bug", "{log}");
    Ok(())
}
