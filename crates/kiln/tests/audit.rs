//! The audit log, v2 (docs/ci.md W19-AC04): every mutating route in the route table writes
//! exactly one audit row, with the right principal and on-behalf-of. Generated from the table:
//! each rule that changes something is called by an agent token acting for a user, with ids
//! where the pattern has wildcards. The token may do nothing, so no handler runs and nothing
//! changes, yet each call writes its one row, refused. A change that's let through writes one row
//! too, with what its handler recorded as its detail.

#![cfg(unix)]

use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// A port free now.
fn free_port() -> anyhow::Result<u16> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

/// One HTTP/1.1 request: the status and the body.
fn http(port: u16, method: &str, path: &str, token: &str, body: Option<&Value>) -> anyhow::Result<(u16, String)> {
    let mut s = TcpStream::connect(("127.0.0.1", port))?;
    s.set_read_timeout(Some(Duration::from_secs(30)))?;
    let payload = body.map(Value::to_string).unwrap_or_default();
    write!(s, "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}", payload.len())?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw)?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, rest) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status: u16 = head.split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    let body = if head.to_ascii_lowercase().contains("transfer-encoding: chunked") { dechunk(rest) } else { rest.to_owned() };
    Ok((status, body))
}

fn dechunk(mut s: &str) -> String {
    let mut out = String::new();
    while let Some((size, rest)) = s.split_once("\r\n") {
        let n = usize::from_str_radix(size.trim(), 16).unwrap_or(0);
        if n == 0 {
            break;
        }
        out.push_str(rest.get(..n).unwrap_or(rest));
        s = rest.get(n + 2..).unwrap_or("");
    }
    out
}

struct Server {
    child: Child,
    port: u16,
    admin: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start(dir: &Path) -> anyhow::Result<Server> {
    let (port, quic) = (free_port()?, free_port()?);
    let data = dir.join("data");
    let config = format!(
        r#"[server]
http_addr = "127.0.0.1:{port}"
quic_addr = "127.0.0.1:{quic}"
public_url = "http://127.0.0.1:{port}"
quic_advertise = "127.0.0.1:{quic}"
data_dir = "{data}"
https = false

[gitops]
auto_pipelines = false

[local_agent]
enabled = false

[kubernetes]
enabled = false

[terminal]
enabled = false

[discovery]
enabled = false
announce = false
browse = false
join_requests = false
tailscale = false

[cd]
enabled = false

[ci]
enabled = true
"#,
        data = data.display()
    );
    let cfg = dir.join("kiln.toml");
    std::fs::write(&cfg, config)?;
    let home = dir.join("home");
    std::fs::create_dir_all(&home)?;
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kiln"));
    cmd.args(["--config", &cfg.to_string_lossy(), "server"]).env("HOME", &home).stdout(Stdio::null()).stderr(Stdio::null());
    for var in ["KILN_CONFIG", "KILN_URL", "KILN_ADMIN_TOKEN", "KILN_SUPERVISED", "KILN_GENERATION", "KILN_LOG"] {
        cmd.env_remove(var);
    }
    let child = cmd.spawn()?;
    let mut server = Server { child, port, admin: String::new() };
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Ok(t) = std::fs::read_to_string(data.join("admin.token"))
            && let Ok((200, _)) = http(port, "GET", "/healthz", "", None)
        {
            server.admin = t.trim().to_owned();
            return Ok(server);
        }
        anyhow::ensure!(Instant::now() < deadline, "the server didn't start");
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// The audit rows after `after` (by id), oldest first.
fn rows_after(s: &Server, after: i64) -> anyhow::Result<Vec<Value>> {
    let (status, body) = http(s.port, "GET", "/api/audit?limit=50", &s.admin, None)?;
    anyhow::ensure!(status == 200, "audit: {status} {body}");
    let all: Vec<Value> = serde_json::from_str(&body)?;
    let mut new: Vec<Value> = all.into_iter().filter(|r| r.get("id").and_then(Value::as_i64).is_some_and(|i| i > after)).collect();
    new.reverse();
    Ok(new)
}

fn last_id(s: &Server) -> anyhow::Result<i64> {
    Ok(rows_after(s, -1)?.last().and_then(|r| r.get("id")).and_then(Value::as_i64).unwrap_or(0))
}

/// A concrete path for a pattern: `*` an id, `**` two segments.
fn concrete(pattern: &str) -> String {
    pattern.split('/').map(|p| match p {
        "*" => "x0",
        "**" => "x1/x2",
        p => p,
    }).collect::<Vec<_>>().join("/")
}

#[test]
fn w19_ac04_every_change_writes_one_row() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path())?;
    // A user, and an agent acting for her that may only look.
    let (st, body) = http(s.port, "POST", "/api/users", &s.admin, Some(&json!({ "name": "ada", "grants": ["viewer"] })))?;
    anyhow::ensure!((200..300).contains(&st), "user: {st} {body}");
    let (st, body) = http(s.port, "POST", "/api/tokens", &s.admin, Some(&json!({ "kind": "agent", "agent": "fixer", "user": "ada", "grants": ["viewer"] })))?;
    anyhow::ensure!((200..300).contains(&st), "token: {st} {body}");
    let token = serde_json::from_str::<Value>(&body)?.get("token").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("no token in {body}"))?;

    // Every rule that changes something, as the agent.
    let mut checked = 0;
    let mut wrong = Vec::new();
    for rule in kiln::authz::ROUTES {
        let method = match rule.method {
            Some("GET") => continue,
            Some(m) => m,
            None => "POST",
        };
        let path = concrete(rule.pattern);
        let before = last_id(&s)?;
        let (status, _) = http(s.port, method, &path, &token, Some(&json!({})))?;
        let rows = rows_after(&s, before)?;
        checked += 1;
        let ok = rows.len() == 1
            && rows.first().is_some_and(|r| {
                r.get("principal").and_then(Value::as_str) == Some("agent:fixer")
                    && r.get("on_behalf_of").and_then(Value::as_str) == Some("user:ada")
                    && r.get("result").and_then(Value::as_str) == Some(status.to_string().as_str())
                    && r.get("action").and_then(Value::as_str).is_some_and(|a| a.starts_with(method))
                    && r.get("target").and_then(Value::as_str) == Some(path.as_str())
            });
        if !ok {
            wrong.push(format!("{method} {path} ({status}): {rows:?}"));
        }
    }
    assert!(checked > 40, "{checked} mutating routes");
    assert!(wrong.is_empty(), "{} of {checked} routes didn't write exactly one right row:\n{}", wrong.len(), wrong.join("\n"));

    // A change let through writes one row too, with what its handler recorded.
    let before = last_id(&s)?;
    let (status, body) = http(s.port, "PUT", "/api/ci/variables/REGION?scope=server", &s.admin, Some(&json!({ "value": "eu" })))?;
    anyhow::ensure!((200..300).contains(&status), "variable: {status} {body}");
    let rows = rows_after(&s, before)?;
    assert_eq!(rows.len(), 1, "{rows:?}");
    let row = rows.first().cloned().unwrap_or_default();
    assert_eq!(row.get("principal").and_then(Value::as_str), Some("admin"));
    assert!(row.get("detail").and_then(Value::as_str).is_some_and(|d| d.contains("ci.variable.set")), "{row}");
    assert_eq!(row.get("scope").and_then(Value::as_str), Some("server"));
    // Filters by any field.
    let (_, body) = http(s.port, "GET", "/api/audit?principal=agent:fixer&result=403&limit=5", &s.admin, None)?;
    let filtered: Vec<Value> = serde_json::from_str(&body)?;
    assert!(!filtered.is_empty() && filtered.iter().all(|r| r.get("principal").and_then(Value::as_str) == Some("agent:fixer")), "{body}");
    Ok(())
}
