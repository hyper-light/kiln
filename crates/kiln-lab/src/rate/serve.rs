//! A target's server, up and seeded: kiln as `kiln-lab run` starts it (its fleet: the server, the lab's runners
//! with the stand-in agent, and the profile's sessions filled in), or a target's own command, waited for and then
//! seeded by its seed command. Each on a free port of its own (never 7070, production's) with a fresh directory.

use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::fleet::{Fleet, Proc};
use crate::lab::{Build, Place, Profile};

use super::target::{self, Pick, Ready, Template, Token, Value, Vars};

/// The port production's kiln answers on (127.0.0.1:7070), which nothing of the lab's may take.
const PRODUCTION_PORT: u16 = 7070;

/// How often a starting server is looked at: as `crate::fleet` looks at kiln's.
const LOOK: Duration = Duration::from_millis(100);

/// A server, up: its port, its root process (sampled), what its templates name, and how long its fill took.
pub struct Up {
    pub port: u16,
    pub pid: u32,
    pub vars: Vars,
    pub fill_s: Option<f64>,
    /// Its processes, stopped when this goes.
    _running: Running,
}

enum Running {
    Fleet(Box<Fleet>),
    Proc(Proc),
}

/// A free port on 127.0.0.1, never production's.
pub fn free_port() -> anyhow::Result<u16> {
    for _ in 0..8 {
        let port = TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
        if port != PRODUCTION_PORT {
            return Ok(port);
        }
    }
    anyhow::bail!("no free port but {PRODUCTION_PORT}")
}

/// 32 hex digits no one else has: the OS's random bytes, else a digest of this moment and process.
fn fresh_token() -> String {
    let mut bytes = [0u8; 16];
    let read = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes));
    if read.is_ok() {
        return hex::encode(bytes);
    }
    use sha2::Digest as _;
    let seed = format!("{:?} {} {:p}", std::time::SystemTime::now(), std::process::id(), &bytes);
    hex::encode(sha2::Sha256::digest(seed)).chars().take(32).collect()
}

/// `~/x` as the home it names.
fn expand(p: &str) -> PathBuf {
    match (p.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(p),
    }
}

/// kiln of `build` in `dir`, as `kiln-lab run` starts a round's (its server placed as `place` says), its runners
/// connected and `profile`'s sessions filled in: `${session}` is their ids, largest first; `${token}` its admin
/// token.
pub fn kiln(build: &Build, profile: &Profile, place: &Place, dir: &Path, bin: &Path, unit: &str) -> anyhow::Result<Up> {
    std::fs::create_dir_all(dir)?;
    let mut fleet = Fleet::server(&build.kiln, dir, &build.env, build.bus_memory, place, unit)?;
    let names = crate::lab::runners(&mut fleet, build, profile, bin)?;
    let (mut opened, fill_s) = crate::lab::fill(&fleet, profile, &names)?;
    opened.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    let mut vars = Vars::default();
    vars.set("port", Value::One(fleet.port.to_string()));
    vars.set("dir", Value::One(dir.display().to_string()));
    vars.set("token", Value::One(fleet.admin.clone()));
    vars.set("session", Value::List(opened.into_iter().map(|(id, _)| id).collect()));
    Ok(Up { port: fleet.port, pid: fleet.server.child.id(), vars, fill_s: Some(fill_s), _running: Running::Fleet(Box::new(fleet)) })
}

fn render(t: &Template, vars: &Vars) -> anyhow::Result<String> {
    t.render(vars, Pick::First, 0, 0).map_err(anyhow::Error::msg)
}

/// What a target's command and seed run with: its cwd, and its environment (on top of the lab's, less what points
/// at another kiln): `HOME` in its directory unless it sets its own, and `KILN_LAB_PORT`, `KILN_LAB_DIR` and
/// `KILN_LAB_TOKEN`.
fn prepare(cmd: &mut Command, c: &target::Command, vars: &Vars, dir: &Path) -> anyhow::Result<()> {
    if let Some(cwd) = &c.cwd {
        cmd.current_dir(expand(&render(cwd, vars)?));
    }
    cmd.env("HOME", dir.join("home")).env("KILN_LAB_PORT", vars.one("port").unwrap_or("")).env("KILN_LAB_DIR", dir).env("KILN_LAB_TOKEN", vars.one("token").unwrap_or(""));
    for (k, v) in &c.env {
        cmd.env(k, render(v, vars)?);
    }
    Ok(())
}

/// Whether the server on `port` answers as `ready` asks.
fn answers(port: u16, ready: &Ready) -> bool {
    let Ok(mut s) = TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_secs(1)) else { return false };
    let Ready::Get(path) = ready else { return true };
    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
    if s.write_all(format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n").as_bytes()).is_err() {
        return false;
    }
    let mut head = [0u8; 64];
    let n = s.read(&mut head).unwrap_or(0);
    let line = String::from_utf8_lossy(head.get(..n).unwrap_or_default()).into_owned();
    line.split_whitespace().nth(1).and_then(|c| c.parse::<u16>().ok()).is_some_and(|c| c < 500)
}

/// A target's own server: its command run in `dir` (its output in `server.log`), waited for until it answers, its
/// token read where it writes one, then its seed run (its output in `seed.out` and `seed.log`) and what that
/// printed taken as the workloads' values.
pub fn command(c: &target::Command, place: &Place, dir: &Path, unit: &str) -> anyhow::Result<Up> {
    std::fs::create_dir_all(dir.join("home"))?;
    let port = free_port()?;
    let mut vars = Vars::default();
    vars.set("port", Value::One(port.to_string()));
    vars.set("dir", Value::One(dir.display().to_string()));
    if c.token == Token::Generate {
        vars.set("token", Value::One(fresh_token()));
    }
    let argv: Vec<String> = c.argv.iter().map(|a| render(a, &vars)).collect::<anyhow::Result<_>>()?;
    let (program, args) = argv.split_first().ok_or_else(|| anyhow::anyhow!("an empty command"))?;
    let log = std::fs::File::create(dir.join("server.log"))?;
    let mut cmd = crate::fleet::clean(place.server(Path::new(program), unit));
    cmd.args(args).stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
    prepare(&mut cmd, c, &vars, dir)?;
    let mut server = Proc { child: cmd.spawn().map_err(|e| anyhow::anyhow!("{program}: {e}"))? };
    let deadline = Instant::now() + c.ready_within;
    loop {
        if let Some(status) = server.child.try_wait()? {
            anyhow::bail!("it exited ({status}) before answering: {}", crate::fleet::tail(&dir.join("server.log")));
        }
        if answers(port, &c.ready) {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "it didn't answer within {:?} on {port}: {}", c.ready_within, crate::fleet::tail(&dir.join("server.log")));
        std::thread::sleep(LOOK);
    }
    if let Token::File(f) = &c.token {
        let path = expand(&render(f, &vars)?);
        let token = std::fs::read_to_string(&path).map_err(|e| anyhow::anyhow!("its token file {}: {e}", path.display()))?.trim().to_owned();
        anyhow::ensure!(!token.is_empty(), "its token file {} is empty", path.display());
        vars.set("token", Value::One(token));
    }
    if let Some(seed) = &c.seed {
        for (k, v) in run_seed(seed, c, &vars, dir)? {
            anyhow::ensure!(!target::BUILT_IN.contains(&k.as_str()), "the seed printed {k}, which kiln-lab gives");
            vars.set(&k, v);
        }
    }
    Ok(Up { port, pid: server.child.id(), vars, fill_s: None, _running: Running::Proc(server) })
}

/// Runs a target's seed to its end (within its time): the values its last line gave.
fn run_seed(seed: &[Template], c: &target::Command, vars: &Vars, dir: &Path) -> anyhow::Result<Vec<(String, Value)>> {
    let argv: Vec<String> = seed.iter().map(|a| render(a, vars)).collect::<anyhow::Result<_>>()?;
    let (program, args) = argv.split_first().ok_or_else(|| anyhow::anyhow!("an empty seed"))?;
    let (out, err) = (std::fs::File::create(dir.join("seed.out"))?, std::fs::File::create(dir.join("seed.log"))?);
    let mut cmd = crate::fleet::clean(Command::new(program));
    cmd.args(args).stdin(Stdio::null()).stdout(out).stderr(err);
    prepare(&mut cmd, c, vars, dir)?;
    let started = Instant::now();
    let mut child = Proc { child: cmd.spawn().map_err(|e| anyhow::anyhow!("the seed {program}: {e}"))? };
    let status = loop {
        if let Some(s) = child.child.try_wait()? {
            break s;
        }
        anyhow::ensure!(started.elapsed() < c.seed_within, "the seed didn't finish within {:?}: {}", c.seed_within, crate::fleet::tail(&dir.join("seed.log")));
        std::thread::sleep(LOOK);
    };
    anyhow::ensure!(status.success(), "the seed exited {status}: {}", crate::fleet::tail(&dir.join("seed.log")));
    eprintln!("kiln-lab rate: seeded in {:.1} s", started.elapsed().as_secs_f64());
    Vars::of_seed(&std::fs::read_to_string(dir.join("seed.out"))?).map_err(anyhow::Error::msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Goal 10 (Ada's fixed-rate benchmark): a command target is started on a free port of its own (never
    /// 7070) with its templates filled, waited for until it answers, seeded, and its seed's values kept; it's
    /// stopped when it's let go. (Unix: the server is a shell's `nc`-free loop, a Python one where Python is.)
    #[test]
    fn rate_a_command_target_starts_answers_and_is_seeded() -> anyhow::Result<()> {
        if !cfg!(unix) || Command::new("python3").arg("--version").output().is_err() {
            return Ok(());
        }
        let dir = std::env::temp_dir().join(format!("kiln-lab-rate-serve-{}", std::process::id()));
        let text = r#"
name = "toy"
[server]
kind = "command"
command = ["python3", "-c", "import http.server as h, os; h.HTTPServer(('127.0.0.1', int(os.environ['KILN_LAB_PORT'])), h.SimpleHTTPRequestHandler).serve_forever()", "${port}"]
ready = "/"
token = "generate"
seed = ["python3", "-c", "import os; print('noise'); print('{\"thread\": [\"a\", \"b\"], \"tok\": ' + str(len(os.environ['KILN_LAB_TOKEN'])) + '}')"]

[[workload]]
name = "x"
kind = "http"
path = "/${thread}"
"#;
        let t = target::Target::parse(text).map_err(anyhow::Error::msg)?;
        let target::Server::Command(c) = &t.server else { anyhow::bail!("a command target") };
        let place = Place::of(None, "shared")?;
        let up = command(c, &place, &dir, "kiln-lab-test")?;
        assert_ne!(up.port, PRODUCTION_PORT);
        assert!(answers(up.port, &Ready::Get("/".into())));
        t.check(&|n| up.vars.has(n)).map_err(anyhow::Error::msg)?;
        let thread = Template::parse("${thread}/${tok}").map_err(anyhow::Error::msg)?.render(&up.vars, Pick::Cycle, 0, 1).map_err(anyhow::Error::msg)?;
        assert_eq!(thread, "b/32", "the second thread in turn, and the token's 32 digits reached the seed");
        let port = up.port;
        drop(up);
        let deadline = Instant::now() + Duration::from_secs(10);
        while answers(port, &Ready::Tcp) && Instant::now() < deadline {
            std::thread::sleep(LOOK);
        }
        assert!(!answers(port, &Ready::Tcp), "stopped when let go");
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }
}
