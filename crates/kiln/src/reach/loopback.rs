//! kiln's MCP for a harness that runs as a process of a Windows runner (docs/reach.md §9, docs/agents.md): where a Unix
//! runner's harness reaches it through `kiln reach mcp` over the runner's reach socket, a Windows runner has no Unix
//! socket, so its harness got no kiln MCP at all (an agent there could not land, raise its mode or start another; the
//! bare-metal drive on Windows: "LAND_FAIL no kiln MCP server was given to this harness", GitHub run 44807509237).
//!
//! The runner listens on the loopback alone (`127.0.0.1`, a port the system picks) and serves each connection in its
//! own process as an isolated session's lane is served (`reach::mcp::serve_lane`, every platform's): whose a
//! connection is comes from the token it presents first, never from where it came from. Each session's launch is
//! granted a token of its own ([`grant`], 32 random bytes; granting again replaces the last), which its harness is
//! handed in the environment of the `kiln reach mcp` it starts (its MCP config, a file only this user reads); a
//! connection whose first line names no granted token is closed unanswered. `kiln reach mcp` there pipes its standard
//! input and output over the connection ([`pipe`]).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde_json::{Value, json};

/// The variable naming the runner's loopback address for `kiln reach mcp`.
pub const ADDR_ENV: &str = "KILN_REACH_ADDR";
/// The variable carrying the session's token for `kiln reach mcp`.
pub const TOKEN_ENV: &str = "KILN_REACH_TOKEN";

/// How long a connection has to say whose it is before it is closed: a local process's first line.
const HELLO_WITHIN: Duration = Duration::from_secs(10);

/// The longest first line a connection may send (its token and folder).
const HELLO_BYTES: u64 = 64 * 1024;

/// What a connection says first: its token, and its harness's folder.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct Hello {
    token: String,
    cwd: String,
}

/// The tokens granted, each with its session.
#[derive(Default)]
struct Grants {
    by_session: std::collections::HashMap<String, String>,
}

impl Grants {
    /// A new token for `session`, replacing the one it held.
    fn grant(&mut self, session: &str) -> String {
        let token = crate::util::random_token();
        self.by_session.insert(session.to_owned(), token.clone());
        token
    }

    /// The session whose token `token` is.
    fn session_of(&self, token: &str) -> Option<String> {
        self.by_session.iter().find(|(_, t)| crate::util::ct_eq(t.as_bytes(), token.as_bytes())).map(|(s, _)| s.clone())
    }
}

/// The runner's listener, once a launch first needed it.
struct Loopback {
    addr: SocketAddr,
    grants: Mutex<Grants>,
}

static LOOPBACK: OnceLock<Loopback> = OnceLock::new();

/// The loopback this runner serves kiln's MCP on, started at its first use, and `session`'s new token for it. None
/// where this process runs no reach node (a server's own agents get its kit instead), or where nothing can listen.
pub fn grant(session: &str) -> Option<(SocketAddr, String)> {
    crate::reach::node::global()?;
    let lb = match LOOPBACK.get() {
        Some(lb) => lb,
        None => {
            let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).map_err(|e| tracing::warn!(error = %e, "kiln's MCP for this runner's harnesses can't listen on the loopback")).ok()?;
            let addr = listener.local_addr().ok()?;
            if LOOPBACK.set(Loopback { addr, grants: Mutex::new(Grants::default()) }).is_ok() {
                accept(listener);
            }
            LOOPBACK.get()?
        }
    };
    let token = lb.grants.lock().ok()?.grant(session);
    Some((lb.addr, token))
}

/// Takes each connection on a thread of its own, for as long as this process runs.
fn accept(listener: TcpListener) {
    let started = std::thread::Builder::new().name("kiln-mcp-loopback".into()).spawn(move || {
        for conn in listener.incoming() {
            let Ok(stream) = conn else { continue };
            if let Err(e) = std::thread::Builder::new().name("kiln-mcp-loopback-conn".into()).spawn(move || connection(stream)) {
                tracing::warn!(error = %e, "a connection to kiln's MCP let go of: no thread to serve it");
            }
        }
    });
    if let Err(e) = started {
        tracing::warn!(error = %e, "kiln's MCP for this runner's harnesses takes no connections: no thread");
    }
}

/// One connection: its first line says whose it is ([`Hello`]); then MCP, as its session's lane.
fn connection(stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(HELLO_WITHIN));
    let Ok(writer) = stream.try_clone() else { return };
    let mut reader = BufReader::new(stream);
    let Some(hello) = hello(&mut reader) else { return };
    let session = LOOPBACK.get().and_then(|lb| lb.grants.lock().ok().and_then(|g| g.session_of(&hello.token)));
    let (Some(session), Some(handle)) = (session, crate::reach::node::global()) else {
        tracing::warn!("a connection to kiln's MCP named no token this runner granted: closed");
        return;
    };
    let _ = reader.get_ref().set_read_timeout(None);
    let here = crate::reach::local::Here { handle: handle.clone(), me: handle.name().to_owned(), host: None, session: session.clone() };
    if let Err(e) = crate::reach::mcp::serve_lane(here, hello.cwd, &mut reader, Box::new(writer)) {
        tracing::warn!(%session, error = %format!("{e:#}"), "a connection to kiln's MCP ended in error");
    }
}

/// A connection's first line, read within its bound.
fn hello(reader: &mut impl BufRead) -> Option<Hello> {
    let mut line = String::new();
    reader.take(HELLO_BYTES).read_line(&mut line).ok()?;
    serde_json::from_str(line.trim_end()).ok()
}

/// The environment of the `kiln reach mcp` session `session`'s harness starts: the runner's loopback, its token, the
/// session's preferred runner and the session.
fn env_of(addr: SocketAddr, token: &str, prefer: Option<&str>, session: &str) -> serde_json::Map<String, Value> {
    let mut env = serde_json::Map::new();
    env.insert(ADDR_ENV.into(), json!(addr.to_string()));
    env.insert(TOKEN_ENV.into(), json!(token));
    if let Some(p) = prefer {
        env.insert("KILN_REACH_ON".into(), json!(p));
    }
    if !session.is_empty() {
        env.insert("KILN_SESSION".into(), json!(session));
    }
    env
}

/// Claude Code's MCP config for session `session`'s harness: `kiln reach mcp` over this runner's loopback.
pub fn claude_config_json(kiln: &std::path::Path, addr: SocketAddr, token: &str, prefer: Option<&str>, session: &str) -> Value {
    json!({"mcpServers": {"kiln": {"type": "stdio", "command": kiln, "args": ["reach", "mcp"], "env": env_of(addr, token, prefer, session)}}})
}

/// Claude Code's `--mcp-config` for session `session`'s harness on this runner, a file only this user reads (a
/// harness on Windows starts through `cmd.exe`, which a JSON argument's quotes don't cross whole): None where this
/// runner serves no loopback ([`grant`]).
pub fn claude_config(kiln: &std::path::Path, prefer: Option<&str>, session: &str) -> Option<std::path::PathBuf> {
    let (addr, token) = grant(session)?;
    let safe: String = session.chars().filter(char::is_ascii_alphanumeric).take(64).collect();
    let path = std::env::temp_dir().join("kiln-mcp").join(format!("{}.json", if safe.is_empty() { "session" } else { &safe }));
    let body = serde_json::to_vec(&claude_config_json(kiln, addr, &token, prefer, session)).ok()?;
    match crate::util::write_secret(&path, &body, 0o600) {
        Ok(()) => Some(path),
        Err(e) => {
            tracing::warn!(%session, error = %e, "its harness's kiln MCP config couldn't be written");
            None
        }
    }
}

/// `kiln reach mcp` where its runner serves the loopback: says whose it is, then carries `input` to the runner and
/// the runner's answers to `output` until either ends.
pub fn pipe(addr: &str, token: &str, cwd: &str, input: impl Read + Send + 'static, mut output: impl Write) -> anyhow::Result<()> {
    let mut stream = TcpStream::connect(addr).map_err(|e| anyhow::anyhow!("cannot reach the kiln runner's MCP at {addr}: {e}"))?;
    let hello = serde_json::to_string(&Hello { token: token.to_owned(), cwd: cwd.to_owned() })?;
    stream.write_all(format!("{hello}\n").as_bytes())?;
    let mut to_runner = stream.try_clone()?;
    std::thread::Builder::new().name("kiln-reach-mcp-in".into()).spawn(move || {
        let mut input = input;
        let _ = std::io::copy(&mut input, &mut to_runner);
        let _ = to_runner.shutdown(std::net::Shutdown::Write);
    })?;
    let mut buf = [0u8; 16 * 1024];
    loop {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        output.write_all(buf.get(..n).unwrap_or_default())?;
        output.flush()?;
    }
}

/// `kiln reach mcp` over the loopback its environment names, if it names one.
pub fn serve_from_env() -> Option<anyhow::Result<()>> {
    let addr = std::env::var(ADDR_ENV).ok().filter(|a| !a.trim().is_empty())?;
    let token = std::env::var(TOKEN_ENV).unwrap_or_default();
    let cwd = std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    Some(pipe(&addr, &token, &cwd, std::io::stdin(), std::io::stdout().lock()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A token is its session's alone, granting again replaces it, and no other string names a session.
    #[test]
    fn w_win_a_token_names_its_session_alone_and_a_new_grant_replaces_it() {
        let mut g = Grants::default();
        let first = g.grant("s1");
        let other = g.grant("s2");
        assert_eq!(g.session_of(&first).as_deref(), Some("s1"));
        assert_eq!(g.session_of(&other).as_deref(), Some("s2"));
        let again = g.grant("s1");
        assert_ne!(first, again);
        assert_eq!(g.session_of(&first), None, "a replaced token names no session");
        assert_eq!(g.session_of(&again).as_deref(), Some("s1"));
        assert_eq!(g.session_of(""), None);
        assert_eq!(g.session_of("not-a-token"), None);
    }

    /// The config a Windows runner's Claude Code gets starts `kiln reach mcp` with the runner's loopback, the session's
    /// token, its preferred runner and the session, and nothing of a Unix socket.
    #[test]
    fn w_win_claudes_config_names_the_loopback_its_token_and_its_session() {
        let addr: SocketAddr = "127.0.0.1:50123".parse().unwrap_or_else(|_| SocketAddr::from(([127, 0, 0, 1], 50123)));
        let c = claude_config_json(std::path::Path::new("C:\\kiln\\kiln.exe"), addr, "tok", Some("windows"), "s1");
        assert_eq!(c.pointer("/mcpServers/kiln/command"), Some(&json!("C:\\kiln\\kiln.exe")));
        assert_eq!(c.pointer("/mcpServers/kiln/args"), Some(&json!(["reach", "mcp"])));
        assert_eq!(c.pointer("/mcpServers/kiln/env"), Some(&json!({ADDR_ENV: "127.0.0.1:50123", TOKEN_ENV: "tok", "KILN_REACH_ON": "windows", "KILN_SESSION": "s1"})));
        assert!(c.pointer("/mcpServers/kiln/env/KILN_REACH_SOCK").is_none());
    }

    /// `kiln reach mcp`'s pipe says whose it is first, then carries its input to the runner and the runner's answers
    /// back, byte for byte, until the runner ends.
    #[test]
    fn w_win_kiln_reach_mcp_says_its_token_first_then_carries_both_ways() -> anyhow::Result<()> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        let addr = listener.local_addr()?.to_string();
        let runner = std::thread::spawn(move || -> anyhow::Result<(String, String)> {
            let (stream, _) = listener.accept()?;
            let mut writer = stream.try_clone()?;
            let mut reader = BufReader::new(stream);
            let mut first = String::new();
            reader.read_line(&mut first)?;
            let mut asked = String::new();
            reader.read_line(&mut asked)?;
            writer.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n")?;
            Ok((first, asked))
        });
        let mut heard = Vec::new();
        pipe(&addr, "tok", "C:\\p", std::io::Cursor::new(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}\n".to_vec()), &mut heard)?;
        let (first, asked) = runner.join().map_err(|_| anyhow::anyhow!("the runner's side panicked"))??;
        let said: Hello = serde_json::from_str(first.trim_end())?;
        assert_eq!((said.token.as_str(), said.cwd.as_str()), ("tok", "C:\\p"));
        assert_eq!(asked.trim_end(), "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}");
        assert_eq!(String::from_utf8(heard)?, "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n");
        Ok(())
    }

    /// A connection whose first line is no hello, or longer than its bound, is read as none.
    #[test]
    fn w_win_a_first_line_that_isnt_a_hello_is_none() {
        assert!(hello(&mut "not json\n".as_bytes()).is_none());
        assert!(hello(&mut "{\"token\":\"t\"}\n".as_bytes()).is_none());
        let long = format!("{{\"token\":\"{}\",\"cwd\":\"\"}}\n", "a".repeat(usize::try_from(HELLO_BYTES).unwrap_or(65_536)));
        assert!(hello(&mut long.as_bytes()).is_none());
        let ok = hello(&mut "{\"token\":\"t\",\"cwd\":\"c\"}\n".as_bytes());
        assert_eq!(ok.map(|h| (h.token, h.cwd)), Some(("t".to_owned(), "c".to_owned())));
    }
}
