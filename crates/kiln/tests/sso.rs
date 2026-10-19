//! SSO with GitHub, GitLab and Bitbucket (docs/ci.md W23-R08), against one fake that answers as
//! all three: a person signs in with GitHub and gets the grants their organization and team give;
//! an outsider is refused; a team they left takes its grants back; GitLab finds the same kiln user
//! by the verified email (it makes nobody); Bitbucket makes a new one; a sign-in state is used once,
//! and the way back is never another site.

#![cfg(unix)]

mod common;

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};

use common::{Options, start};
use serde_json::{Value, json};

/// The fake host: each provider's token and API endpoints under its own prefix.
fn fake() -> anyhow::Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(match stream.try_clone() {
                Ok(s) => s,
                Err(_) => continue,
            });
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let mut parts = line.split_whitespace();
            let (method, path) = (parts.next().unwrap_or_default().to_owned(), parts.next().unwrap_or_default().to_owned());
            let (mut len, mut auth) = (0usize, String::new());
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).is_err() || h.trim().is_empty() {
                    break;
                }
                let (k, v) = h.split_once(':').unwrap_or((&h, ""));
                match k.trim().to_ascii_lowercase().as_str() {
                    "content-length" => len = v.trim().parse().unwrap_or(0),
                    "authorization" => auth = v.trim().to_owned(),
                    _ => {}
                }
            }
            let mut body = vec![0u8; len];
            let _ = reader.read_exact(&mut body);
            let form = String::from_utf8_lossy(&body).into_owned();
            let code = form.split('&').find_map(|kv| kv.strip_prefix("code=")).unwrap_or_default().to_owned();
            let path = path.split('?').next().unwrap_or_default().to_owned();
            let bearer = auth.strip_prefix("Bearer ").unwrap_or_default().to_owned();
            let (status, reply) = match (method.as_str(), path.as_str()) {
                ("POST", "/gh/login/oauth/access_token") => match code.as_str() {
                    "good" | "outsider" | "lost" => (200, json!({ "access_token": format!("gho_{code}"), "token_type": "bearer" })),
                    _ => (200, json!({ "error": "bad_verification_code", "error_description": "The code is wrong" })),
                },
                ("GET", "/gh-api/user") => match bearer.as_str() {
                    "gho_good" | "gho_lost" => (200, json!({ "id": 101, "login": "octo" })),
                    "gho_outsider" => (200, json!({ "id": 202, "login": "stranger" })),
                    _ => (401, json!({})),
                },
                ("GET", "/gh-api/user/emails") => (200, json!([{ "email": if bearer == "gho_outsider" { "s@example.com" } else { "octo@example.com" }, "primary": true, "verified": true }])),
                ("GET", "/gh-api/user/orgs") => (200, if bearer == "gho_outsider" { json!([{ "login": "other" }]) } else { json!([{ "login": "acme" }]) }),
                ("GET", "/gh-api/user/teams") => (200, if bearer == "gho_good" { json!([{ "slug": "devs", "organization": { "login": "acme" } }]) } else { json!([]) }),
                ("POST", "/gl/oauth/token") if code == "gl" => (200, json!({ "access_token": "glt" })),
                ("GET", "/gl/api/v4/user") if bearer == "glt" => (200, json!({ "id": 7, "username": "gl-octo", "email": "octo@example.com", "confirmed_at": "2026-01-01T00:00:00Z" })),
                ("GET", "/gl/api/v4/groups") if bearer == "glt" => (200, json!([{ "full_path": "acme/platform" }])),
                ("POST", "/bb/site/oauth2/access_token") if auth.starts_with("Basic ") && code == "bb" => (200, json!({ "access_token": "bbt" })),
                ("GET", "/bb-api/user") if bearer == "bbt" => (200, json!({ "uuid": "{0f0f}", "username": "bb-user" })),
                ("GET", "/bb-api/user/emails") if bearer == "bbt" => (200, json!({ "values": [{ "email": "bb@example.com", "is_primary": true, "is_confirmed": true }] })),
                ("GET", "/bb-api/user/permissions/workspaces") if bearer == "bbt" => (200, json!({ "values": [{ "workspace": { "slug": "acme" } }] })),
                _ => (404, json!({ "message": "Not Found" })),
            };
            let text = reply.to_string();
            let mut s = reader.into_inner();
            let _ = write!(s, "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len());
        }
    });
    Ok(port)
}

/// A request as a browser makes it: its status, headers and body.
fn browse(port: u16, path: &str, cookie: Option<&str>) -> anyhow::Result<(u16, String, String)> {
    let mut s = TcpStream::connect(("127.0.0.1", port))?;
    let c = cookie.map(|c| format!("Cookie: {c}\r\n")).unwrap_or_default();
    write!(s, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{c}Connection: close\r\n\r\n")?;
    let mut raw = String::new();
    s.read_to_string(&mut raw)?;
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
    let status = head.split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    Ok((status, head.to_owned(), body.to_owned()))
}

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().find_map(|l| l.split_once(':').filter(|(k, _)| k.trim().eq_ignore_ascii_case(name)).map(|(_, v)| v.trim()))
}

/// A sign-in from start to its session: the session cookie, and where it went.
fn sign_in(port: u16, provider: &str, code: &str, ret: &str) -> anyhow::Result<(u16, Option<String>, String, String)> {
    let (st, head, _) = browse(port, &format!("/api/auth/{provider}/start?return={ret}"), None)?;
    anyhow::ensure!(st == 303, "start: {st} {head}");
    let to = header(&head, "location").unwrap_or_default().to_owned();
    let state = to.split(['?', '&']).find_map(|kv| kv.strip_prefix("state=")).unwrap_or_default().to_owned();
    let (st, head, body) = browse(port, &format!("/api/auth/{provider}/callback?code={code}&state={state}"), None)?;
    let cookie = header(&head, "set-cookie").and_then(|c| c.split(';').next()).map(str::to_owned);
    Ok((st, cookie, header(&head, "location").unwrap_or_default().to_owned(), format!("{to}\n{body}")))
}

fn me(port: u16, cookie: &str) -> anyhow::Result<Value> {
    let (_, _, body) = browse(port, "/api/me", Some(cookie))?;
    Ok(serde_json::from_str(&body)?)
}

#[test]
fn w23_r08_sso() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let fake = fake()?;
    for name in ["gh", "gl", "bb"] {
        std::fs::write(dir.path().join(format!("{name}-secret")), "s3cret\n")?;
    }
    let d = dir.path().display();
    let extra = format!(
        r#"[sso.github]
client_id = "cid"
client_secret_file = "{d}/gh-secret"
web_url = "http://127.0.0.1:{fake}/gh"
api_url = "http://127.0.0.1:{fake}/gh-api"
allowed = ["acme"]
create_users = true
grants = {{ "acme/devs" = ["developer@org:acme"], "acme" = ["viewer@org:acme"] }}

[sso.gitlab]
url = "http://127.0.0.1:{fake}/gl"
client_id = "glid"
client_secret_file = "{d}/gl-secret"
allowed = ["acme"]

[sso.bitbucket]
web_url = "http://127.0.0.1:{fake}/bb"
api_url = "http://127.0.0.1:{fake}/bb-api"
client_id = "bbid"
client_secret_file = "{d}/bb-secret"
allowed = ["acme"]
create_users = true
"#
    );
    let s = start(dir.path(), &Options { extra: &extra, ..Options::default() })?;
    let (_, _, body) = browse(s.port, "/api/auth/sso", None)?;
    let list: Value = serde_json::from_str(&body)?;
    assert_eq!(list.get("providers").and_then(Value::as_array).map(Vec::len), Some(3), "{body}");

    // GitHub: a member of acme's devs team.
    let (st, cookie, back, _) = sign_in(s.port, "github", "good", "/runs")?;
    assert_eq!((st, back.as_str()), (303, "/runs"));
    let cookie = cookie.ok_or_else(|| anyhow::anyhow!("no session cookie"))?;
    let who = me(s.port, &cookie)?;
    assert_eq!((who.get("authenticated"), who.get("principal")), (Some(&json!(true)), Some(&json!("user:octo"))), "{who}");
    let grants = who.get("grants").cloned().unwrap_or_default().to_string();
    assert!(grants.contains("developer@org:acme") && grants.contains("viewer@org:acme"), "{grants}");

    // An outsider: refused.
    let (st, cookie, _, why) = sign_in(s.port, "github", "outsider", "/")?;
    assert!(st == 403 && cookie.is_none() && why.contains("isn't a member of acme"), "{st} {why}");
    // The team's grants go with the team.
    let (st, cookie, _, _) = sign_in(s.port, "github", "lost", "/")?;
    assert_eq!(st, 303);
    let grants = me(s.port, &cookie.unwrap_or_default())?.get("grants").cloned().unwrap_or_default().to_string();
    assert!(grants.contains("viewer@org:acme") && !grants.contains("developer@org:acme"), "{grants}");

    // GitLab makes nobody, but finds octo by the verified email.
    let (st, cookie, _, why) = sign_in(s.port, "gitlab", "gl", "/")?;
    assert_eq!(st, 303, "{why}");
    assert_eq!(me(s.port, &cookie.unwrap_or_default())?.get("principal"), Some(&json!("user:octo")));
    // Bitbucket makes a new user.
    let (st, cookie, _, why) = sign_in(s.port, "bitbucket", "bb", "/")?;
    assert_eq!(st, 303, "{why}");
    assert_eq!(me(s.port, &cookie.unwrap_or_default())?.get("principal"), Some(&json!("user:bb-user")));

    // A state is good once; the way back is kiln's own.
    let (_, head, _) = browse(s.port, "/api/auth/github/start?return=//evil.example/", None)?;
    let state = header(&head, "location").unwrap_or_default().split(['?', '&']).find_map(|kv| kv.strip_prefix("state=")).unwrap_or_default().to_owned();
    let (st, head, _) = browse(s.port, &format!("/api/auth/github/callback?code=good&state={state}"), None)?;
    assert!(st == 303 && header(&head, "location") == Some("/"), "{head}");
    let (st, _, _) = browse(s.port, &format!("/api/auth/github/callback?code=good&state={state}"), None)?;
    assert_eq!(st, 403, "a used state");
    let (st, _, _) = browse(s.port, "/api/auth/github/callback?code=wrong&state=nope", None)?;
    assert_eq!(st, 403);
    Ok(())
}
