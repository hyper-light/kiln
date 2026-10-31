//! U03: the legacy assignment wire adapter and the runner's upstream transport. All jobs use
//! `runner::runtime::Leg` for their process tree, steps, masking, cancellation and cleanup. Only
//! the old server's source/artifact endpoints and event spelling remain here.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use kiln_ci::wire::{AssignmentV2, Contexts, ExecutorSpec, JobEventV2, LegInfo, Runtime, Secret};
use crate::proto::{AgentMsg, Assignment, JobResult, LogChunk};
use crate::rt::Shutdown;
use crate::store::Status;

/// What the executor sends upstream; the agent's uplink buffers it across reconnects.
#[derive(Debug)]
pub enum Up {
    Msg(AgentMsg),
    Log(LogChunk),
    /// Interactive terminal traffic: never buffered across a disconnect.
    Term(AgentMsg),
    /// Repo sync, on its own lane: never buffered across a disconnect (it says it all again).
    Sync(AgentMsg),
    /// Reach links (`crate::reach`), on their own lane: never buffered across a disconnect (a link
    /// does not outlive the connection; its runs resume on the next one).
    Reach(AgentMsg),
    /// The observe group (`crate::observe`), on its own lane: never buffered across a disconnect
    /// (Raft sends again, and a write is sent again until answered).
    Observe(AgentMsg),
    /// The conversation feed (`crate::convo::feed`), on its own lane: never buffered across a
    /// disconnect (the feed goes on from what the server says it has, on the next connection).
    Convo(AgentMsg),
    /// Files read for the people viewing an agent session (`crate::harness::files`), on their own
    /// lane: never buffered across a disconnect (whoever asked has given up by then).
    File(AgentMsg),
    /// A streamed page's chunks and events (`crate::browser::stream`), on their own lane: never
    /// buffered across a disconnect (a viewer wants what's now, from the next key frame).
    Stream(AgentMsg),
    /// Connections to the machine's ports (`crate::browser::ports`), on their own lane: never
    /// buffered across a disconnect (a connection doesn't outlive it).
    Port(AgentMsg),
}

#[derive(Clone)]
pub struct Uplink(pub async_channel::Sender<Up>);

impl Uplink {
    pub async fn msg(&self, m: AgentMsg) {
        let _ = self.0.send(Up::Msg(m)).await;
    }

    /// A message that's only said along the way (an upgrade's progress): sent without waiting, so
    /// what's doing it never waits on the connection; dropped when the uplink is full.
    pub fn say(&self, m: AgentMsg) {
        let _ = self.0.try_send(Up::Msg(m));
    }

    pub async fn log(&self, job: &str, idx: u32, data: Vec<u8>) {
        let _ = self.0.send(Up::Log(LogChunk { job: job.to_owned(), idx, data })).await;
    }

    pub async fn note(&self, job: &str, idx: u32, text: &str) {
        self.log(job, idx, format!("\x1b[2m[kiln] {text}\x1b[0m\r\n").into_bytes()).await;
    }
}

pub struct Exec {
    pub assign: Assignment,
    pub work_dir: PathBuf,
    pub agent_name: String,
    pub up: Uplink,
    pub cancel: Shutdown,
    /// HTTP(S) client for the server's source-archive and artifact endpoints.
    pub http: Arc<crate::http::client::Client>,
}

fn have(bin: &str) -> bool { crate::os::have(bin) }

fn slug(s: &str) -> String { s.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect() }

async fn fs<R: Send + 'static>(f: impl FnOnce() -> std::io::Result<R> + Send + 'static) -> anyhow::Result<R> {
    Ok(crate::rt::blocking(f).await??)
}

/// The accepted job's canonical process owner retires every Git subtree before returning.
/// Mirror commands lend the SAME File custody; workspace reads lend their pinned selection.
async fn git_owned<H: Send + 'static>(command: Command, custody: H) -> anyhow::Result<Vec<u8>> {
    let out = crate::proc::output_with_custody(command, Duration::from_secs(1800), custody).await?;
    if !out.status.success() {
        return Err(kiln_cd::git::GitExit::new(out.status, crate::util::truncate(String::from_utf8_lossy(&out.stderr).trim(), 600).to_owned()).into());
    }
    Ok(out.stdout)
}

/// U08: only a known missing exact-commit lookup permits one targeted fetch. A local pin
/// failure or unknown process outcome returns directly while the SAME operation stays owned.
async fn select_owned<F, Fut>(operation: &mut kiln_cd::git::OwnedOperation, sha: &str, mut execute: F) -> anyhow::Result<kiln_cd::git::SelectedCommit>
where
    F: FnMut(Command) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<Vec<u8>>>,
{
    match operation.retain_using(sha, &mut execute).await {
        Ok(selected) => Ok(selected),
        Err(error) if error.downcast_ref::<kiln_cd::git::MissingCommit>().is_some() => {
            operation.fetch_one_using(sha, &mut execute).await?;
            operation.retain_using(sha, &mut execute).await
        }
        Err(error) => Err(error),
    }
}

impl Exec {
    fn require_native_execution(&self) -> anyhow::Result<()> {
        let execution = crate::runner::processes::current().ok_or(crate::proc::CustodyRefusal::NoExecution)?;
        execution.require_native_attempt(&self.assign.job_id, self.assign.attempt.saturating_add(1))
    }

    pub async fn run(self) -> JobResult {
        // The receiver retains the accepted slot/resources and exact physical owner through
        // stop(). Do not let Leg::run manufacture a new unbudgeted scope for this adapter.
        if let Err(error) = self.require_native_execution() {
            return JobResult { job: self.assign.job_id.clone(), status: Status::Failed, exit_code: None,
                error: Some(format!("legacy native execution: {error:#}")), peak_rss: None, cpu_seconds: None };
        }
        self.up.msg(AgentMsg::JobStarted { job: self.assign.job_id.clone() }).await;
        let a = self.assignment();
        let legacy = Arc::new(self);
        let (tx, rx) = async_channel::bounded(128);
        let upstream = legacy.up.clone();
        let masks = a.secrets.iter().map(|s| s.value.clone()).chain(a.masks.iter().cloned()).collect::<Vec<_>>();
        let bridge = crate::rt::spawn(async move { legacy_events(rx, upstream, masks).await });
        let leg = crate::runner::runtime::Leg {
            a, work_dir: legacy.work_dir.clone(), runner_name: legacy.agent_name.clone(), up: Uplink(tx),
            cancel: legacy.cancel.clone(), http: Arc::clone(&legacy.http), warm: Default::default(), cache_max: None,
            legacy: Some(legacy),
        };
        let result = leg.run().await;
        let _ = bridge.await;
        result
    }

    fn assignment(&self) -> AssignmentV2 {
        let a = &self.assign;
        let names = a.secrets.iter().map(|(name, _)| name.clone()).collect::<Vec<_>>();
        let job = crate::ci::legacy::job(&a.spec, Status::Running, &names);
        let git_ref = format!("refs/{}/{}", if a.event == "tag" { "tags" } else { "heads" }, a.branch);
        let base = a.server_url.trim_end_matches('/');
        AssignmentV2 {
            job_id: a.job_id.clone(), run_id: a.run_id.clone(), attempt: a.attempt.saturating_add(1),
            repo: a.repo.clone(), sha: a.sha.clone(), git_ref: git_ref.clone(), dialect: "kiln".into(),
            leg: LegInfo { job_id: a.spec.spec_id.clone(), name: a.spec.display.clone(), total: 1, ..LegInfo::default() },
            executor: ExecutorSpec { kind: "host".into(), ..ExecutorSpec::default() },
            steps: job.steps, job_env: job.env, job_defaults: job.defaults,
            contexts: Contexts { github: serde_json::json!({
                "repository": a.repo, "sha": a.sha, "ref": git_ref, "ref_name": a.branch,
                "ref_type": if a.event == "tag" { "tag" } else { "branch" },
                "event_name": match a.event.as_str() { "manual" => "workflow_dispatch", "tag" => "push", other => other },
                "run_id": a.run_id, "run_number": a.run_number, "run_attempt": 1, "job": a.spec.key,
                "server_url": base, "api_url": format!("{base}/api/v3"), "event": {},
            }), ..Contexts::default() },
            secrets: a.secrets.iter().map(|(name, value)| Secret { name: name.clone(), value: value.clone() }).collect(),
            // v1 supplied every secret as an environment variable as well as masking it.
            env_from: [("KILN_ATTEMPT".into(), a.attempt.to_string()), ("KILN_JOB".into(), a.spec.key.clone())].into_iter().chain(a.secrets.iter().cloned()).collect(), masks: vec![a.job_token.clone()],
            timeout_minutes: a.spec.timeout_minutes.max(1), checkout: "auto".into(), trusted: true,
            runtime: Runtime { results_url: base.into(), runtime_token: a.job_token.clone(), facade_url: base.into(),
                api_url: format!("{base}/api/v3"), graphql_url: format!("{base}/api/graphql"), ..Runtime::default() },
            exclusive: a.spec.exclusive, holds: match crate::hub::v1_incoming(a.spec.exclusive, &a.spec.resources) {
                kiln_ci::sched::state::Incoming::Share(q) => Some(q), _ => None,
            }, ..AssignmentV2::default()
        }
    }

    /// Prefer a git checkout (keeps `.git` for tools that need history), using a per-repo bare
    /// mirror cache; fall back to the server's source tarball (no git or GitHub access needed).
    pub(crate) async fn checkout(&self, ws: &Path, temp: &Path) -> anyhow::Result<()> {
        // Refused/mismatched ownership is not an unavailable remote and must not fall through
        // to another checkout path. Inherited scopes need real delegated child authority.
        self.require_native_execution()?;
        let a = &self.assign;
        let job = &a.job_id;
        let w = ws.to_path_buf();
        fs(move || std::fs::create_dir_all(w)).await?;
        if have("git") && !a.clone_url.is_empty() {
            match self.git_checkout(ws).await {
                Ok(()) => return Ok(()),
                Err(error) if error.downcast_ref::<crate::proc::CustodyRefusal>().is_some() => return Err(error),
                Err(e) => {
                    let values = a.secrets.iter().map(|(_, v)| v.as_str()).chain(std::iter::once(a.job_token.as_str())).collect::<Vec<_>>();
                    let message = kiln_ci::runner::mask::Masker::new(&values).mask(&format!("git checkout failed ({e:#}); using server source archive"));
                    self.up.note(job, 0, &message).await;
                    let w = ws.to_path_buf();
                    let _ = fs(move || std::fs::remove_dir_all(w)).await;
                    let w = ws.to_path_buf();
                    fs(move || std::fs::create_dir_all(w)).await?;
                }
            }
        }
        self.tarball(ws, temp).await
    }

    async fn git(&self, args: &[&str], selected: &kiln_cd::git::SelectedCommit) -> anyhow::Result<()> {
        let mut cmd = Command::new("git");
        crate::os::git_no_prompts(cmd.args(args));
        git_owned(cmd, selected.clone()).await?;
        Ok(())
    }

    async fn git_checkout(&self, ws: &Path) -> anyhow::Result<()> {
        self.require_native_execution()?;
        // Reconcile the SAME durable owner before acquiring the mirror File. Each captured
        // command revalidates again after acquisition, before pending native submission.
        crate::proc::reconcile_custody_admission().await?;
        let a = &self.assign;
        anyhow::ensure!(matches!(a.sha.len(), 40 | 64) && a.sha.bytes().all(|b| b.is_ascii_hexdigit()), "legacy checkout requires a full commit SHA");
        let cache = std::env::var_os("KILN_GIT_CACHE").map(PathBuf::from).unwrap_or_else(|| self.work_dir.join("git"));
        let old = cache.join(format!("{}.git", slug(&a.repo)));
        // U08: a logical repository name is not a source identity. Keep legacy mirrors as
        // read-only imports, and separate upstream authorities/readers even when slugs collide.
        // Hash the assignment credential only as a cache boundary; never forward it to Git.
        let identity = crate::util::sha256_hex(&serde_json::to_vec(&(&a.server_url, &a.clone_url, &a.job_token))?);
        let canonical = cache.join("read").join(format!("{identity}.git"));
        let legacy = if old.join("kiln-runner-mirror").exists() { Vec::new() } else { vec![old.clone()] };
        // The v1 adapter has no explicit credentials: preserve its ambient Git authentication.
        // It must never borrow the host server's CI/CD credentials or the job's API token.
        let mut operation = kiln_cd::git::Mirror::open_import_owned_using(
            &canonical, &a.clone_url, kiln_cd::git::Auth::default(), kiln_cd::git::Options::default(), &legacy,
            git_owned,
        ).await?;
        let selected = {
            let custody = operation.custody();
            select_owned(&mut operation, &a.sha, |command| git_owned(command, custody.clone())).await?
        };
        // The requested commit is permanently pinned. Release the mutation fence before
        // workspace clone/submodules; those reads retain this exact captured selection.
        let mirror = operation.into_mirror()?;
        // U08: old warm discovery names become pointers, never additional mirror writers.
        // Publish only after the requested commit is available and retained.
        fs(move || {
            std::fs::create_dir_all(&old)?;
            let temp = old.join(format!("kiln-runner-mirror-{}", crate::util::new_id()));
            std::fs::write(&temp, identity)?;
            std::fs::rename(temp, old.join("kiln-runner-mirror"))
        }).await?;
        let m = mirror.dir.to_string_lossy().to_string();
        let w = ws.to_string_lossy().to_string();
        self.git(&["clone", "--quiet", "--shared", "--no-checkout", &m, &w], &selected).await?;
        self.git(&["-C", &w, "checkout", "--quiet", "--detach", selected.sha()], &selected).await?;
        self.git(&["-C", &w, "remote", "set-url", "origin", &a.clone_url], &selected).await?;
        if ws.join(".gitmodules").exists() {
            self.git(&["-C", &w, "submodule", "update", "--init", "--recursive", "--quiet"], &selected).await?;
        }
        Ok(())
    }

    /// Stream the server's `git archive` of the commit to disk, then unpack it.
    async fn tarball(&self, ws: &Path, temp: &Path) -> anyhow::Result<()> {
        let a = &self.assign;
        let url = format!("{}/api/jobs/{}/source", a.server_url.trim_end_matches('/'), a.job_id);
        let mut resp = self.http.get(&url).bearer(&a.job_token).timeout(Duration::from_secs(120)).send().await?;
        if !resp.is_success() {
            anyhow::bail!("source archive: HTTP {}", resp.status());
        }
        let tar_path = temp.join("source.tar");
        let (sink, done) = crate::rt::file_sink(tar_path.clone(), 16 << 30)?;
        while let Some(chunk) = resp.chunk().await? {
            if sink.send(chunk).await.is_err() {
                break;
            }
        }
        drop(sink);
        done.await?;
        let ws = ws.to_path_buf();
        fs(move || {
            let f = std::fs::File::open(&tar_path)?;
            tar::Archive::new(std::io::BufReader::new(f)).unpack(&ws)?;
            std::fs::remove_file(&tar_path)
        })
        .await
    }

    pub(crate) async fn upload_artifacts(&self, ws: &Path) -> anyhow::Result<usize> {
        let patterns = self.assign.spec.artifacts.clone();
        let root = ws.to_path_buf();
        let (count, bytes) = crate::rt::blocking(move || -> anyhow::Result<(usize, Vec<u8>)> {
            let mut builder = tar::Builder::new(Vec::new());
            let mut n = 0usize;
            for pat in &patterns {
                let full = root.join(pat).to_string_lossy().into_owned();
                for entry in glob::glob(&full)?.flatten() {
                    let Ok(rel) = entry.strip_prefix(&root) else { continue };
                    if entry.is_file() {
                        builder.append_path_with_name(&entry, rel)?;
                        n += 1;
                    } else if entry.is_dir() {
                        builder.append_dir_all(rel, &entry)?;
                        n += 1;
                    }
                }
            }
            Ok((n, builder.into_inner()?))
        })
        .await??;
        if count == 0 {
            return Ok(0);
        }
        let a = &self.assign;
        let url = format!("{}/api/jobs/{}/artifacts/upload", a.server_url.trim_end_matches('/'), a.job_id);
        let bytes = bytes::Bytes::from(bytes);
        // In pieces, where the server takes them so.
        if self.upload_pieces(&url, &bytes).await? {
            return Ok(count);
        }
        // Whole (an older server), tried again when it breaks off: three times in all.
        let mut last = None;
        for attempt in 0..3u32 {
            if attempt > 0 {
                crate::rt::sleep(Duration::from_secs(5 * u64::from(attempt))).await;
            }
            match self.http.put(&url).bearer(&a.job_token).body(bytes.clone()).timeout(Duration::from_secs(900)).send().await {
                Ok(resp) if resp.is_success() => return Ok(count),
                // Refused (its token, its size): no use trying again.
                Ok(resp) if resp.status() < 500 => anyhow::bail!("HTTP {}", resp.status()),
                Ok(resp) => last = Some(anyhow::anyhow!("HTTP {}", resp.status())),
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("it wasn't sent")))
    }

    /// A job's artifacts in pieces, each taken before the next goes and the whole checked at the
    /// end (its size and digest): one that breaks off costs that piece, and the upload goes on
    /// from where the server's pieces end. `false`: the server doesn't take them so (an older one,
    /// which answers no `GET` for how much came).
    async fn upload_pieces(&self, url: &str, bytes: &bytes::Bytes) -> anyhow::Result<bool> {
        use sha2::Digest as _;
        let token = &self.assign.job_token;
        let have = |v: &serde_json::Value| v.get("have").and_then(serde_json::Value::as_u64).and_then(|n| usize::try_from(n).ok());
        // Where the server's pieces end.
        let had = || async {
            let r = self.http.get(url).bearer(token).timeout(Duration::from_secs(30)).send().await.ok()?;
            if !r.is_success() {
                return None;
            }
            have(&r.json::<serde_json::Value>(1 << 10).await.ok()?)
        };
        if had().await.is_none() {
            return Ok(false);
        }
        let total = bytes.len();
        let (mut at, mut misses) = (0usize, 0u32);
        while at < total {
            let piece = bytes.slice(at..at.saturating_add(ARTIFACT_PIECE).min(total));
            match self.http.put(&format!("{url}?at={at}")).bearer(token).body(piece).timeout(Duration::from_secs(120)).send().await {
                Ok(r) if r.is_success() || r.status() == 409 => {
                    // Taken (or not where they ended): on from where they end now.
                    let ok = r.is_success();
                    let next = r.json::<serde_json::Value>(1 << 10).await.ok().as_ref().and_then(have);
                    match next {
                        Some(n) if n <= total => at = n,
                        _ => anyhow::bail!("the server's pieces of its artifacts end past them"),
                    }
                    if ok {
                        misses = 0;
                        continue;
                    }
                }
                Ok(r) if r.status() < 500 => anyhow::bail!("HTTP {} for a piece of its artifacts", r.status()),
                _ => {}
            }
            misses += 1;
            anyhow::ensure!(misses <= 6, "its artifacts' upload broke off {misses} times running");
            crate::rt::sleep(Duration::from_secs(2 * u64::from(misses))).await;
            if let Some(n) = had().await.filter(|n| *n <= total) {
                at = n;
            }
        }
        let digest = hex::encode(sha2::Sha256::digest(bytes));
        let r = self.http.post(&format!("{url}?size={total}&sha256={digest}")).bearer(token).timeout(Duration::from_secs(300)).send().await?;
        anyhow::ensure!(r.is_success(), "HTTP {} as its artifacts' upload ended", r.status());
        Ok(true)
    }
}

/// Translate the shared runtime's events back to the old server without giving internal
/// checkout/cleanup steps new durable v1 indices. The channel is bounded; it carries no state
/// ownership and ends after the common runtime has drained all step output.
async fn legacy_events(rx: async_channel::Receiver<Up>, up: Uplink, masks: Vec<String>) {
    let mut indices = BTreeMap::new();
    let mut next = 0_u32;
    let masker = kiln_ci::runner::mask::Masker::new(&masks);
    while let Ok(item) = rx.recv().await {
        let item = match item {
            Up::Msg(AgentMsg::JobEvent { job, json }) => {
                let event = serde_json::from_str::<JobEventV2>(&json);
                match event {
                    Ok(JobEventV2::StepStarted { idx, kind, .. }) if kind == "run" => {
                        let old = *indices.entry(idx).or_insert_with(|| { let old = next; next = next.saturating_add(1); old });
                        Up::Msg(AgentMsg::StepStarted { job, idx: old })
                    }
                    Ok(JobEventV2::StepFinished { idx, outcome, conclusion, exit_code, .. }) => {
                        let Some(old) = indices.get(&idx).copied() else { continue };
                        let status = match conclusion {
                            kiln_ci::wire::Conclusion::Success if outcome == kiln_ci::wire::Conclusion::Failure => Status::Failed,
                            kiln_ci::wire::Conclusion::Success => Status::Succeeded,
                            kiln_ci::wire::Conclusion::Failure => Status::Failed,
                            kiln_ci::wire::Conclusion::Cancelled => Status::Cancelled,
                            kiln_ci::wire::Conclusion::Skipped => Status::Skipped,
                        };
                        Up::Msg(AgentMsg::StepFinished { job, idx: old, status, exit_code })
                    }
                    Ok(JobEventV2::Annotation { idx, level, message, .. }) => Up::Log(LogChunk {
                        job, idx: indices.get(&idx).copied().unwrap_or_else(|| next.saturating_sub(1)),
                        data: format!("[kiln] {level}: {}\r\n", masker.mask(&message)).into_bytes(),
                    }),
                    // Old servers have no outputs or failure-classification protocol.
                    _ => continue,
                }
            }
            Up::Log(mut log) => {
                log.idx = indices.get(&log.idx).copied().unwrap_or_else(|| next.saturating_sub(1));
                Up::Log(log)
            }
            other => other,
        };
        if up.0.send(item).await.is_err() { return }
    }
}

/// Each legacy artifact piece is bounded, and a broken upload resumes at its last acknowledged byte.
const ARTIFACT_PIECE: usize = 8 << 20;

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};

    /// U03: old assignment frames use the common runtime while an old server still receives
    /// its own source/artifact requests and original step indices. Secrets and interpolation
    /// shaped literal data survive the wire conversion without being evaluated again.
    #[test]
    fn u03_legacy_assignment_runs_through_shared_runtime_and_old_transports() -> anyhow::Result<()> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        listener.set_nonblocking(true)?;
        let mut source = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(0);
        header.set_mode(0o644);
        header.set_cksum();
        source.append_data(&mut header, "seed.txt", std::io::empty())?;
        let source = source.into_inner()?;
        let server = std::thread::spawn(move || -> anyhow::Result<Vec<u8>> {
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            let mut artifact = Vec::new();
            for _ in 0..3 {
                let mut socket = loop {
                    match listener.accept() {
                        Ok((socket, _)) => break socket,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            anyhow::ensure!(std::time::Instant::now() < deadline, "legacy transport request timed out");
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(e) => return Err(e.into()),
                    }
                };
                socket.set_read_timeout(Some(Duration::from_secs(10)))?;
                let mut raw = Vec::new();
                let split = loop {
                    let mut buf = [0_u8; 4096];
                    let n = socket.read(&mut buf)?;
                    anyhow::ensure!(n > 0 && raw.len() < 1 << 20, "incomplete legacy request");
                    raw.extend_from_slice(buf.get(..n).unwrap_or_default());
                    if let Some(end) = raw.windows(4).position(|s| s == b"\r\n\r\n") { break end + 4 }
                };
                let head = String::from_utf8_lossy(raw.get(..split).unwrap_or_default()).into_owned();
                anyhow::ensure!(head.to_ascii_lowercase().contains("authorization: bearer old-job-token"), "legacy request has its job token");
                let length = head.lines().find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("content-length")).and_then(|(_, v)| v.trim().parse::<usize>().ok())).unwrap_or(0);
                while raw.len() < split.saturating_add(length) {
                    let mut buf = [0_u8; 4096];
                    let n = socket.read(&mut buf)?;
                    anyhow::ensure!(n > 0 && raw.len() < 1 << 20, "incomplete legacy body");
                    raw.extend_from_slice(buf.get(..n).unwrap_or_default());
                }
                let (status, body) = if head.starts_with("GET /api/jobs/legacy/source ") {
                    (200, source.as_slice())
                } else if head.starts_with("GET /api/jobs/legacy/artifacts/upload ") {
                    (404, b"old server".as_slice())
                } else {
                    anyhow::ensure!(head.starts_with("PUT /api/jobs/legacy/artifacts/upload "), "unexpected request: {}", head.lines().next().unwrap_or_default());
                    artifact = raw.get(split..split.saturating_add(length)).unwrap_or_default().to_vec();
                    (200, b"{}".as_slice())
                };
                write!(socket, "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len())?;
                socket.write_all(body)?;
            }
            Ok(artifact)
        });
        let work = tempfile::tempdir()?;
        let workflow = crate::pipeline::parse("name: ci\njobs:\n  job:\n    runs-on: [local]\n    artifacts: [result.txt]\n    env: { MY_SECRET: overridden }\n    steps:\n      - run: |\n          test -f seed.txt\n          printf '%s\\n' \"$MY_SECRET\"\n          printf '%s' '${{ still data }}' > result.txt\n      - run: exit 7\n        continue-on-error: true\n")?;
        let spec = crate::pipeline::plan(&workflow, &crate::pipeline::Context::default())?.into_iter().next().ok_or_else(|| anyhow::anyhow!("no fixture job"))?;
        // This is stored, already expanded v1 text, not a workflow expression to evaluate.
        let mut spec = spec;
        if let Some(step) = spec.steps.first_mut() {
            step.run = "test -f seed.txt\nprintf '%s\\n' \"$MY_SECRET\"\nprintf '%s' '${{ still data }}' > result.txt\nsleep 2.2".into();
        }
        let (up, messages) = async_channel::unbounded();
        let exec = Exec {
            assign: Assignment { job_id: "legacy".into(), run_id: "run".into(), repo: "test/legacy".into(), clone_url: String::new(), sha: "saved".into(), branch: "main".into(), event: "manual".into(), run_number: 1, attempt: 0, spec,
                secrets: vec![("MY_SECRET".into(), "masked-old-secret".into())], job_token: "old-job-token".into(), server_url: format!("http://{addr}") },
            work_dir: work.path().into(), agent_name: "fixture".into(), up: Uplink(up), cancel: Shutdown::new(), http: Arc::new(crate::http::client::Client::public()?),
        };
        let unowned = work.path().join("unowned");
        let native_work = tempfile::tempdir()?;
        let mut native = Exec { assign: exec.assign.clone(), work_dir: native_work.path().into(), agent_name: exec.agent_name.clone(),
            up: exec.up.clone(), cancel: Shutdown::new(), http: Arc::clone(&exec.http) };
        let admission_dir = tempfile::tempdir()?;
        let admission = crate::runner::processes::Admission::private(admission_dir.path())?;
        let native_admission = admission.clone();
        let execution = crate::runner::processes::Execution::new_in(admission, work.path().into(), "legacy".into(), Some(1));
        let result = crate::rt::block_on(async move {
            assert!(exec.checkout(&unowned, &exec.work_dir).await.is_err_and(|error| error.downcast_ref::<crate::proc::CustodyRefusal>() == Some(&crate::proc::CustodyRefusal::NoExecution)), "legacy checkout cannot manufacture an unbudgeted native owner");
            assert!(!unowned.exists(), "refused ownership cannot create a workspace or enter archive fallback");
            let result = execution.scope(exec.run()).await;
            execution.stop().await;
            Ok(result)
        })?;
        assert_eq!(result.status, Status::Succeeded, "{result:?}");
        assert!(result.peak_rss.is_some_and(|rss| rss > 0), "the common sampler retains legacy memory metrics: {result:?}");
        assert!(result.cpu_seconds.is_some_and(|cpu| cpu >= 0.0));
        let artifact = server.join().map_err(|_| anyhow::anyhow!("legacy fixture server failed"))??;
        let mut archive = tar::Archive::new(artifact.as_slice());
        let mut contents = String::new();
        for entry in archive.entries()? {
            let mut entry = entry?;
            if entry.path()?.as_ref() == Path::new("result.txt") { entry.read_to_string(&mut contents)?; }
        }
        assert_eq!(contents, "${{ still data }}");
        let (mut logs, mut starts, mut ends) = (String::new(), Vec::new(), Vec::new());
        while let Ok(item) = messages.try_recv() {
            match item {
                Up::Log(log) => logs.push_str(&String::from_utf8_lossy(&log.data)),
                Up::Msg(AgentMsg::StepStarted { idx, .. }) => starts.push(idx),
                Up::Msg(AgentMsg::StepFinished { idx, status, .. }) => ends.push((idx, status)),
                Up::Msg(AgentMsg::JobEvent { .. }) => anyhow::bail!("old server received a new event frame"),
                _ => {}
            }
        }
        assert_eq!(starts, vec![0, 1]);
        assert_eq!(ends, vec![(0, Status::Succeeded), (1, Status::Failed)]);
        assert!(logs.contains("***") && !logs.contains("masked-old-secret"), "legacy logs preserve common masking");
        let mut dirs = vec![work.path().to_path_buf()];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(dir)? {
                let entry = entry?;
                assert_ne!(entry.file_name(), "result.txt", "the common runtime cleans its workspace");
                if entry.file_type()?.is_dir() { dirs.push(entry.path()); }
            }
        }

        // The same consumer uses owned cold-open/selection/pinning on a native attempt. Once
        // pinned, a second workspace still checks out with the original remote offline; it
        // cannot depend on a duplicate implicit fetch or the server archive fallback.
        let repository = tempfile::tempdir()?;
        let source = repository.path().join("source");
        std::fs::create_dir(&source)?;
        let git = |dir: &Path, args: &[&str]| -> anyhow::Result<String> {
            let output = Command::new("git").current_dir(dir).args(args).env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_AUTHOR_NAME", "fixture").env("GIT_AUTHOR_EMAIL", "fixture@test").env("GIT_COMMITTER_NAME", "fixture").env("GIT_COMMITTER_EMAIL", "fixture@test").output()?;
            anyhow::ensure!(output.status.success(), "fixture git failed: {}", String::from_utf8_lossy(&output.stderr));
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
        };
        git(&source, &["init", "-q", "-b", "main"])?;
        std::fs::write(source.join("seed.txt"), b"native pinned checkout")?;
        git(&source, &["add", "."])?;
        git(&source, &["commit", "-qm", "native checkout"])?;
        native.assign.clone_url = source.to_string_lossy().into_owned();
        native.assign.sha = git(&source, &["rev-parse", "HEAD"])?;

        // The production retry helper distinguishes the lookup from publication even when
        // both commands exit 1. The injected phases do not spawn their commands.
        let missing_status = kiln_cd::git::command(Some(&source.join(".git")), None, &kiln_cd::git::Auth::default())
            .args(["rev-parse", "--verify", "--quiet", "--end-of-options", "refs/heads/fixture-missing^{commit}"]).output()?.status;
        assert_eq!(missing_status.code(), Some(1));
        let selection_mirror = kiln_cd::git::Mirror::open(&repository.path().join("selection.git"), &native.assign.clone_url, kiln_cd::git::Auth::default())?;
        let mut selection_operation = futures::executor::block_on(selection_mirror.operation())?;
        let mut pin_fetches = 0;
        let pin_failed = futures::executor::block_on(select_owned(&mut selection_operation, &native.assign.sha, |command| {
            let result = if command.get_args().any(|arg| arg == "fetch") {
                pin_fetches += 1;
                Err(anyhow::anyhow!("pin failure must not fetch"))
            } else if command.get_args().any(|arg| arg == "update-ref") {
                Err(kiln_cd::git::GitExit::new(missing_status, "fixture pin refused".into()).into())
            } else {
                assert!(command.get_args().any(|arg| arg == "rev-parse"));
                Ok(native.assign.sha.as_bytes().to_vec())
            };
            std::future::ready(result)
        }));
        assert!(pin_failed.is_err_and(|error| error.downcast_ref::<kiln_cd::git::GitExit>().is_some_and(|exit| exit.code() == Some(1))
            && error.downcast_ref::<kiln_cd::git::MissingCommit>().is_none()), "pin exit 1 is not revision absence");
        assert_eq!(pin_fetches, 0, "a present commit with a refused pin never triggers a network fetch");
        let (mut lookups, mut missing_fetches, mut pins) = (0, 0, 0);
        let fetch_spec = format!("{}:refs/kiln/pins/{}", native.assign.sha, native.assign.sha);
        let selected = futures::executor::block_on(select_owned(&mut selection_operation, &native.assign.sha, |command| {
            let result = if command.get_args().any(|arg| arg == "fetch") {
                missing_fetches += 1;
                assert!(command.get_args().any(|arg| arg == fetch_spec.as_str()));
                Ok(Vec::new())
            } else if command.get_args().any(|arg| arg == "update-ref") {
                pins += 1;
                Ok(Vec::new())
            } else {
                assert!(command.get_args().any(|arg| arg == "rev-parse"));
                lookups += 1;
                if lookups == 1 { Err(kiln_cd::git::GitExit::new(missing_status, "fixture commit absent".into()).into()) }
                else { Ok(native.assign.sha.as_bytes().to_vec()) }
            };
            std::future::ready(result)
        }))?;
        assert_eq!(selected.sha(), native.assign.sha);
        assert_eq!((lookups, missing_fetches, pins), (2, 1, 1), "only a missing lookup permits one targeted fetch before verification and pinning");
        drop(selection_operation);

        let native_execution = crate::runner::processes::Execution::new_in(native_admission.clone(), native_work.path().into(), "legacy".into(), Some(1));
        crate::rt::block_on(async move {
            let inherited = crate::runner::processes::Execution::inherited_in(native_admission.clone(), native_work.path().into(), "legacy".into(), 1,
                crate::runner::processes::ParentExecution::verified(kiln_ci::wire::ParentBinding { executor: "parent".into(), generation: "generation".into(), allocation: "allocation".into(), physical_execution: "physical-parent".into(), job: "legacy".into(), attempt: 1 }, "legacy", 1)?)?;
            inherited.scope(async {
                assert!(native.checkout(&native_work.path().join("refused"), native_work.path()).await.is_err_and(|error| error.downcast_ref::<crate::proc::CustodyRefusal>() == Some(&crate::proc::CustodyRefusal::InheritedExecution)), "parent identity alone is not independent native child authority");
            }).await;
            let wrong = crate::runner::processes::Execution::new_in(native_admission, native_work.path().into(), "legacy".into(), Some(2));
            wrong.scope(async { assert!(native.checkout(&native_work.path().join("refused"), native_work.path()).await.is_err(), "another attempt cannot lend its physical owner"); }).await;
            wrong.stop().await;
            assert!(!native_work.path().join("refused").exists());
            let result = native_execution.scope(async {
                let first = native_work.path().join("first");
                native.checkout(&first, native_work.path()).await?;
                assert_eq!(std::fs::read(first.join("seed.txt"))?, b"native pinned checkout");
                std::fs::rename(&source, repository.path().join("offline"))?;
                let second = native_work.path().join("second");
                native.checkout(&second, native_work.path()).await?;
                assert_eq!(std::fs::read(second.join("seed.txt"))?, b"native pinned checkout");
                assert_eq!(std::fs::read_to_string(second.join(".git/HEAD"))?.trim(), native.assign.sha);
                Ok::<(), anyhow::Error>(())
            }).await;
            native_execution.stop().await;
            result
        })?;
        Ok(())
    }
}
