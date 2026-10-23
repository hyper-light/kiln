//! Fault injection (docs/ci.md W22-AC04), with a server built with the `chaos` feature: its own
//! runner drops its connection and sends late, kills steps as the OOM killer does, fills a job
//! container's disk and corrupts uploads. W05's retries and classes, W08's integrity checks and
//! W06's cleanup do what they say under each.
//!
//! `cargo test -p kiln --features chaos --test chaos`

#![cfg(all(unix, feature = "chaos"))]

mod common;

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use common::{Options, Server, http, start_env};
use serde_json::Value;

fn git(dir: &Path, args: &[&str]) -> anyhow::Result<()> {
    let o = Command::new("git").arg("-C").arg(dir).args(args).env("GIT_AUTHOR_NAME", "T").env("GIT_AUTHOR_EMAIL", "t@example.com").env("GIT_COMMITTER_NAME", "T").env("GIT_COMMITTER_EMAIL", "t@example.com").output()?;
    anyhow::ensure!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    Ok(())
}

fn docker_works() -> bool {
    Command::new("docker").args(["info", "--format", "{{.ServerVersion}}"]).output().is_ok_and(|o| o.status.success())
}

/// A server whose runner injects `chaos`, the run of `workflow` on it once finished: its id and
/// legs.
fn run(dir: &Path, chaos: &str, workflow: &str) -> anyhow::Result<(Server, String, Vec<Value>)> {
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo)?;
    std::fs::write(repo.join("README.md"), "chaos\n")?;
    git(&repo, &["init", "-q", "-b", "main"])?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-qm", "start"])?;
    let extra = format!("[[repos]]\nname = \"acme/chaos\"\nclone_url = \"file://{}\"\nbranches = [\"main\"]\npoll_seconds = 1\n", repo.display());
    let s = start_env(dir, &Options { agent: true, ci: "kiln_workflows = true", extra: &extra }, false, &[("KILN_CHAOS", chaos)])?;
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        if body.contains("acme/chaos") && !body.contains("\"last_polled_at\":null") {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "never polled: {body}");
        std::thread::sleep(Duration::from_millis(300));
    }
    std::fs::create_dir_all(repo.join(".kiln"))?;
    std::fs::write(repo.join(".kiln/chaos.yaml"), workflow)?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-qm", "chaos"])?;
    let deadline = Instant::now() + Duration::from_secs(600);
    loop {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/chaos&limit=5", &s.admin, None)?;
        let runs: Value = serde_json::from_str(&body).unwrap_or_default();
        if let Some(id) = runs.pointer("/runs/0/id").and_then(Value::as_str)
            && runs.pointer("/runs/0/finished_at").is_some_and(|f| !f.is_null())
        {
            let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{id}"), &s.admin, None)?;
            let legs = serde_json::from_str::<Value>(&detail)?.get("legs").and_then(Value::as_array).cloned().unwrap_or_default();
            return Ok((s, id.to_owned(), legs));
        }
        anyhow::ensure!(Instant::now() < deadline, "the run didn't finish: {body}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn leg<'a>(legs: &'a [Value], key: &str) -> Option<&'a Value> {
    legs.iter().find(|l| l.get("key").and_then(Value::as_str) == Some(key))
}

/// A leg's (conclusion, attempt).
fn state(legs: &[Value], key: &str) -> (String, i64) {
    let l = leg(legs, key);
    (l.and_then(|l| l.get("conclusion")).and_then(Value::as_str).unwrap_or_default().to_owned(), l.and_then(|l| l.get("attempt")).and_then(Value::as_i64).unwrap_or(0))
}

/// A leg's log, every step's.
fn log(s: &Server, legs: &[Value], key: &str) -> anyhow::Result<String> {
    let id = leg(legs, key).and_then(|l| l.get("id")).and_then(Value::as_str).unwrap_or_default().to_owned();
    let mut all = String::new();
    for step in 0..4 {
        let (_, text) = http(s.port, "GET", &format!("/api/ci/legs/{id}/logs?step={step}"), &s.admin, None)?;
        all.push_str(&text);
    }
    Ok(all)
}

/// Dropped every 3 s and every frame 200 ms late, the runner reconnects and the leg goes on: it
/// isn't lost or retried, its log arrives whole, and its output reaches the job after it.
#[test]
fn w22_ac04_dropped_and_late() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let dir = tmp.path().to_path_buf();
    // KILN_CHAOS_KEEP: the server's directory stays, to read its whole log.
    let _tmp = if std::env::var_os("KILN_CHAOS_KEEP").is_some() {
        eprintln!("keeping {}", tmp.keep().display());
        None
    } else {
        Some(tmp)
    };
    let workflow = r#"on: push
jobs:
  talk:
    runs-on: local
    outputs:
      said: ${{ steps.s.outputs.said }}
    steps:
      - id: s
        run: |
          for i in $(seq 1 40); do echo "line $i of 40"; sleep 0.25; done
          echo "said=forty" >> "$GITHUB_OUTPUT"
  after:
    needs: talk
    runs-on: local
    steps:
      - run: |
          echo "heard ${{ needs.talk.outputs.said }}"
          test "${{ needs.talk.outputs.said }}" = forty
"#;
    let (s, _, legs) = run(&dir, "drop=3s,delay=200ms", workflow)?;
    assert_eq!(state(&legs, "talk"), ("success".to_owned(), 1), "a dropped connection loses nothing\n{legs:?}\n{}", s.log());
    let said = log(&s, &legs, "talk")?;
    let missing: Vec<u32> = (1..=40).filter(|i| !said.contains(&format!("line {i} of 40"))).collect();
    assert!(missing.is_empty(), "lines lost: {missing:?}\n{said}\n{}", s.log());
    assert_eq!(state(&legs, "after"), ("success".to_owned(), 1), "the output arrived\n{}\n{}", log(&s, &legs, "after")?, s.log());
    assert!(s.log().contains("chaos: dropping the connection"), "the connection was dropped\n{}", s.log());
    Ok(())
}

/// The artifact upload of `upload`, by hand (the toolkit's calls): create, put the blob, finalize
/// with the content's hash.
const UPLOAD: &str = r#"
import base64, hashlib, json, os, sys, urllib.error, urllib.parse, urllib.request
token = os.environ["ACTIONS_RUNTIME_TOKEN"]
claims = token.split(".")[1]
claims += "=" * (-len(claims) % 4)
scp = json.loads(base64.urlsafe_b64decode(claims))["scp"]
_, run, job = next(s for s in scp.split() if s.startswith("Actions.Results:")).split(":")
u = urllib.parse.urlsplit(os.environ["ACTIONS_RESULTS_URL"])
base = f"{u.scheme}://{u.netloc}/twirp/github.actions.results.api.v1.ArtifactService/"
ids = {"workflow_run_backend_id": run, "workflow_job_run_backend_id": job, "name": "payload"}
def twirp(method, body):
    req = urllib.request.Request(base + method, json.dumps(body).encode(), {"Authorization": "Bearer " + token, "Content-Type": "application/json"})
    try:
        return json.load(urllib.request.urlopen(req))
    except urllib.error.HTTPError as e:
        sys.exit(f"{method}: {e.read().decode()}")
data = b"kiln chaos\n" * 4096
url = twirp("CreateArtifact", dict(ids, version=4))["signed_upload_url"]
urllib.request.urlopen(urllib.request.Request(url, data, {"x-ms-blob-type": "BlockBlob"}, method="PUT"))
done = twirp("FinalizeArtifact", dict(ids, size=str(len(data)), hash="sha256:" + hashlib.sha256(data).hexdigest()))
print("uploaded", done["artifact_id"], "on attempt", os.environ["KILN_ATTEMPT"])
"#;

/// Killed as the OOM killer kills, a full disk in a job container, a corrupted upload: each first
/// attempt fails as its kind, the policy that names the kind retries it, and nothing of the
/// failed attempt is left (its processes, its container, its artifact).
#[test]
fn w22_ac04_oom_disk_and_corrupt_uploads() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let docker = docker_works();
    let mut workflow = format!(
        r#"on: push
jobs:
  oom:
    runs-on: local
    retry:
      max: 1
      on: [oom]
      backoff: {{ initial: 1s }}
    steps:
      - run: |
          if [ "$KILN_ATTEMPT" = 1 ]; then sleep 471; fi
          echo "oom passed on attempt $KILN_ATTEMPT"
  upload:
    runs-on: local
    retry:
      max: 1
      on: [{{ log: "the artifact's content hashes to" }}]
      backoff: {{ initial: 1s }}
    steps:
      - run: python3 -c "$UPLOAD"
        env:
          UPLOAD: {upload}
"#,
        upload = serde_json::to_string(UPLOAD)?,
    );
    if docker {
        workflow.push_str(
            r#"  disk:
    runs-on: local
    container: alpine:3.20
    retry:
      max: 1
      on: [disk-full]
      backoff: { initial: 1s }
    steps:
      - run: |
          dd if=/dev/zero of=big bs=1M count=32
          echo "disk passed on attempt $KILN_ATTEMPT"
"#,
        );
    }
    let (s, run_id, legs) = run(dir.path(), "oom=3s,disk=8,corrupt", &workflow)?;
    // W05: the OOM killer's kill is `oom`, retried by the policy that names it.
    assert_eq!(state(&legs, "oom"), ("success".to_owned(), 2), "killed as by the OOM killer: retried\n{legs:?}\n{}", s.log());
    assert!(log(&s, &legs, "oom")?.contains("oom passed on attempt 2"));
    // W06: the killed step's processes went with it.
    let left = Command::new("pgrep").args(["-f", "sleep 471"]).output()?;
    assert!(left.stdout.is_empty(), "the killed step left processes: {}", String::from_utf8_lossy(&left.stdout));
    // W08: the corrupted upload doesn't finalize; the retry's does, and it's the only artifact.
    assert_eq!(state(&legs, "upload"), ("success".to_owned(), 2), "a corrupted upload fails its integrity check, retried\n{legs:?}\n{}", s.log());
    assert!(log(&s, &legs, "upload")?.contains("on attempt 2"));
    let (_, listed) = http(s.port, "GET", &format!("/api/ci/runs/{run_id}/artifacts"), &s.admin, None)?;
    let want = format!("sha256:{}", kiln::util::sha256_hex("kiln chaos\n".repeat(4096).as_bytes()));
    assert!(listed.contains(&want) && listed.matches("\"payload\"").count() == 1, "one artifact, the uncorrupted one ({want}): {listed}");
    if docker {
        // W05: ENOSPC in the job container is `disk-full`, retried.
        assert_eq!(state(&legs, "disk"), ("success".to_owned(), 2), "a full disk: retried\n{}\n{}", log(&s, &legs, "disk")?, s.log());
        assert!(log(&s, &legs, "disk")?.contains("disk passed on attempt 2"));
        // W06: no container of the leg is left.
        let id = leg(&legs, "disk").and_then(|l| l.get("id")).and_then(Value::as_str).unwrap_or_default();
        let left = Command::new("docker").args(["ps", "-aq", "--filter", &format!("label=kiln.leg={id}")]).output()?;
        assert!(left.stdout.is_empty(), "the leg's containers are left: {}", String::from_utf8_lossy(&left.stdout));
    } else {
        eprintln!("w22_ac04_oom_disk_and_corrupt_uploads: no Docker, so no full disk");
    }
    Ok(())
}

/// A runner of its own, apart from the server, so it runs on through the server's restart.
struct Keeper(std::process::Child);

impl Drop for Keeper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `run`'s legs: `(key, status, attempt, id)` each.
fn legs_of(s: &Server, run: &str) -> anyhow::Result<Vec<(String, String, i64, String)>> {
    let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{run}"), &s.admin, None)?;
    let legs = serde_json::from_str::<Value>(&detail)?.get("legs").and_then(Value::as_array).cloned().unwrap_or_default();
    let text = |l: &Value, k: &str| l.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    Ok(legs.iter().map(|l| (text(l, "key"), text(l, "status"), l.get("attempt").and_then(Value::as_i64).unwrap_or(0), text(l, "id"))).collect())
}

/// Until `f` says it's done (`None`), else what it last saw.
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

/// The newest run of `workflow` in `acme/chaos`, once there is one.
fn newest(s: &Server, workflow: &str) -> anyhow::Result<String> {
    let mut run = String::new();
    until(s, 120, "the run", || {
        let (_, body) = http(s.port, "GET", &format!("/api/ci/runs?repo=acme/chaos&workflow={workflow}&limit=1"), &s.admin, None)?;
        run = serde_json::from_str::<Value>(&body)?.pointer("/runs/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
        Ok(run.is_empty().then_some(body))
    })?;
    Ok(run)
}

/// W05-R05 with the CI engine's rebuild after a restart late (`rebuild=10s`), so both runners say
/// hello before it knows their legs. A runner apart from the server ran on through the restart:
/// its leg is re-adopted, not cancelled as a job nobody knows, and finishes on its first attempt.
/// The server's own runner went with it: its leg was lost (`runner-lost`), and its next attempt
/// ran there, the only runner that could take it (W05-R16's default), so its next leg runs.
#[test]
fn w05_r05_hellos_before_the_rebuild() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let dir = tmp.path();
    let repo = dir.join("repo");
    std::fs::create_dir_all(repo.join(".kiln"))?;
    std::fs::write(repo.join("README.md"), "restart\n")?;
    git(&repo, &["init", "-q", "-b", "main"])?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-qm", "start"])?;
    let extra = format!("[[repos]]\nname = \"acme/chaos\"\nclone_url = \"file://{}\"\nbranches = [\"main\"]\npoll_seconds = 1\n", repo.display());
    let mut s = start_env(dir, &Options { agent: true, ci: "kiln_workflows = true", extra: &extra }, false, &[("KILN_CHAOS", "rebuild=10s")])?;
    let join = std::fs::read_to_string(dir.join("data/join.token"))?.trim().to_owned();
    std::fs::create_dir_all(dir.join("keeper-home"))?;
    let out = std::fs::File::create(dir.join("keeper.log"))?;
    let _keeper = Keeper(
        Command::new(env!("CARGO_BIN_EXE_kiln"))
            .args(["agent", "--server", &format!("http://127.0.0.1:{}", s.port), "--token", &join, "--name", "keeper", "--labels", "keep", "--slots", "1", "--no-terminal"])
            .arg("--work-dir")
            .arg(dir.join("keeper-work"))
            .arg("--state-dir")
            .arg(dir.join("keeper-state"))
            .env("HOME", dir.join("keeper-home"))
            .env("NO_COLOR", "1")
            .env_remove("KILN_CONFIG")
            .env_remove("KILN_CHAOS")
            .stdout(std::process::Stdio::from(out.try_clone()?))
            .stderr(std::process::Stdio::from(out))
            .spawn()?,
    );
    let keeper_log = || std::fs::read_to_string(dir.join("keeper.log")).unwrap_or_default();
    until(&s, 120, "the repository polled and the keeper connected", || {
        let (_, repos) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        let (_, agents) = http(s.port, "GET", "/api/agents", &s.admin, None)?;
        Ok((repos.contains("\"last_polled_at\":null") || !agents.contains("\"keeper\"")).then(|| format!("{repos}\n{agents}\n{}", keeper_log())))
    })?;
    std::fs::write(
        repo.join(".kiln/both.yaml"),
        "on: push\njobs:\n  kept:\n    runs-on: keep\n    steps:\n      - run: sleep 30; echo \"kept to its end\"\n  lost:\n    runs-on: local\n    steps:\n      - run: if [ \"$KILN_ATTEMPT\" = 1 ]; then sleep 300; fi\n",
    )?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-qm", "both"])?;
    let run = newest(&s, ".kiln/both.yaml")?;
    // Both running, the kept one's step on the keeper.
    until(&s, 300, "both legs running", || {
        let all = legs_of(&s, &run)?;
        let kept = all.iter().find(|l| l.0 == "kept").map(|l| l.3.clone()).unwrap_or_default();
        let (_, about) = http(s.port, "GET", &format!("/api/ci/legs/{kept}"), &s.admin, None)?;
        let stepping = serde_json::from_str::<Value>(&about)
            .unwrap_or_default()
            .get("steps")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(|x| x.get("name").and_then(Value::as_str).is_some_and(|n| n.starts_with("Run sleep")) && x.get("status").and_then(Value::as_str) == Some("running"));
        Ok((!stepping || all.iter().filter(|l| l.1 == "running").count() < 2).then(|| format!("{all:?}")))
    })?;
    s.restart()?;
    // The kept leg finishes where it ran, its first attempt.
    until(&s, 180, "the kept leg to finish", || {
        let all = legs_of(&s, &run)?;
        let kept = all.iter().find(|l| l.0 == "kept").cloned().unwrap_or_default();
        anyhow::ensure!(kept.1 == "running" || kept.1 == "succeeded", "the kept leg ended {kept:?}\n--- the keeper:\n{}\n--- the server:\n{}", keeper_log(), s.log());
        Ok((kept.1 != "succeeded").then(|| format!("{all:?}")))
    })?;
    let all = legs_of(&s, &run)?;
    let kept = all.iter().find(|l| l.0 == "kept").cloned().unwrap_or_default();
    assert_eq!(kept.2, 1, "re-adopted, not retried: {all:?}");
    let (_, text) = http(s.port, "GET", &format!("/api/ci/legs/{}/logs?step=0", kept.3), &s.admin, None)?;
    assert!(text.contains("kept to its end"), "it ran to its end: {text}");
    // The lost one: its first attempt `runner-lost`, its next on the runner that lost it, the
    // only one that can take it.
    until(&s, 120, "the lost leg's next attempt", || {
        let lost = legs_of(&s, &run)?.into_iter().find(|l| l.0 == "lost").unwrap_or_default();
        Ok((lost.2 < 2 || lost.1 != "succeeded").then(|| format!("{lost:?}")))
    })?;
    let lost = legs_of(&s, &run)?.into_iter().find(|l| l.0 == "lost").unwrap_or_default();
    let (_, about) = http(s.port, "GET", &format!("/api/ci/legs/{}", lost.3), &s.admin, None)?;
    let about: Value = serde_json::from_str(&about)?;
    let first = about.get("attempts").and_then(Value::as_array).and_then(|a| a.iter().find(|x| x.get("number").and_then(Value::as_i64) == Some(1))).cloned().unwrap_or_default();
    assert_eq!((first.get("conclusion").and_then(Value::as_str), first.get("class").and_then(Value::as_str)), (Some("failure"), Some("runner-lost")), "{about}\n{}", s.log());
    // None of the server's runner's slots held: its next leg runs.
    std::fs::write(repo.join(".kiln/after.yaml"), "on: push\njobs:\n  after:\n    runs-on: local\n    steps:\n      - run: echo after\n")?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-qm", "after"])?;
    let after = newest(&s, ".kiln/after.yaml")?;
    until(&s, 120, "the next leg on the server's runner", || {
        let all = legs_of(&s, &after)?;
        Ok((!all.iter().any(|l| l.1 == "succeeded")).then(|| format!("{all:?}")))
    })?;
    let (st, body) = http(s.port, "POST", &format!("/api/ci/runs/{run}/cancel"), &s.admin, Some(&serde_json::json!({})))?;
    anyhow::ensure!((200..300).contains(&st), "cancel: {st} {body}");
    Ok(())
}
