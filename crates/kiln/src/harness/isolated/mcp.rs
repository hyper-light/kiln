//! kiln's MCP server for an isolated session's harness (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d,
//! step 18). On its machine a harness gets it as a program, `kiln reach mcp` over its runner's
//! socket, or as the server's `/api/mcp` with a token of its own (`crate::mcp::Kit`); a microVM has
//! neither the program nor the socket, so its harness reaches `/api/mcp` over HTTP on the machine
//! itself, which its microVM names `host.docker.internal` (Shards D236), at the one port its image
//! grants: `NETWORK --egress=host.docker.internal:<port>`, out on that port for its harness, and
//! nothing else of the machine. Default deny holds: an image that grants no such destination gets
//! no MCP server ([`granted`]), and the run can only keep or drop what its image grants, never add
//! (D236's `Narrowing.destinations`).
//!
//! The token it presents is the session's own MCP token, as a harness on the server's machine has
//! it (`crate::mcp::session_token`: calls with it are that agent's, and end with it), in each
//! harness's own form ([`for_harness`]). Step 11 makes the microVM's own key what says which
//! session asks, in place of a token any process given it could present (docs/agents.md, "In a
//! microVM of its own").

use serde_json::{Value, json};

use super::Refusal;
use crate::harness::kinds::{Kind, Protocol};

/// The machine itself, as a microVM names it (Shards D236: Docker Desktop's name for it; `.internal`
/// is the top-level domain ICANN reserved for private use, so no name in the DNS is it).
pub const HOST: &str = "host.docker.internal";

/// The variable a harness that takes its MCP token from its environment reads it from (Codex's
/// `bearer_token_env_var`: it takes no token in its configuration, codex-rs `config/src/mcp_types.rs`).
pub const TOKEN_VAR: &str = "KILN_MCP_TOKEN";

/// Where this machine serves kiln's MCP to a microVM, and the session's own token there.
#[derive(Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub port: u16,
    token: String,
}

/// Never its token.
impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint").field("port", &self.port).finish_non_exhaustive()
    }
}

impl Endpoint {
    pub fn new(port: u16, token: String) -> Self {
        Self { port, token }
    }

    /// Its URL, as the microVM reaches it.
    pub fn url(&self) -> String {
        format!("http://{HOST}:{}/api/mcp", self.port)
    }

    /// The destination its image must grant (D236's `HOST:PORT`).
    pub fn destination(&self) -> String {
        format!("{HOST}:{}", self.port)
    }
}

/// The variable of its harness's environment that holds its MCP token, where its harness reads it
/// from there ([`TOKEN_VAR`]).
#[derive(Clone, PartialEq, Eq)]
pub struct McpEnv {
    pub name: &'static str,
    pub value: String,
}

/// Never its value.
impl std::fmt::Debug for McpEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpEnv").field("name", &self.name).finish_non_exhaustive()
    }
}

/// Whether the image's destinations (Shards D236, as `shards-api` reads them from its
/// `NETWORK --egress=HOST:PORT`) take its microVM to kiln's MCP at `port`: refused, typed, when
/// not, so that no harness is given a server it can't reach.
pub fn granted(destinations: &[String], port: u16) -> Result<(), Refusal> {
    let want = format!("{HOST}:{port}");
    if destinations.iter().any(|d| d.trim().eq_ignore_ascii_case(&want)) {
        Ok(())
    } else {
        Err(Refusal::McpNotGranted { host: HOST, port })
    }
}

/// kiln's MCP as one harness takes it in a microVM.
#[derive(Debug, Clone, PartialEq)]
pub enum Config {
    /// Arguments its command line gets, and a variable its environment gets.
    Args { args: Vec<String>, env: Option<McpEnv> },
    /// An ACP agent's session's `mcpServers` entry (ACP's `http` transport, for an agent that
    /// says it takes one: `agentCapabilities.mcpCapabilities.http`).
    Acp(Value),
    /// It takes none kiln can give it there, and why.
    Refused(&'static str),
}

/// kiln's MCP at `endpoint` as `kind`'s harness takes it:
/// - Claude Code: `--mcp-config` with the server as JSON (it takes JSON strings as well as files,
///   code.claude.com/docs/en/cli-reference), an `http` server with its token in its headers.
/// - Codex: `-c` settings of a streamable-HTTP server (`url`, `bearer_token_env_var`), its token in
///   [`TOKEN_VAR`], and its tool calls given as long as one of kiln's programs may run, as on its
///   machine (`reach::mcp::codex_config`).
/// - ACP agents: ACP's `http` server, its token in its headers.
/// - Antigravity reads its servers only from its own config file, which is in its person's volume
///   and so every one of their agy sessions': a session's own token can't go there.
/// - Pi takes no MCP server from kiln on its machine either.
pub fn for_harness(kind: &Kind, endpoint: &Endpoint) -> Config {
    let bearer = format!("Bearer {}", endpoint.token);
    match kind.protocol {
        Protocol::ClaudeStreamJson => {
            let server = json!({"mcpServers": {"kiln": {"type": "http", "url": endpoint.url(), "headers": {"Authorization": bearer}}}});
            Config::Args { args: vec!["--mcp-config".to_owned(), server.to_string()], env: None }
        }
        Protocol::CodexAppServer => {
            let q = |s: &str| serde_json::to_string(s).unwrap_or_default();
            Config::Args {
                args: vec![
                    "-c".to_owned(),
                    format!("mcp_servers.kiln.url={}", q(&endpoint.url())),
                    "-c".to_owned(),
                    format!("mcp_servers.kiln.bearer_token_env_var={}", q(TOKEN_VAR)),
                    "-c".to_owned(),
                    format!("mcp_servers.kiln.tool_timeout_sec={}", crate::reach::mcp::codex_tool_timeout_sec()),
                ],
                env: Some(McpEnv { name: TOKEN_VAR, value: endpoint.token.clone() }),
            }
        }
        Protocol::Acp(_) => Config::Acp(json!({"type": "http", "name": "kiln", "url": endpoint.url(), "headers": [{"name": "Authorization", "value": bearer}]})),
        Protocol::AgyStreamJson => Config::Refused("Antigravity reads its MCP servers only from its own config file, in its person's volume and so shared by their other agy sessions, where no session's own token can go"),
        Protocol::PiRpc => Config::Refused("Pi takes no MCP server from kiln"),
        Protocol::Terminal => Config::Refused("it runs as a terminal program, with no structured mode"),
    }
}

/// The server's own machine's endpoint for session `session`, as its kit has it (`crate::mcp::Kit`):
/// the port its own agents reach it at, and the session's own token. A runner's machine has no
/// kit: it serves kiln's MCP over its socket alone, which a microVM can't reach, so its isolated
/// sessions get none until it serves `/api/mcp` itself ([`Refusal::McpUnserved`]).
pub fn on_this_machine(session: &str) -> Result<Endpoint, Refusal> {
    let kit = crate::mcp::kit().ok_or(Refusal::McpUnserved("its kiln is a runner's, which serves kiln's MCP over its own socket alone (owed: `/api/mcp` on its loopback, docs/agents.md \"In a microVM of its own\")"))?;
    let (port, token) = kit.for_microvm(session).ok_or(Refusal::McpUnserved("the server's address for its own agents names no port"))?;
    Ok(Endpoint::new(port, token))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{Config, Endpoint, HOST, TOKEN_VAR, for_harness, granted};
    use crate::harness::isolated::Refusal;
    use crate::harness::kinds::Kind;

    fn kind(id: &str) -> anyhow::Result<&'static Kind> {
        Kind::get(id).ok_or_else(|| anyhow::anyhow!("no kind {id}"))
    }

    /// Only an image that grants kiln's MCP's destination gets the server; any other is refused,
    /// saying which destination it needs.
    #[test]
    fn step18_mcp_goes_only_where_the_image_grants_it() {
        let d = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(granted(&d(&["api.anthropic.com:443", "host.docker.internal:7070"]), 7070), Ok(()));
        assert_eq!(granted(&d(&["HOST.docker.internal:7070"]), 7070), Ok(()));
        assert_eq!(granted(&d(&["host.docker.internal:7071"]), 7070), Err(Refusal::McpNotGranted { host: HOST, port: 7070 }));
        assert_eq!(granted(&d(&["api.anthropic.com:443"]), 7070), Err(Refusal::McpNotGranted { host: HOST, port: 7070 }));
        assert_eq!(granted(&[], 7070), Err(Refusal::McpNotGranted { host: HOST, port: 7070 }));
        let words = Refusal::McpNotGranted { host: HOST, port: 7070 }.to_string();
        assert!(words.contains("host.docker.internal:7070"), "{words}");
    }

    /// Each harness gets the server in its own form, at the machine's name for itself, its token
    /// where that harness takes one; none is given a server on its own machine's loopback.
    #[test]
    fn step18_each_harness_gets_kilns_mcp_in_its_own_form() -> anyhow::Result<()> {
        let e = Endpoint::new(7070, "kiln_mcp_session-s1_abc".into());
        anyhow::ensure!(e.url() == "http://host.docker.internal:7070/api/mcp" && e.destination() == "host.docker.internal:7070");
        let Config::Args { args, env: None } = for_harness(kind("claude")?, &e) else { anyhow::bail!("claude") };
        anyhow::ensure!(args.first().map(String::as_str) == Some("--mcp-config"), "{args:?}");
        let server: Value = serde_json::from_str(args.get(1).map(String::as_str).unwrap_or_default())?;
        anyhow::ensure!(server.pointer("/mcpServers/kiln") == Some(&json!({"type": "http", "url": e.url(), "headers": {"Authorization": "Bearer kiln_mcp_session-s1_abc"}})), "{server}");
        let Config::Args { args, env: Some(token) } = for_harness(kind("codex")?, &e) else { anyhow::bail!("codex") };
        anyhow::ensure!(args.contains(&"mcp_servers.kiln.url=\"http://host.docker.internal:7070/api/mcp\"".to_owned()), "{args:?}");
        anyhow::ensure!(args.contains(&format!("mcp_servers.kiln.bearer_token_env_var=\"{TOKEN_VAR}\"")), "{args:?}");
        anyhow::ensure!(token.name == TOKEN_VAR && token.value == "kiln_mcp_session-s1_abc");
        anyhow::ensure!(!args.iter().any(|a| a.contains("kiln_mcp_session")), "Codex's token is in its arguments: {args:?}");
        for acp in ["devin", "gemini", "opencode", "goose", "amp", "cursor"] {
            let Config::Acp(s) = for_harness(kind(acp)?, &e) else { anyhow::bail!("{acp}") };
            anyhow::ensure!(s == json!({"type": "http", "name": "kiln", "url": e.url(), "headers": [{"name": "Authorization", "value": "Bearer kiln_mcp_session-s1_abc"}]}), "{acp}: {s}");
        }
        for none in ["antigravity", "pi"] {
            anyhow::ensure!(matches!(for_harness(kind(none)?, &e), Config::Refused(_)), "{none}");
        }
        // Its debug form never shows the token.
        anyhow::ensure!(!format!("{e:?}").contains("kiln_mcp_session"));
        Ok(())
    }
}
