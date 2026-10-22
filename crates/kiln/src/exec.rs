//! Executes one assigned job on an agent: fetch source, run steps, stream output, sample the
//! process tree, upload artifacts, clean up. Every failure becomes a job result; nothing here can
//! take the agent down.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::FutureExt as _;

use crate::proto::{AgentMsg, Assignment, JobResult, LOG_CHUNK, LogChunk};
use crate::rt::{Interval, Shutdown};
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

/// One turn of a step's event loop (computed inside `select!`, handled after it).
enum Ev {
    Chunk(Option<Vec<u8>>),
    Exit(Option<i32>),
    Flush,
    Sample,
    Cancel,
    Deadline,
}

/// The job's workspace: `<work>/w/<8 hex>`. Kept short on purpose: Unix socket paths are limited
/// to 107 bytes, and test suites bind sockets several directories deep inside their workspace
/// (`target/tmp/...`), so every byte of prefix counts.
fn workspace(work_dir: &Path, job: &str) -> PathBuf {
    let digest = crate::util::sha256_hex(job.as_bytes());
    work_dir.join("w").join(digest.get(..8).unwrap_or("job"))
}

/// The job's private temp directory: a sibling of its workspace on the same volume.
fn tmp_dir(ws: &Path) -> PathBuf {
    let mut name = ws.file_name().map(std::ffi::OsStr::to_os_string).unwrap_or_default();
    name.push(".tmp");
    ws.with_file_name(name)
}

fn have(bin: &str) -> bool {
    crate::os::have(bin)
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

/// Pipes deliver LF; xterm wants CRLF.
fn push_crlf(pending: &mut Vec<u8>, chunk: Vec<u8>) {
    for b in chunk {
        if b == b'\n' {
            pending.push(b'\r');
        }
        pending.push(b);
    }
}

/// `rx.recv()` while `enabled`, otherwise a future that never completes.
async fn recv_if(rx: &async_channel::Receiver<Vec<u8>>, enabled: bool) -> Option<Vec<u8>> {
    if enabled {
        rx.recv().await.ok()
    } else {
        futures::future::pending().await
    }
}

async fn fs<R: Send + 'static>(f: impl FnOnce() -> std::io::Result<R> + Send + 'static) -> anyhow::Result<R> {
    Ok(crate::rt::blocking(f).await??)
}

impl Exec {
    pub async fn run(self) -> JobResult {
        let job = self.assign.job_id.clone();
        self.up.msg(AgentMsg::JobStarted { job: job.clone() }).await;
        let ws = workspace(&self.work_dir, &job);
        let tmp = tmp_dir(&ws);
        let t = tmp.clone();
        if let Err(e) = fs(move || std::fs::create_dir_all(t)).await {
            tracing::warn!(error = %e, tmp = %tmp.display(), "cannot create job TMPDIR");
        }
        let result = self.run_inner(&ws).await;
        let t = tmp.clone();
        let _ = fs(move || std::fs::remove_dir_all(t)).await;
        let w = ws.clone();
        if let Err(e) = crate::rt::blocking(move || std::fs::remove_dir_all(w)).await.map_err(|e| std::io::Error::other(e.to_string())).and_then(|r| r)
            && e.kind() != std::io::ErrorKind::NotFound
        {
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
        let checked_out = futures::select_biased! {
            () = self.cancel.cancelled().fuse() => Err(anyhow::anyhow!("cancelled during checkout")),
            r = self.checkout(ws).fuse() => r,
        };
        if let Err(e) = checked_out {
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
            // Temp space on the work volume, not tmpfs or a container's overlay layer: big
            // build temporaries fit, and storage-sensitive tests see a real filesystem.
            ("TMPDIR", tmp_dir(ws).to_string_lossy().into_owned()),
            ("KILN_AGENT", self.agent_name.clone()),
            ("TERM", "xterm-256color".to_owned()),
            ("CARGO_TERM_COLOR", "always".to_owned()),
            ("FORCE_COLOR", "1".to_owned()),
        ] {
            env.entry(k.to_owned()).or_insert(v);
        }
        if cfg!(windows) {
            // Windows tools find their temp dir through TEMP / TMP rather than TMPDIR.
            let tmp = tmp_dir(ws).to_string_lossy().into_owned();
            for k in ["TEMP", "TMP"] {
                env.entry(k.to_owned()).or_insert_with(|| tmp.clone());
            }
        }
        for (k, v) in &a.secrets {
            env.insert(k.clone(), v.clone());
        }

        let deadline = Instant::now() + Duration::from_secs(u64::from(a.spec.timeout_minutes) * 60);
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
                .map(|m| (Instant::now() + Duration::from_secs(u64::from(m) * 60)).min(deadline))
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
                Err(e) => {
                    let why = format!("artifact upload failed: {e:#}");
                    self.up.note(&job, last, &why).await;
                    // A job whose artifacts didn't arrive didn't do what it's for (a release's
                    // binaries are its artifacts).
                    failed = failed.or(Some((Status::Failed, None, why)));
                }
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
        let w = ws.to_path_buf();
        fs(move || std::fs::create_dir_all(w)).await?;
        if have("git") && !a.clone_url.is_empty() {
            match self.git_checkout(ws).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    self.up.note(job, 0, &format!("git checkout failed ({e:#}); using server source archive")).await;
                    let w = ws.to_path_buf();
                    let _ = fs(move || std::fs::remove_dir_all(w)).await;
                    let w = ws.to_path_buf();
                    fs(move || std::fs::create_dir_all(w)).await?;
                }
            }
        }
        self.tarball(ws).await
    }

    async fn git(&self, args: &[&str]) -> anyhow::Result<()> {
        let mut cmd = Command::new("git");
        crate::os::git_no_prompts(cmd.args(args));
        let out = crate::proc::output(cmd, Duration::from_secs(1800)).await?;
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
        let c = cache.clone();
        fs(move || std::fs::create_dir_all(c)).await?;
        // Serialise mirror updates across concurrent jobs (and processes) with a lock file.
        let lock = FileLock::acquire(&lock_path).await?;
        if mirror.join("HEAD").exists() {
            self.git(&["-C", &m, "fetch", "--quiet", "--prune", "origin", "+refs/heads/*:refs/heads/*", "+refs/tags/*:refs/tags/*"]).await?;
        } else {
            self.git(&["clone", "--quiet", "--bare", &a.clone_url, &m]).await?;
        }
        if self.git(&["-C", &m, "cat-file", "-e", &format!("{}^{{commit}}", a.sha)]).await.is_err() {
            self.git(&["-C", &m, "fetch", "--quiet", "origin", &a.sha]).await?;
        }
        drop(lock);
        let w = ws.to_string_lossy().to_string();
        self.git(&["clone", "--quiet", "--shared", "--no-checkout", &m, &w]).await?;
        self.git(&["-C", &w, "checkout", "--quiet", "--detach", &a.sha]).await?;
        self.git(&["-C", &w, "remote", "set-url", "origin", &a.clone_url]).await?;
        if ws.join(".gitmodules").exists() {
            self.git(&["-C", &w, "submodule", "update", "--init", "--recursive", "--quiet"]).await?;
        }
        Ok(())
    }

    /// Stream the server's `git archive` of the commit to disk, then unpack it.
    async fn tarball(&self, ws: &Path) -> anyhow::Result<()> {
        let a = &self.assign;
        let url = format!("{}/api/jobs/{}/source", a.server_url.trim_end_matches('/'), a.job_id);
        let mut resp = self.http.get(&url).bearer(&a.job_token).timeout(Duration::from_secs(120)).send().await?;
        if !resp.is_success() {
            anyhow::bail!("source archive: HTTP {}", resp.status());
        }
        let tar_path = tmp_dir(ws).join("source.tar");
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
                    args.extend(crate::os::script_args(&prog, script));
                }
                (prog, args)
            }
            None => crate::os::default_step_shell(script),
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
        deadline: Instant,
        usage: &mut Usage,
    ) -> anyhow::Result<Outcome> {
        let (prog, args) = self.command_line(shell, script);
        let mut cmd = Command::new(prog);
        cmd.args(args).current_dir(cwd).envs(env);
        let mut running = crate::proc::spawn_group(cmd)?;
        let pgid = running.pgid;
        let _guard = PgidRecord::new(&self.work_dir, &self.assign.job_id, pgid);
        let mut pending: Vec<u8> = Vec::with_capacity(LOG_CHUNK);
        let mut flush = Interval::new(Duration::from_millis(100));
        let mut sample = Interval::new(Duration::from_secs(2));
        let mut output_done = false;
        let mut status: Option<Outcome> = None;
        loop {
            if output_done && status.is_some() {
                break;
            }
            // Children that daemonise keep the pipes open: once the step itself exited, give
            // output a moment to drain, then stop waiting for it.
            if status.is_some() {
                match crate::rt::timeout(Duration::from_secs(2), running.output.recv()).await {
                    Ok(Ok(c)) => push_crlf(&mut pending, c),
                    Ok(Err(_)) | Err(_) => output_done = true,
                }
                continue;
            }
            let ev = futures::select_biased! {
                c = recv_if(&running.output, !output_done).fuse() => Ev::Chunk(c),
                code = (&mut running.exit).fuse() => Ev::Exit(code.ok().flatten()),
                () = self.cancel.cancelled().fuse() => Ev::Cancel,
                () = crate::rt::sleep_until(deadline).fuse() => Ev::Deadline,
                () = flush.tick().fuse() => Ev::Flush,
                () = sample.tick().fuse() => Ev::Sample,
            };
            match ev {
                Ev::Chunk(Some(c)) => {
                    push_crlf(&mut pending, c);
                    if pending.len() >= LOG_CHUNK {
                        self.ship(idx, &mut pending).await;
                    }
                }
                Ev::Chunk(None) => output_done = true,
                Ev::Exit(code) => status = Some(Outcome::Exit(code)),
                Ev::Flush => {
                    if !pending.is_empty() {
                        self.ship(idx, &mut pending).await;
                    }
                }
                Ev::Sample => self.sample(pgid, usage).await,
                Ev::Cancel => {
                    crate::proc::terminate_group(&mut running, Duration::from_secs(10)).await;
                    status = Some(Outcome::Cancelled);
                }
                Ev::Deadline => {
                    crate::proc::terminate_group(&mut running, Duration::from_secs(10)).await;
                    status = Some(Outcome::TimedOut);
                }
            }
        }
        // Reap anything the step left behind in its process group.
        running.kill_group();
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
        deadline: Instant,
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
        let (tx, rx) = async_channel::bounded::<Vec<u8>>(64);
        std::thread::Builder::new().name("kiln-pty-step".into()).spawn(move || {
            let mut buf = vec![0u8; 16 * 1024];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 || tx.send_blocking(buf.get(..n).map(<[u8]>::to_vec).unwrap_or_default()).is_err() {
                    break;
                }
            }
        })?;
        let (done_tx, mut done_rx) = futures::channel::oneshot::channel();
        std::thread::Builder::new().name("kiln-pty-wait".into()).spawn(move || {
            let code = child.wait().ok().and_then(|s| i32::try_from(s.exit_code()).ok());
            let _ = done_tx.send(code);
        })?;
        let mut pending = Vec::new();
        let mut flush = Interval::new(Duration::from_millis(100));
        let mut sample = Interval::new(Duration::from_secs(2));
        let mut output_open = true;
        let outcome = loop {
            let ev = futures::select_biased! {
                c = recv_if(&rx, output_open).fuse() => Ev::Chunk(c),
                code = (&mut done_rx).fuse() => Ev::Exit(code.ok().flatten()),
                () = self.cancel.cancelled().fuse() => Ev::Cancel,
                () = crate::rt::sleep_until(deadline).fuse() => Ev::Deadline,
                () = flush.tick().fuse() => Ev::Flush,
                () = sample.tick().fuse() => Ev::Sample,
            };
            match ev {
                Ev::Chunk(Some(c)) => {
                    pending.extend(c);
                    if pending.len() >= LOG_CHUNK {
                        self.ship(idx, &mut pending).await;
                    }
                }
                Ev::Chunk(None) => output_open = false,
                Ev::Exit(code) => break Outcome::Exit(code),
                Ev::Flush => {
                    if !pending.is_empty() {
                        self.ship(idx, &mut pending).await;
                    }
                }
                Ev::Sample => self.sample(pgid, usage).await,
                Ev::Cancel => {
                    crate::proc::terminate_tree(pgid, Duration::from_secs(5)).await;
                    break Outcome::Cancelled;
                }
                Ev::Deadline => {
                    crate::proc::terminate_tree(pgid, Duration::from_secs(5)).await;
                    break Outcome::TimedOut;
                }
            }
        };
        crate::os::pty::close_master(pty.master);
        while let Ok(Ok(c)) = crate::rt::timeout(Duration::from_millis(300), rx.recv()).await {
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
        let s = crate::rt::blocking(move || crate::host::process_group(pgid)).await.unwrap_or_default();
        usage.peak_rss = usage.peak_rss.max(s.rss_bytes);
        usage.cpu_seconds = usage.cpu_seconds.max(s.cpu_seconds);
        self.up
            .msg(AgentMsg::JobSample { job: self.assign.job_id.clone(), rss: s.rss_bytes, cpu_seconds: s.cpu_seconds })
            .await;
    }

    async fn upload_artifacts(&self, ws: &Path) -> anyhow::Result<usize> {
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

/// How big each piece of an upload in pieces is (`Exec::upload_pieces`): what one holds on its way
/// is no more than this, and a piece that breaks off costs no more.
const ARTIFACT_PIECE: usize = 8 << 20;

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
/// is never touched, and its parent isn't a kiln still running: another runner process of this
/// machine (an upgrade's new build beside the old one shares the work directory) runs it.
pub fn reap_orphans(work_dir: &Path) -> usize {
    let dir = work_dir.join("pgids");
    let Ok(entries) = std::fs::read_dir(&dir) else { return 0 };
    let mut killed = 0;
    for e in entries.flatten() {
        let path = e.path();
        let pgid: i32 = e.file_name().to_string_lossy().parse().unwrap_or(0);
        if u32::try_from(pgid).is_ok_and(parent_is_another_kiln) {
            continue;
        }
        let job = std::fs::read_to_string(&path).unwrap_or_default();
        let _ = std::fs::remove_file(&path);
        if pgid <= 0 || job.is_empty() {
            continue;
        }
        if crate::proc::kill_orphan_group(pgid, &format!("KILN_JOB_ID={job}")) {
            killed += 1;
        }
    }
    killed
}

/// Whether process `pid` was started by a kiln that still runs, other than this process.
fn parent_is_another_kiln(pid: u32) -> bool {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    let mut sys = System::new();
    let p = Pid::from_u32(pid);
    sys.refresh_processes_specifics(ProcessesToUpdate::Some(&[p]), false, ProcessRefreshKind::nothing());
    let Some(parent) = sys.process(p).and_then(sysinfo::Process::parent) else { return false };
    if parent.as_u32() <= 1 || parent.as_u32() == std::process::id() {
        return false;
    }
    sys.refresh_processes_specifics(ProcessesToUpdate::Some(&[parent]), false, ProcessRefreshKind::nothing());
    sys.process(parent).is_some_and(|pp| pp.name().to_string_lossy().starts_with("kiln"))
}

/// A lock file (`flock` on Unix, `LockFileEx` on Windows) so concurrent jobs, even in separate
/// agent processes, update a shared git mirror one at a time. Released on drop (closing the file
/// releases the lock).
struct FileLock(#[allow(dead_code, reason = "held for its lock")] std::fs::File);

impl FileLock {
    async fn acquire(path: &Path) -> anyhow::Result<Self> {
        let path = path.to_path_buf();
        crate::rt::blocking(move || {
            let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&path)?;
            f.lock().map_err(|e| anyhow::anyhow!("lock {}: {e}", path.display()))?;
            Ok(Self(f))
        })
        .await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspaces_are_short_and_distinct() {
        let base = Path::new("/home/someone/.local/share/kiln/agent/work");
        let a = workspace(base, "01a146e7d7e972949d3537141e355b07");
        let b = workspace(base, "01a146e7d7e972949d3537141e355b08");
        assert_ne!(a, b);
        assert_eq!(a.parent(), Some(base.join("w").as_path()));
        // Leaves room for a socket several directories deep (e.g. cargo's target/tmp/...).
        assert!(a.as_os_str().len() + "/target/tmp/slates-ipc-1623558/original/socket".len() <= 107);
    }

    #[test]
    fn masks_secrets() {
        let out = mask(b"token=hunter2-secret ok", &[("T".into(), "hunter2-secret".into())]);
        assert_eq!(out, b"token=*** ok");
    }
}
