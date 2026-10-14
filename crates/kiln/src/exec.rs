//! Executes one assigned job on an agent: fetch source, run steps, stream output, sample the
//! process tree, upload artifacts, clean up. Every failure becomes a job result; nothing here can
//! take the agent down.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context as _;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use tokio::io::AsyncReadExt as _;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::proto::{AgentMsg, Assignment, JobResult, LOG_CHUNK, LogChunk};
use crate::store::Status;

/// What the executor sends upstream; the agent's uplink buffers it across reconnects.
#[derive(Debug)]
pub enum Up {
    Msg(AgentMsg),
    Log(LogChunk),
    /// Interactive terminal traffic: never buffered across a disconnect.
    Term(AgentMsg),
}

#[derive(Clone)]
pub struct Uplink(pub mpsc::Sender<Up>);

impl Uplink {
    pub async fn msg(&self, m: AgentMsg) {
        let _ = self.0.send(Up::Msg(m)).await;
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
    pub cancel: CancellationToken,
}

#[derive(Default)]
struct Usage {
    peak_rss: u64,
    cpu_seconds: f64,
}

enum Outcome {
    Exit(Option<i32>),
    Cancelled,
    TimedOut,
}

fn have(bin: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|p| p.join(bin).is_file()))
}

fn mask(data: &[u8], secrets: &[(String, String)]) -> Vec<u8> {
    let mut s = String::from_utf8_lossy(data).into_owned();
    for (_, v) in secrets {
        if v.len() >= 4 {
            s = s.replace(v.as_str(), "***");
        }
    }
    s.into_bytes()
}

fn slug(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect()
}

impl Exec {
    pub async fn run(self) -> JobResult {
        let job = self.assign.job_id.clone();
        self.up.msg(AgentMsg::JobStarted { job: job.clone() }).await;
        let ws = self.work_dir.join("ws").join(format!("{}-{}", slug(&self.assign.repo), job.get(job.len().saturating_sub(12)..).unwrap_or(&job)));
        let result = self.run_inner(&ws).await;
        if let Err(e) = tokio::fs::remove_dir_all(&ws).await
            && e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(error = %e, ws = %ws.display(), "workspace cleanup failed");
            }
        result
    }

    async fn run_inner(&self, ws: &Path) -> JobResult {
        let a = &self.assign;
        let job = a.job_id.clone();
        let fail = |status: Status, error: String| JobResult {
            job: job.clone(),
            status,
            exit_code: None,
            error: Some(error),
            peak_rss: None,
            cpu_seconds: None,
        };
        self.up
            .note(&job, 0, &format!(
                "job {} · attempt {} · agent {} · {}@{}",
                a.spec.display, a.attempt + 1, self.agent_name, a.repo, a.sha.get(..12).unwrap_or(&a.sha)
            ))
            .await;
        if let Err(e) = tokio::select! {
            r = self.checkout(ws) => r,
            () = self.cancel.cancelled() => Err(anyhow::anyhow!("cancelled during checkout")),
        } {
            let msg = format!("checkout failed: {e:#}");
            self.up.log(&job, 0, format!("\x1b[31m[kiln] {msg}\x1b[0m\r\n").into_bytes()).await;
            let status = if self.cancel.is_cancelled() { Status::Cancelled } else { Status::Failed };
            return fail(status, msg);
        }

        let mut env: BTreeMap<String, String> = a.spec.env.clone();
        for (k, v) in [
            ("CI", "true".to_owned()),
            ("KILN", "true".to_owned()),
            ("KILN_REPO", a.repo.clone()),
            ("KILN_SHA", a.sha.clone()),
            ("KILN_BRANCH", a.branch.clone()),
            ("KILN_EVENT", a.event.clone()),
            ("KILN_RUN_ID", a.run_id.clone()),
            ("KILN_RUN_NUMBER", a.run_number.to_string()),
            ("KILN_JOB_ID", job.clone()),
            ("KILN_JOB", a.spec.key.clone()),
            ("KILN_ATTEMPT", a.attempt.to_string()),
            ("KILN_WORKSPACE", ws.to_string_lossy().into_owned()),
            ("KILN_AGENT", self.agent_name.clone()),
            ("TERM", "xterm-256color".to_owned()),
            ("CARGO_TERM_COLOR", "always".to_owned()),
            ("FORCE_COLOR", "1".to_owned()),
        ] {
            env.entry(k.to_owned()).or_insert(v);
        }
        for (k, v) in &a.secrets {
            env.insert(k.clone(), v.clone());
        }

        let deadline = tokio::time::Instant::now() + Duration::from_secs(u64::from(a.spec.timeout_minutes) * 60);
        let mut usage = Usage::default();
        let mut failed: Option<(Status, Option<i32>, String)> = None;
        for (i, step) in a.spec.steps.iter().enumerate() {
            let idx = u32::try_from(i).unwrap_or(u32::MAX);
            if failed.is_some() {
                self.up.msg(AgentMsg::StepFinished { job: job.clone(), idx, status: Status::Skipped, exit_code: None }).await;
                continue;
            }
            self.up.msg(AgentMsg::StepStarted { job: job.clone(), idx }).await;
            let step_deadline = step
                .timeout_minutes
                .map(|m| (tokio::time::Instant::now() + Duration::from_secs(u64::from(m) * 60)).min(deadline))
                .unwrap_or(deadline);
            let mut step_env = env.clone();
            step_env.extend(step.env.clone());
            let cwd = match &step.working_directory {
                Some(d) => ws.join(d),
                None => ws.to_path_buf(),
            };
            let outcome = if step.tty {
                self.run_pty(idx, &step.run, step.shell.as_deref(), &cwd, &step_env, step_deadline, &mut usage).await
            } else {
                self.run_piped(idx, &step.run, step.shell.as_deref(), &cwd, &step_env, step_deadline, &mut usage).await
            };
            let (status, code, err) = match outcome {
                Ok(Outcome::Exit(Some(0))) => (Status::Succeeded, Some(0), None),
                Ok(Outcome::Exit(code)) => (Status::Failed, code, Some(format!("step {} exited with {code:?}", idx + 1))),
                Ok(Outcome::Cancelled) => (Status::Cancelled, None, Some("cancelled".to_owned())),
                Ok(Outcome::TimedOut) => (Status::TimedOut, None, Some("timed out".to_owned())),
                Err(e) => (Status::Failed, None, Some(format!("step {} could not start: {e:#}", idx + 1))),
            };
            if let Some(e) = &err {
                self.up.log(&job, idx, format!("\r\n\x1b[31m[kiln] {e}\x1b[0m\r\n").into_bytes()).await;
            }
            self.up.msg(AgentMsg::StepFinished { job: job.clone(), idx, status, exit_code: code }).await;
            if status != Status::Succeeded && !(step.continue_on_error && status == Status::Failed) {
                failed = Some((status, code, err.unwrap_or_default()));
            }
        }

        if !a.spec.artifacts.is_empty() && !self.cancel.is_cancelled() {
            let last = u32::try_from(a.spec.steps.len().saturating_sub(1)).unwrap_or(0);
            match self.upload_artifacts(ws).await {
                Ok(n) if n > 0 => self.up.note(&job, last, &format!("uploaded {n} artifact files")).await,
                Ok(_) => self.up.note(&job, last, "no files matched the artifact patterns").await,
                Err(e) => self.up.note(&job, last, &format!("artifact upload failed: {e:#}")).await,
            }
        }

        let (status, exit_code, error) = match failed {
            None => (Status::Succeeded, Some(0), None),
            Some((s, c, e)) => (s, c, Some(e)),
        };
        JobResult {
            job,
            status,
            exit_code,
            error,
            peak_rss: Some(usage.peak_rss),
            cpu_seconds: Some(usage.cpu_seconds),
        }
    }

    /// Prefer a git checkout (keeps `.git` for tools that need history), using a per-repo bare
    /// mirror cache; fall back to the server's source tarball (no git or GitHub access needed).
    async fn checkout(&self, ws: &Path) -> anyhow::Result<()> {
        let a = &self.assign;
        let job = &a.job_id;
        tokio::fs::create_dir_all(ws).await?;
        if have("git") && !a.clone_url.is_empty() {
            match self.git_checkout(ws).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    self.up.note(job, 0, &format!("git checkout failed ({e:#}); using server source archive")).await;
                    let _ = tokio::fs::remove_dir_all(ws).await;
                    tokio::fs::create_dir_all(ws).await?;
                }
            }
        }
        self.tarball(ws).await
    }

    async fn git(&self, args: &[&str]) -> anyhow::Result<()> {
        let out = tokio::process::Command::new("git")
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "/bin/true")
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output()
            .await?;
        if !out.status.success() {
            anyhow::bail!("git {}: {}", args.first().unwrap_or(&""), String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(())
    }

    async fn git_checkout(&self, ws: &Path) -> anyhow::Result<()> {
        let a = &self.assign;
        let cache = std::env::var_os("KILN_GIT_CACHE").map(PathBuf::from).unwrap_or_else(|| self.work_dir.join("git"));
        let mirror = cache.join(format!("{}.git", slug(&a.repo)));
        let m = mirror.to_string_lossy().to_string();
        let lock_path = cache.join(format!("{}.lock", slug(&a.repo)));
        tokio::fs::create_dir_all(&cache).await?;
        // Serialise mirror updates across concurrent jobs (and processes) with a lock file.
        let _lock = FileLock::acquire(&lock_path).await?;
        if mirror.join("HEAD").exists() {
            self.git(&["-C", &m, "fetch", "--quiet", "--prune", "origin", "+refs/heads/*:refs/heads/*", "+refs/tags/*:refs/tags/*"]).await?;
        } else {
            self.git(&["clone", "--quiet", "--bare", &a.clone_url, &m]).await?;
        }
        if self.git(&["-C", &m, "cat-file", "-e", &format!("{}^{{commit}}", a.sha)]).await.is_err() {
            self.git(&["-C", &m, "fetch", "--quiet", "origin", &a.sha]).await?;
        }
        drop(_lock);
        let w = ws.to_string_lossy().to_string();
        self.git(&["clone", "--quiet", "--shared", "--no-checkout", &m, &w]).await?;
        self.git(&["-C", &w, "checkout", "--quiet", "--detach", &a.sha]).await?;
        self.git(&["-C", &w, "remote", "set-url", "origin", &a.clone_url]).await?;
        if ws.join(".gitmodules").exists() {
            self.git(&["-C", &w, "submodule", "update", "--init", "--recursive", "--quiet"]).await?;
        }
        Ok(())
    }

    async fn tarball(&self, ws: &Path) -> anyhow::Result<()> {
        let a = &self.assign;
        let url = format!("{}/api/jobs/{}/source", a.server_url.trim_end_matches('/'), a.job_id);
        let resp = reqwest::Client::new()
            .get(&url)
            .bearer_auth(&a.job_token)
            .timeout(Duration::from_secs(600))
            .send()
            .await
            .context("download source")?;
        if !resp.status().is_success() {
            anyhow::bail!("source archive: HTTP {}", resp.status());
        }
        let body = resp.bytes().await?;
        let ws = ws.to_path_buf();
        tokio::task::spawn_blocking(move || tar::Archive::new(std::io::Cursor::new(body)).unpack(ws))
            .await
            .map_err(|e| anyhow::anyhow!("unpack task: {e}"))??;
        Ok(())
    }

    fn command_line(&self, shell: Option<&str>, script: &str) -> (String, Vec<String>) {
        match shell.map(str::trim).filter(|s| !s.is_empty()) {
            Some(custom) => {
                let mut parts = custom.split_whitespace().map(str::to_owned);
                let prog = parts.next().unwrap_or_else(|| "sh".into());
                let mut args: Vec<String> = parts.collect();
                if args.iter().any(|a| a == "{0}") {
                    for a in &mut args {
                        if a == "{0}" {
                            *a = script.to_owned();
                        }
                    }
                } else {
                    args.push("-c".into());
                    args.push(script.to_owned());
                }
                (prog, args)
            }
            None if have("bash") => ("bash".into(), vec!["--noprofile".into(), "--norc".into(), "-eo".into(), "pipefail".into(), "-c".into(), script.to_owned()]),
            None => ("sh".into(), vec!["-ec".into(), script.to_owned()]),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_piped(
        &self,
        idx: u32,
        script: &str,
        shell: Option<&str>,
        cwd: &Path,
        env: &BTreeMap<String, String>,
        deadline: tokio::time::Instant,
        usage: &mut Usage,
    ) -> anyhow::Result<Outcome> {
        let (prog, args) = self.command_line(shell, script);
        let mut child = tokio::process::Command::new(prog)
            .args(args)
            .current_dir(cwd)
            .envs(env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()?;
        let pgid = child.id().and_then(|p| i32::try_from(p).ok()).unwrap_or(0);
        let _guard = PgidRecord::new(&self.work_dir, &self.assign.job_id, pgid);
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
        if let Some(out) = child.stdout.take() {
            pump(out, tx.clone());
        }
        if let Some(err) = child.stderr.take() {
            pump(err, tx.clone());
        }
        drop(tx);
        let mut pending: Vec<u8> = Vec::with_capacity(LOG_CHUNK);
        let mut flush = tokio::time::interval(Duration::from_millis(100));
        let mut sample = tokio::time::interval(Duration::from_secs(2));
        let mut output_done = false;
        let mut status: Option<Outcome> = None;
        loop {
            tokio::select! {
                chunk = rx.recv(), if !output_done => match chunk {
                    Some(c) => {
                        // Pipes deliver LF; xterm wants CRLF.
                        for b in c {
                            if b == b'\n' { pending.push(b'\r'); }
                            pending.push(b);
                        }
                        if pending.len() >= LOG_CHUNK { self.ship(idx, &mut pending).await; }
                    }
                    None => output_done = true,
                },
                st = child.wait(), if status.is_none() => {
                    status = Some(Outcome::Exit(st.ok().and_then(|s| s.code())));
                }
                _ = flush.tick() => { if !pending.is_empty() { self.ship(idx, &mut pending).await; } }
                _ = sample.tick() => self.sample(pgid, usage).await,
                () = self.cancel.cancelled(), if status.is_none() => {
                    terminate(pgid, &mut child).await;
                    status = Some(Outcome::Cancelled);
                }
                () = tokio::time::sleep_until(deadline), if status.is_none() => {
                    terminate(pgid, &mut child).await;
                    status = Some(Outcome::TimedOut);
                }
            }
            if output_done && status.is_some() {
                break;
            }
            // Children that daemonise keep the pipes open: once the step itself exited, give
            // output a moment to drain, then stop waiting for it.
            if status.is_some() && !output_done {
                match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
                    Ok(Some(c)) => {
                        for b in c {
                            if b == b'\n' { pending.push(b'\r'); }
                            pending.push(b);
                        }
                    }
                    Ok(None) | Err(_) => output_done = true,
                }
            }
        }
        // Reap anything the step left behind in its process group.
        if pgid > 0 {
            let _ = killpg(Pid::from_raw(pgid), Signal::SIGKILL);
        }
        if !pending.is_empty() {
            self.ship(idx, &mut pending).await;
        }
        Ok(status.unwrap_or(Outcome::Exit(None)))
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_pty(
        &self,
        idx: u32,
        script: &str,
        shell: Option<&str>,
        cwd: &Path,
        env: &BTreeMap<String, String>,
        deadline: tokio::time::Instant,
        usage: &mut Usage,
    ) -> anyhow::Result<Outcome> {
        use portable_pty::{CommandBuilder, PtySize, native_pty_system};
        use std::io::Read as _;
        let (prog, args) = self.command_line(shell, script);
        let pty = native_pty_system().openpty(PtySize { rows: 50, cols: 160, pixel_width: 0, pixel_height: 0 })?;
        let mut cmd = CommandBuilder::new(prog);
        cmd.args(args);
        cmd.cwd(cwd);
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = pty.slave.spawn_command(cmd)?;
        drop(pty.slave);
        let pgid = child.process_id().and_then(|p| i32::try_from(p).ok()).unwrap_or(0);
        let _guard = PgidRecord::new(&self.work_dir, &self.assign.job_id, pgid);
        let mut reader = pty.master.try_clone_reader()?;
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 16 * 1024];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 || tx.blocking_send(buf.get(..n).map(<[u8]>::to_vec).unwrap_or_default()).is_err() {
                    break;
                }
            }
        });
        let (done_tx, mut done_rx) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let code = child.wait().ok().and_then(|s| i32::try_from(s.exit_code()).ok());
            let _ = done_tx.send(code);
        });
        let mut pending = Vec::new();
        let mut flush = tokio::time::interval(Duration::from_millis(100));
        let mut sample = tokio::time::interval(Duration::from_secs(2));
        let outcome = loop {
            tokio::select! {
                Some(c) = rx.recv() => {
                    pending.extend(c);
                    if pending.len() >= LOG_CHUNK { self.ship(idx, &mut pending).await; }
                }
                code = &mut done_rx => break Outcome::Exit(code.ok().flatten()),
                _ = flush.tick() => { if !pending.is_empty() { self.ship(idx, &mut pending).await; } }
                _ = sample.tick() => self.sample(pgid, usage).await,
                () = self.cancel.cancelled() => { kill_group(pgid).await; break Outcome::Cancelled; }
                () = tokio::time::sleep_until(deadline) => { kill_group(pgid).await; break Outcome::TimedOut; }
            }
        };
        drop(pty.master);
        while let Ok(Some(c)) = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await {
            pending.extend(c);
        }
        if !pending.is_empty() {
            self.ship(idx, &mut pending).await;
        }
        Ok(outcome)
    }

    async fn ship(&self, idx: u32, pending: &mut Vec<u8>) {
        let data = mask(pending, &self.assign.secrets);
        pending.clear();
        for chunk in data.chunks(LOG_CHUNK) {
            self.up.log(&self.assign.job_id, idx, chunk.to_vec()).await;
        }
    }

    async fn sample(&self, pgid: i32, usage: &mut Usage) {
        if pgid <= 0 {
            return;
        }
        let s = tokio::task::spawn_blocking(move || crate::host::process_group(pgid)).await.unwrap_or_default();
        usage.peak_rss = usage.peak_rss.max(s.rss_bytes);
        usage.cpu_seconds = usage.cpu_seconds.max(s.cpu_seconds);
        self.up
            .msg(AgentMsg::JobSample { job: self.assign.job_id.clone(), rss: s.rss_bytes, cpu_seconds: s.cpu_seconds })
            .await;
    }

    async fn upload_artifacts(&self, ws: &Path) -> anyhow::Result<usize> {
        let patterns = self.assign.spec.artifacts.clone();
        let root = ws.to_path_buf();
        let (count, bytes) = tokio::task::spawn_blocking(move || -> anyhow::Result<(usize, Vec<u8>)> {
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
        .await
        .map_err(|e| anyhow::anyhow!("artifact task: {e}"))??;
        if count == 0 {
            return Ok(0);
        }
        let a = &self.assign;
        let url = format!("{}/api/jobs/{}/artifacts/upload", a.server_url.trim_end_matches('/'), a.job_id);
        let resp = reqwest::Client::new()
            .put(url)
            .bearer_auth(&a.job_token)
            .body(bytes)
            .timeout(Duration::from_secs(900))
            .send()
            .await?;
        if !resp.status().is_success() {
            anyhow::bail!("HTTP {}", resp.status());
        }
        Ok(count)
    }
}

/// Forward everything a pipe produces into `tx` until EOF.
fn pump<R>(mut r: R, tx: mpsc::Sender<Vec<u8>>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            match r.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf.get(..n).map(<[u8]>::to_vec).unwrap_or_default()).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
}

/// Records a running step's process group on disk so that, if this agent dies without cleaning
/// up (SIGKILL, power loss, server crash for the in-process agent), the next start can kill the
/// orphans. Removed when the step ends normally.
struct PgidRecord(Option<PathBuf>);

impl PgidRecord {
    fn new(work_dir: &Path, job: &str, pgid: i32) -> Self {
        if pgid <= 0 {
            return Self(None);
        }
        let dir = work_dir.join("pgids");
        let path = dir.join(format!("{pgid}"));
        let ok = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, job)).is_ok();
        Self(ok.then_some(path))
    }
}

impl Drop for PgidRecord {
    fn drop(&mut self) {
        if let Some(p) = self.0.take() {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Kill process groups left behind by a previous incarnation of this agent. A group is only
/// killed when its leader still carries `KILN_JOB_ID=<job>` in its environment, so a recycled pid
/// is never touched.
pub fn reap_orphans(work_dir: &Path) -> usize {
    let dir = work_dir.join("pgids");
    let Ok(entries) = std::fs::read_dir(&dir) else { return 0 };
    let mut killed = 0;
    for e in entries.flatten() {
        let path = e.path();
        let pgid: i32 = e.file_name().to_string_lossy().parse().unwrap_or(0);
        let job = std::fs::read_to_string(&path).unwrap_or_default();
        let _ = std::fs::remove_file(&path);
        if pgid <= 0 || job.is_empty() {
            continue;
        }
        let environ = std::fs::read(format!("/proc/{pgid}/environ")).unwrap_or_default();
        let marker = format!("KILN_JOB_ID={job}");
        if environ.split(|b| *b == 0).any(|kv| kv == marker.as_bytes())
            && killpg(Pid::from_raw(pgid), Signal::SIGKILL).is_ok() {
                killed += 1;
            }
    }
    killed
}

async fn kill_group(pgid: i32) {
    if pgid <= 0 {
        return;
    }
    let _ = killpg(Pid::from_raw(pgid), Signal::SIGTERM);
    tokio::time::sleep(Duration::from_secs(5)).await;
    let _ = killpg(Pid::from_raw(pgid), Signal::SIGKILL);
}

async fn terminate(pgid: i32, child: &mut tokio::process::Child) {
    if pgid > 0 {
        let _ = killpg(Pid::from_raw(pgid), Signal::SIGTERM);
    }
    if tokio::time::timeout(Duration::from_secs(10), child.wait()).await.is_err() {
        if pgid > 0 {
            let _ = killpg(Pid::from_raw(pgid), Signal::SIGKILL);
        }
        let _ = child.kill().await;
    }
}

/// An advisory lock file (`flock`) so concurrent jobs, even in separate agent processes, update a
/// shared git mirror one at a time. Released on drop.
struct FileLock(Option<nix::fcntl::Flock<std::fs::File>>);

impl FileLock {
    async fn acquire(path: &Path) -> anyhow::Result<Self> {
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&path)?;
            nix::fcntl::Flock::lock(f, nix::fcntl::FlockArg::LockExclusive)
                .map(|l| Self(Some(l)))
                .map_err(|(_, e)| anyhow::anyhow!("flock {}: {e}", path.display()))
        })
        .await
        .map_err(|e| anyhow::anyhow!("lock task: {e}"))?
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        drop(self.0.take());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_secrets() {
        let out = mask(b"token=hunter2-secret ok", &[("T".into(), "hunter2-secret".into())]);
        assert_eq!(out, b"token=*** ok");
    }
}
