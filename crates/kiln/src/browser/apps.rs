//! What a person opens live from a session (docs/previews.md, "Opening a page"): the ports this
//! machine serves (the agent's own processes first), and the project's app, started as its
//! `.kiln/preview.yaml` says:
//!
//! ```yaml
//! start: npm run dev     # how the app starts, in the session's folder
//! port: 5173             # where it listens
//! path: /                # the page to open (/ by default)
//! env: { BROWSER: none } # what it's started with besides
//! ready_s: 60            # how long it may take to listen
//! ```
//!
//! An app kiln started runs until it's stopped, or its session closes.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// A port this machine listens on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Listening {
    pub port: u16,
    /// What it listens on (`127.0.0.1`, `0.0.0.0`, `::1`, `::`).
    pub address: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    pub command: String,
    /// One of the session's own processes (its agent's, or what they started).
    pub agents: bool,
}

/// A project's `.kiln/preview.yaml`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Preview {
    pub start: String,
    pub port: u16,
    #[serde(default = "slash")]
    pub path: String,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub ready_s: Option<u64>,
}

fn slash() -> String {
    "/".to_owned()
}

/// A project's preview file (none: it has none; an error: it says something kiln can't read).
pub fn config(cwd: &Path) -> Option<Result<Preview, String>> {
    let f = [".kiln/preview.yaml", ".kiln/preview.yml"].iter().map(|n| cwd.join(n)).find(|p| p.is_file())?;
    Some(std::fs::read_to_string(&f).map_err(|e| format!("{}: {e}", f.display())).and_then(|t| serde_yaml::from_str::<Preview>(&t).map_err(|e| format!("{}: {e}", f.display()))))
}

/// An app kiln started for a session.
struct App {
    child: std::process::Child,
    url: String,
    log: PathBuf,
}

static APPS: std::sync::OnceLock<Mutex<HashMap<String, App>>> = std::sync::OnceLock::new();

fn apps() -> &'static Mutex<HashMap<String, App>> {
    APPS.get_or_init(Mutex::default)
}

/// Whether something listens on `port` here, on either loopback address (a dev server asked for
/// `localhost` often listens on `::1` alone).
fn listens(port: u16) -> bool {
    let v4 = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let v6 = std::net::SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port));
    [v4, v6].iter().any(|a| std::net::TcpStream::connect_timeout(a, Duration::from_millis(300)).is_ok())
}

/// A session's app, while it runs: its address.
fn running(session: &str) -> Option<String> {
    let mut g = apps().lock();
    let alive = g.get_mut(session).is_some_and(|a| matches!(a.child.try_wait(), Ok(None)));
    if !alive {
        g.remove(session);
    }
    g.get(session).map(|a| a.url.clone())
}

/// What a person may open from session `session` (its folder `cwd`, its harness's process `root`):
/// the ports this machine serves, and its app.
pub fn look(session: &str, cwd: &Path, root: Option<u32>) -> Value {
    let app = match config(cwd) {
        None => json!({"configured": false}),
        Some(Err(e)) => json!({"configured": true, "error": e}),
        Some(Ok(p)) => json!({"configured": true, "start": p.start, "port": p.port, "path": p.path, "running": running(session)}),
    };
    json!({"ports": listening(root), "app": app, "engines": super::driven::available(), "devices": super::devices::DEVICES})
}

/// Start session `session`'s app (its folder `cwd`), unless it runs already, and wait for it to
/// listen: its address.
pub async fn start(dir: &Path, session: &str, cwd: &Path) -> anyhow::Result<Value> {
    if let Some(url) = running(session) {
        return Ok(json!({"url": url, "started": false}));
    }
    let p = match config(cwd) {
        None => anyhow::bail!("{} has no .kiln/preview.yaml: write one that says how the app starts (start), and where it listens (port)", cwd.display()),
        Some(Err(e)) => anyhow::bail!("{e}"),
        Some(Ok(p)) => p,
    };
    let logs = dir.join("browser").join("apps");
    std::fs::create_dir_all(&logs)?;
    let key: String = session.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(64).collect();
    let log = logs.join(format!("{key}.log"));
    let out = std::fs::File::create(&log)?;
    let err = out.try_clone()?;
    #[cfg(unix)]
    let mut cmd = {
        use std::os::unix::process::CommandExt as _;
        let mut c = std::process::Command::new("sh");
        c.arg("-c").arg(&p.start).process_group(0);
        c
    };
    #[cfg(not(unix))]
    let mut cmd = {
        let mut c = std::process::Command::new("cmd");
        c.arg("/C").arg(&p.start);
        c
    };
    cmd.current_dir(cwd).envs(&p.env).stdin(std::process::Stdio::null()).stdout(out).stderr(err);
    let mut child = cmd.spawn().map_err(|e| anyhow::anyhow!("{} didn't start: {e}", p.start))?;
    let url = format!("http://localhost:{}{}", p.port, if p.path.starts_with('/') { p.path.clone() } else { format!("/{}", p.path) });
    let deadline = std::time::Instant::now() + Duration::from_secs(p.ready_s.unwrap_or(60).clamp(1, 600));
    loop {
        let port = p.port;
        if crate::rt::blocking(move || listens(port)).await.unwrap_or(false) {
            break;
        }
        if let Ok(Some(status)) = child.try_wait() {
            anyhow::bail!("{} ended ({status}) before it listened on {}: {}", p.start, p.port, tail(&log));
        }
        if std::time::Instant::now() > deadline {
            kill(&mut child);
            anyhow::bail!("{} didn't listen on {} in time: {}", p.start, p.port, tail(&log));
        }
        crate::rt::sleep(Duration::from_millis(300)).await;
    }
    let pid = child.id();
    apps().lock().insert(session.to_owned(), App { child, url: url.clone(), log: log.clone() });
    Ok(json!({"url": url, "started": true, "pid": pid, "log": log}))
}

/// `ServerMsg::Preview` on a runner: `{"op": "look" | "start" | "stop"}`, its answer.
pub async fn serve(dir: &Path, session: &str, cwd: &Path, root: Option<u32>, json: &str) -> Value {
    let q: Value = serde_json::from_str(json).unwrap_or_default();
    match q.get("op").and_then(Value::as_str) {
        Some("look") => {
            let (s, c) = (session.to_owned(), cwd.to_path_buf());
            crate::rt::blocking(move || look(&s, &c, root)).await.unwrap_or_else(|e| json!({"error": format!("{e:#}")}))
        }
        Some("start") => start(dir, session, cwd).await.unwrap_or_else(|e| json!({"error": format!("{e:#}")})),
        Some("stop") => json!({"stopped": stop(session)}),
        other => json!({"error": format!("previews don't do {other:?}")}),
    }
}

/// Stop session `session`'s app (and what it started).
pub fn stop(session: &str) -> bool {
    let Some(mut a) = apps().lock().remove(session) else { return false };
    kill(&mut a.child);
    let _ = a.log;
    true
}

fn kill(child: &mut std::process::Child) {
    #[cfg(unix)]
    if let Ok(pid) = i32::try_from(child.id()) {
        let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid), nix::sys::signal::Signal::SIGTERM);
        std::thread::sleep(Duration::from_millis(200));
        let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid), nix::sys::signal::Signal::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// A log's last lines.
fn tail(log: &Path) -> String {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let n = lines.len();
    lines.get(n.saturating_sub(6)..).unwrap_or_default().join(" | ").chars().take(800).collect()
}

/// The ports this machine listens on, the ones `root`'s processes (and theirs) hold marked; not
/// kiln's own. In order: the agent's first, then by port.
pub fn listening(root: Option<u32>) -> Vec<Listening> {
    let mut v = sockets();
    let tree = root.map(descendants).unwrap_or_default();
    let me = std::process::id();
    v.retain(|l| l.pid != Some(me));
    for l in &mut v {
        l.agents = l.pid.is_some_and(|p| tree.contains(&p));
    }
    // The system's services (ssh, printing, a name server) aren't pages to open.
    v.retain(|l| l.agents || l.port >= 1024);
    let mut seen = HashSet::new();
    v.retain(|l| seen.insert(l.port));
    v.sort_by(|a, b| b.agents.cmp(&a.agents).then(a.port.cmp(&b.port)));
    v
}

/// `root` and every process under it.
fn descendants(root: u32) -> HashSet<u32> {
    let parents = parents();
    let mut out = HashSet::from([root]);
    loop {
        let before = out.len();
        for (pid, ppid) in &parents {
            if out.contains(ppid) {
                out.insert(*pid);
            }
        }
        if out.len() == before {
            return out;
        }
    }
}

#[cfg(target_os = "linux")]
fn parents() -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    for e in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else { continue };
        let Ok(stat) = std::fs::read_to_string(e.path().join("stat")) else { continue };
        // `pid (comm) state ppid ...`: after the command's closing parenthesis.
        let Some(rest) = stat.rsplit_once(')').map(|(_, r)| r) else { continue };
        if let Some(ppid) = rest.split_whitespace().nth(1).and_then(|p| p.parse::<u32>().ok()) {
            out.push((pid, ppid));
        }
    }
    out
}

#[cfg(target_os = "macos")]
fn parents() -> Vec<(u32, u32)> {
    let out = std::process::Command::new("ps").args(["-axo", "pid=,ppid="]).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    out.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
        })
        .collect()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn parents() -> Vec<(u32, u32)> {
    Vec::new()
}

/// Listening sockets, read from `/proc`: their port, address, and the process holding each.
#[cfg(target_os = "linux")]
fn sockets() -> Vec<Listening> {
    let mut by_inode: HashMap<u64, (u16, String)> = HashMap::new();
    for (file, v6) in [("/proc/net/tcp", false), ("/proc/net/tcp6", true)] {
        let Ok(text) = std::fs::read_to_string(file) else { continue };
        for line in text.lines().skip(1) {
            let cols: Vec<&str> = line.split_whitespace().collect();
            // sl local rem st queues timer retrnsmt uid timeout inode
            if cols.get(3) != Some(&"0A") {
                continue;
            }
            let (Some(local), Some(inode)) = (cols.get(1), cols.get(9).and_then(|i| i.parse::<u64>().ok())) else { continue };
            let Some((addr, port)) = local.split_once(':') else { continue };
            let Ok(port) = u16::from_str_radix(port, 16) else { continue };
            by_inode.insert(inode, (port, hex_address(addr, v6)));
        }
    }
    let mut out = Vec::new();
    let mut found = HashSet::new();
    for e in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else { continue };
        let Ok(fds) = std::fs::read_dir(e.path().join("fd")) else { continue };
        for fd in fds.flatten() {
            let Ok(link) = std::fs::read_link(fd.path()) else { continue };
            let l = link.to_string_lossy();
            let Some(inode) = l.strip_prefix("socket:[").and_then(|r| r.strip_suffix(']')).and_then(|i| i.parse::<u64>().ok()) else { continue };
            if let Some((port, address)) = by_inode.get(&inode)
                && found.insert(inode)
            {
                let command = std::fs::read(e.path().join("cmdline")).map(|c| String::from_utf8_lossy(&c).replace('\0', " ").trim().chars().take(160).collect()).unwrap_or_default();
                out.push(Listening { port: *port, address: address.clone(), pid: Some(pid), command, agents: false });
            }
        }
    }
    out
}

/// A `/proc/net/tcp` address (hex, the kernel's byte order) as text.
#[cfg(target_os = "linux")]
fn hex_address(hex: &str, v6: bool) -> String {
    let bytes: Vec<u8> = (0..hex.len() / 2).filter_map(|i| hex.get(i * 2..i * 2 + 2).and_then(|b| u8::from_str_radix(b, 16).ok())).collect();
    if !v6 {
        return match bytes.as_slice() {
            [a, b, c, d] => format!("{d}.{c}.{b}.{a}"),
            _ => hex.to_owned(),
        };
    }
    // Four 32-bit words, each in the kernel's (little-endian) order.
    let mut words = [0u8; 16];
    for (i, chunk) in bytes.chunks(4).enumerate().take(4) {
        for (j, b) in chunk.iter().rev().enumerate() {
            if let Some(w) = words.get_mut(i * 4 + j) {
                *w = *b;
            }
        }
    }
    std::net::Ipv6Addr::from(words).to_string()
}

/// Listening sockets, as `lsof` says them.
#[cfg(target_os = "macos")]
fn sockets() -> Vec<Listening> {
    let out = std::process::Command::new("lsof").args(["-nP", "-iTCP", "-sTCP:LISTEN", "-Fpcn"]).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    let (mut pid, mut command) = (None, String::new());
    let mut v = Vec::new();
    for line in out.lines() {
        match line.split_at_checked(1) {
            Some(("p", p)) => pid = p.parse().ok(),
            Some(("c", c)) => command = c.to_owned(),
            Some(("n", n)) => {
                if let Some((addr, port)) = n.rsplit_once(':')
                    && let Ok(port) = port.parse::<u16>()
                {
                    v.push(Listening { port, address: addr.trim_matches(['[', ']']).replace('*', "0.0.0.0"), pid, command: command.clone(), agents: false });
                }
            }
            _ => {}
        }
    }
    v
}

/// Listening sockets, as `netstat` says them.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn sockets() -> Vec<Listening> {
    let out = std::process::Command::new("netstat").args(["-ano", "-p", "TCP"]).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    out.lines()
        .filter_map(|l| {
            let cols: Vec<&str> = l.split_whitespace().collect();
            if cols.first() != Some(&"TCP") || cols.get(3) != Some(&"LISTENING") {
                return None;
            }
            let (addr, port) = cols.get(1)?.rsplit_once(':')?;
            Some(Listening { port: port.parse().ok()?, address: addr.to_owned(), pid: cols.get(4).and_then(|p| p.parse().ok()), command: String::new(), agents: false })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_projects_preview_file_says_how_its_app_starts() -> anyhow::Result<()> {
        let dir = std::env::temp_dir().join(format!("kiln-preview-yaml-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".kiln"))?;
        assert!(config(&dir).is_none());
        std::fs::write(dir.join(".kiln/preview.yaml"), "start: npm run dev\nport: 5173\nenv: { BROWSER: none }\n")?;
        let p = config(&dir).and_then(Result::ok).ok_or_else(|| anyhow::anyhow!("no config"))?;
        assert_eq!((p.start.as_str(), p.port, p.path.as_str(), p.env.get("BROWSER").map(String::as_str)), ("npm run dev", 5173, "/", Some("none")));
        std::fs::write(dir.join(".kiln/preview.yaml"), "port: nope\n")?;
        assert!(config(&dir).is_some_and(|r| r.is_err()));
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[test]
    fn an_app_on_either_loopback_address_is_listening() -> anyhow::Result<()> {
        let v4 = std::net::TcpListener::bind("127.0.0.1:0")?;
        assert!(listens(v4.local_addr()?.port()));
        // Where IPv6 is there at all.
        if let Ok(v6) = std::net::TcpListener::bind("[::1]:0") {
            assert!(listens(v6.local_addr()?.port()));
        }
        Ok(())
    }

    #[test]
    fn a_port_this_process_opens_is_listed_with_it_and_marked_its() -> anyhow::Result<()> {
        // A listener of a child of this process (kiln's own ports aren't listed).
        let l = std::net::TcpListener::bind("127.0.0.1:0")?;
        let port = l.local_addr()?.port();
        let mine = listening(Some(std::process::id()));
        // This process's own socket is left out: it's kiln's.
        assert!(!mine.iter().any(|x| x.port == port), "{mine:?}");
        drop(l);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn kernel_addresses_read_as_text() {
        assert_eq!(hex_address("0100007F", false), "127.0.0.1");
        assert_eq!(hex_address("00000000", false), "0.0.0.0");
        assert_eq!(hex_address("00000000000000000000000001000000", true), "::1");
    }

    /// An app started as its preview file says: it listens, it's listed as the session's, it
    /// stops.
    #[cfg(unix)]
    #[test]
    fn an_app_starts_listens_and_stops() -> anyhow::Result<()> {
        if std::process::Command::new("python3").arg("--version").output().is_err() {
            eprintln!("skipped: no python3 here to serve");
            return Ok(());
        }
        let dir = std::env::temp_dir().join(format!("kiln-app-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".kiln"))?;
        let port = std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
        std::fs::write(dir.join(".kiln/preview.yaml"), format!("start: exec python3 -m http.server {port} --bind 127.0.0.1\nport: {port}\npath: /index.html\nready_s: 20\n"))?;
        std::fs::write(dir.join("index.html"), "<p>hi</p>")?;
        let (d, w) = (dir.clone(), dir.clone());
        let started = crate::rt::block_on(async move { start(&d, "s-app", &w).await })?;
        assert_eq!(started.get("url").and_then(Value::as_str), Some(format!("http://localhost:{port}/index.html").as_str()));
        let pid = started.get("pid").and_then(Value::as_u64).and_then(|p| u32::try_from(p).ok());
        let seen = listening(pid);
        assert!(seen.iter().any(|l| l.port == port && l.agents), "{seen:?}");
        // Asked again, it's the one running.
        let (d, w) = (dir.clone(), dir.clone());
        let again = crate::rt::block_on(async move { start(&d, "s-app", &w).await })?;
        assert_eq!(again.get("started"), Some(&json!(false)));
        assert!(stop("s-app"));
        assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_err(), "it stopped listening");
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }
}
