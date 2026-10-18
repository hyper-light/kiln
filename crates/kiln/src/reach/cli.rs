//! `kiln run`, `kiln runners` and `kiln reach`: run a command on another runner from any shell on a
//! runner's machine (a person's, or an agent's Bash tool), see which runners there are, and manage
//! reach's trust.
//!
//! ```text
//! kiln run --on linux/amd64 -- cargo test      # the idlest Linux x86-64 runner, this repo synced over
//! kiln run --on adacachy --no-sync -- uptime   # a runner by name, in its home folder
//! kiln runners linux                           # the Linux runners, best first
//! ```

#[cfg(unix)]
use std::io::Write as _;

use super::local::{Ev, Req};
#[cfg(unix)]
use super::local::RunReq;
#[cfg(unix)]
use super::wire::Signal;

/// The remote command's exit status, for `main` to exit with (no message: it said it all).
#[derive(Debug)]
pub struct ExitWith(pub u8);

impl std::fmt::Display for ExitWith {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "exit {}", self.0)
    }
}

impl std::error::Error for ExitWith {}

#[derive(clap::Args, Clone, Debug)]
pub struct RunArgs {
    /// Where: a runner's name, or what it must be — `linux/amd64`, `macos`, `arm64`, `linux,+gpu`,
    /// `cpus>=16`, `mem>=32g`. The idlest runner that suits is picked. Default: the best of all
    /// (this machine itself when it is).
    #[arg(long, short = 'o', default_value = "")]
    pub on: String,
    /// Copy this folder's tree (its git repository, as git sees it) over first, and run in the
    /// matching folder of the copy. On by default inside a git repository.
    #[arg(long, overrides_with = "no_sync")]
    pub sync: bool,
    /// Run in the other runner's home folder (or --cwd), copying nothing.
    #[arg(long)]
    pub no_sync: bool,
    /// The folder on the other runner, with --no-sync.
    #[arg(long)]
    pub cwd: Option<String>,
    /// Environment for the command (repeatable).
    #[arg(long = "env", short = 'e', value_name = "KEY=VALUE")]
    pub env: Vec<String>,
    /// Send this terminal's stdin to the command.
    #[arg(long, short = 'i')]
    pub stdin: bool,
    /// Stop the command after this many seconds.
    #[arg(long)]
    pub timeout: Option<u64>,
    /// A named shell on the other runner: its folder and exported variables carry over between runs.
    #[arg(long)]
    pub shell: Option<String>,
    /// Keep the command running if this one is stopped.
    #[arg(long)]
    pub detach: bool,
    /// No `[kiln]` lines on stderr (where it ran, what was synced).
    #[arg(long, short = 'q')]
    pub quiet: bool,
    /// The command (shell text; several words are joined with spaces).
    #[arg(last = true, required = true)]
    pub command: Vec<String>,
}

#[derive(clap::Args, Clone, Debug)]
pub struct RunnersArgs {
    /// Rank them for this (`linux/amd64`, `macos`, a name ...).
    pub want: Option<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(clap::Args, Clone, Debug)]
pub struct ReachArgs {
    #[command(subcommand)]
    pub cmd: ReachCmd,
}

#[derive(clap::Subcommand, Clone, Debug)]
pub enum ReachCmd {
    /// This runner's reach identity, the runners it pinned, its links.
    Status,
    /// Forget a runner's pinned key (after reinstalling it): its next key is trusted on first use.
    Forget { runner: String },
    /// Serve reach as MCP tools on stdin/stdout (kiln gives these to the agents it starts).
    Mcp,
}

#[cfg(unix)]
fn in_git_repo(dir: &std::path::Path) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--is-inside-work-tree"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(unix)]
fn human(n: u64) -> String {
    match n {
        n if n >= 1 << 30 => format!("{:.1} GB", n as f64 / f64::from(1u32 << 30)),
        n if n >= 1 << 20 => format!("{:.1} MB", n as f64 / f64::from(1u32 << 20)),
        n if n >= 1 << 10 => format!("{:.0} KB", n as f64 / 1024.0),
        n => format!("{n} B"),
    }
}

/// `kiln run`.
#[cfg(unix)]
pub async fn run(a: RunArgs, shutdown: crate::rt::Shutdown) -> anyhow::Result<()> {
    use std::os::fd::OwnedFd;
    let path = super::local::find_socket()?;
    let here = std::env::current_dir()?;
    let sync = if a.no_sync { false } else { a.sync || in_git_repo(&here) };
    let env = a
        .env
        .iter()
        .map(|kv| kv.split_once('=').map(|(k, v)| (k.to_owned(), v.to_owned())).ok_or_else(|| anyhow::anyhow!("--env takes KEY=VALUE, not {kv:?}")))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let req = RunReq {
        target: a.on.clone(),
        command: a.command.join(" "),
        cwd: Some(here.to_string_lossy().into_owned()),
        remote_cwd: a.cwd.clone(),
        sync,
        env,
        stdin: a.stdin,
        timeout_ms: a.timeout.map(|s| s.saturating_mul(1000)),
        shell: a.shell.clone(),
        detach: a.detach,
    };
    let stream = std::os::unix::net::UnixStream::connect(&path).map_err(|e| anyhow::anyhow!("cannot reach the kiln runner at {}: {e}", path.display()))?;
    stream.set_nonblocking(true)?;
    let fd = std::sync::Arc::new(OwnedFd::from(stream));
    crate::os::proc::write_all(&fd, &super::wire::frame(&Req::Run(Box::new(req)))?).await?;
    // Stdin, when asked for, goes over as it comes.
    if a.stdin {
        let w = std::sync::Arc::clone(&fd);
        crate::rt::spawn(async move {
            let lines = crate::rt::blocking(|| {
                let mut buf = Vec::new();
                let _ = std::io::Read::read_to_end(&mut std::io::stdin(), &mut buf);
                buf
            })
            .await
            .unwrap_or_default();
            for chunk in lines.chunks(64 * 1024) {
                if let Ok(b) = super::wire::frame(&Req::Input(chunk.to_vec())) {
                    let _ = crate::os::proc::write_all(&w, &b).await;
                }
            }
            if let Ok(b) = super::wire::frame(&Req::InputEnd) {
                let _ = crate::os::proc::write_all(&w, &b).await;
            }
        });
    }
    let mut de = super::wire::Deframer::default();
    let mut buf = vec![0u8; 64 * 1024];
    let mut interrupted = false;
    let say = |line: String| {
        if !a.quiet {
            let _ = writeln!(std::io::stderr(), "\x1b[2m[kiln] {line}\x1b[0m");
        }
    };
    loop {
        while let Some(ev) = de.take::<Ev>()? {
            match ev {
                Ev::Picked { runner, why } => say(format!("on {runner} · {why}")),
                Ev::Syncing { files, bytes } => say(format!("syncing {files} files ({})…", human(bytes))),
                Ev::Synced { sent, removed, bytes, ms, root } => {
                    let what = if sent == 0 && removed == 0 { "already in sync".to_owned() } else { format!("sent {sent} files ({}), removed {removed}", human(bytes)) };
                    say(format!("{what} in {:.1}s → {root}", ms as f64 / 1000.0));
                }
                Ev::Started { .. } => {}
                Ev::Output { err, data } => {
                    if err {
                        let mut e = std::io::stderr();
                        let _ = e.write_all(&data);
                        let _ = e.flush();
                    } else {
                        let mut o = std::io::stdout();
                        let _ = o.write_all(&data);
                        let _ = o.flush();
                    }
                }
                Ev::Exit { code, error, lost } => {
                    if lost > 0 {
                        say(format!("{lost} bytes of output were dropped"));
                    }
                    if let Some(e) = error {
                        anyhow::bail!("{e}");
                    }
                    return match code {
                        Some(0) => Ok(()),
                        Some(c) => Err(ExitWith(u8::try_from(c.clamp(1, 255)).unwrap_or(1)).into()),
                        None => Err(ExitWith(if interrupted { 130 } else { 137 }).into()),
                    };
                }
                Ev::Error(e) => anyhow::bail!("{e}"),
                Ev::Status(_) | Ev::Runners(_) | Ev::Forgot(_) | Ev::Answer(_) => {}
            }
        }
        use futures::FutureExt as _;
        enum Next {
            Read(std::io::Result<Option<usize>>),
            Stop,
        }
        let next = {
            let was = interrupted;
            let shutdown = shutdown.clone();
            let read = crate::os::proc::read_chunk(&fd, &mut buf).fuse();
            let stop = async move {
                if was {
                    futures::future::pending::<()>().await;
                } else {
                    shutdown.cancelled().await;
                }
            }
            .fuse();
            futures::pin_mut!(read, stop);
            futures::select_biased! {
                r = read => Next::Read(r),
                () = stop => Next::Stop,
            }
        };
        match next {
            Next::Read(Ok(Some(n))) if n > 0 => de.push(buf.get(..n).unwrap_or_default()),
            Next::Read(_) => anyhow::bail!("the kiln runner closed the connection"),
            Next::Stop => {
                // Ctrl-C: the command over there gets it too; its exit comes back as usual.
                interrupted = true;
                if let Ok(b) = super::wire::frame(&Req::Signal(Signal::Interrupt)) {
                    let _ = crate::os::proc::write_all(&fd, &b).await;
                }
            }
        }
    }
}

#[cfg(not(unix))]
pub async fn run(_a: RunArgs, _shutdown: crate::rt::Shutdown) -> anyhow::Result<()> {
    anyhow::bail!("kiln run needs a Unix machine for now")
}

/// One request, one answer.
#[cfg(unix)]
fn ask(req: &Req) -> anyhow::Result<Ev> {
    let mut c = super::local::Client::connect(&super::local::find_socket()?)?;
    c.send(req)?;
    c.event()?.ok_or_else(|| anyhow::anyhow!("the kiln runner closed the connection"))
}

#[cfg(not(unix))]
fn ask(_req: &Req) -> anyhow::Result<Ev> {
    anyhow::bail!("reach needs a Unix machine for now")
}

/// `kiln runners`.
pub fn runners(a: RunnersArgs) -> anyhow::Result<()> {
    let json = match ask(&Req::Runners { want: a.want.clone() })? {
        Ev::Runners(j) => j,
        Ev::Error(e) => anyhow::bail!("{e}"),
        other => anyhow::bail!("unexpected answer {other:?}"),
    };
    if a.json {
        println!("{json}");
        return Ok(());
    }
    let v: serde_json::Value = serde_json::from_str(&json)?;
    let me = v.get("me").and_then(serde_json::Value::as_str).unwrap_or_default();
    let list = v.get("runners").and_then(serde_json::Value::as_array).cloned().unwrap_or_default();
    if list.is_empty() {
        println!("No runner suits {}.", v.get("want").and_then(serde_json::Value::as_str).unwrap_or("that"));
    }
    for c in &list {
        let r = c.get("runner").cloned().unwrap_or_default();
        let name = r.get("name").and_then(serde_json::Value::as_str).unwrap_or("?");
        let why = c.get("why").and_then(serde_json::Value::as_str).unwrap_or("");
        let h: Vec<String> = r.get("harnesses").and_then(serde_json::Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect()).unwrap_or_default();
        let mark = if name == me { " (this machine)" } else { "" };
        let harnesses = if h.is_empty() { String::new() } else { format!(" · {}", h.join(", ")) };
        println!("{name}{mark}  {why}{harnesses}");
    }
    let passed = v.get("passed").and_then(serde_json::Value::as_array).cloned().unwrap_or_default();
    if !passed.is_empty() {
        println!();
        for p in passed {
            let (n, w) = (p.get(0).and_then(serde_json::Value::as_str).unwrap_or("?"), p.get(1).and_then(serde_json::Value::as_str).unwrap_or(""));
            println!("\x1b[2m{n}: {w}\x1b[0m");
        }
    }
    Ok(())
}

/// `kiln reach`.
pub fn reach(a: ReachArgs) -> anyhow::Result<()> {
    match a.cmd {
        ReachCmd::Status => match ask(&Req::Status)? {
            Ev::Status(j) => {
                let v: serde_json::Value = serde_json::from_str(&j)?;
                println!("{}", serde_json::to_string_pretty(&v)?);
                Ok(())
            }
            other => anyhow::bail!("unexpected answer {other:?}"),
        },
        #[cfg(unix)]
        ReachCmd::Mcp => super::mcp::serve(),
        #[cfg(not(unix))]
        ReachCmd::Mcp => anyhow::bail!("reach needs a Unix machine for now"),
        ReachCmd::Forget { runner } => match ask(&Req::Forget { runner: runner.clone() })? {
            Ev::Forgot(true) => {
                println!("Forgot {runner}'s key; the next one it presents is trusted.");
                Ok(())
            }
            Ev::Forgot(false) => {
                println!("No key was pinned for {runner}.");
                Ok(())
            }
            other => anyhow::bail!("unexpected answer {other:?}"),
        },
    }
}
