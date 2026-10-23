//! Issue trackers as notification channels (docs/ci.md W11-R07): Linear and Jira.
//!
//! - Runs and diffs name their issues in their commit's trailers (`Fixes: ENG-123`,
//!   `Refs: OPS-42`, or an issue's link): a rule that sends to a tracker comments on each of its
//!   issues and links the run or diff there (a Linear attachment, a Jira remote link: one per
//!   issue and link, however often it's said). What names none of its issues is skipped.
//! - `test.quarantined` opens an issue in the integration's team or project (or the one the rule
//!   names: `linear:ENG`, `jira:OPS`), unless the quarantine entry names one of its issues, which
//!   is commented on instead.
//! - An integration may say which keys are its own (`prefixes`); without, it tries every one.
//!
//! Linear takes a personal API key (`lin_api_…`), or an OAuth app's `<client id>:<client secret>`
//! (its app actor's token, by the `client_credentials` grant). Jira Cloud takes an API token with
//! the account's `email`, or a service account's `<client id>:<client secret>` (Atlassian's
//! `client_credentials` grant, then the site's cloud through api.atlassian.com).

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::integrations::{self, Integration};
use crate::http::io::Reach;
use crate::hub::Hub;

/// The integration kinds that are issue trackers.
pub const KINDS: &[&str] = &["linear", "jira"];

/// Issue keys (`ENG-123`) in a message's trailers that say what it's for, in order and once each:
/// the trailers' words, and the parts of their links (`https://linear.app/acme/issue/ENG-123/…`).
pub fn references(message: &str) -> Vec<String> {
    const KEYS: &[&str] = &["fixes", "fixed", "closes", "closed", "resolves", "resolved", "refs", "references", "ref", "part-of", "issue", "issues", "see"];
    let mut out: Vec<String> = Vec::new();
    for line in message.lines() {
        let Some((k, v)) = line.split_once(':') else { continue };
        if !KEYS.contains(&k.trim().to_ascii_lowercase().as_str()) {
            continue;
        }
        for word in v.split(|c: char| c == ',' || c == '/' || c.is_whitespace()) {
            let w = word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-');
            if is_key(w) && !out.iter().any(|x| x == w) {
                out.push(w.to_owned());
            }
        }
    }
    out
}

/// `ENG-123`: a capital, then capitals, digits or `_` (2 to 10 in all), a dash, a number.
fn is_key(w: &str) -> bool {
    let Some((p, n)) = w.split_once('-') else { return false };
    (2..=10).contains(&p.len())
        && p.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && p.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && (1..=9).contains(&n.len())
        && n.chars().all(|c| c.is_ascii_digit())
}

/// Whether a tracker configured as `config` takes key `k`: one of its `prefixes`
/// (comma-separated), or any without them.
fn takes(config: &Value, k: &str) -> bool {
    let prefixes: Vec<String> = config.get("prefixes").and_then(Value::as_str).unwrap_or_default().split(',').map(|p| p.trim().to_ascii_uppercase()).filter(|p| !p.is_empty()).collect();
    prefixes.is_empty() || k.split_once('-').is_some_and(|(p, _)| prefixes.iter().any(|x| x == p))
}

/// What to say: a title, a line (with the link), the link, and the issues it names.
pub struct Note<'a> {
    pub event: &'a str,
    pub title: &'a str,
    pub line: &'a str,
    pub url: &'a str,
    pub refs: &'a [String],
}

/// Delivers `n` through tracker `i` (`target`: the team or project a rule names): what it did, or
/// `None` when it had nothing to do there.
pub async fn deliver(hub: &Hub, i: &Integration, target: Option<&str>, n: &Note<'_>) -> Result<Option<String>, String> {
    let client = client(hub, i).await?;
    act(&client, &i.config, target, n).await
}

/// Verify: who the credentials are.
pub async fn verify(hub: &Hub, i: &Integration) -> Result<String, String> {
    client(hub, i).await?.whoami().await
}

async fn client(hub: &Hub, i: &Integration) -> Result<Client, String> {
    let secret = integrations::token(hub, i).await.map_err(|e| format!("{e:#}"))?.ok_or_else(|| format!("{} has no credentials stored", i.name))?;
    Client::of(&i.kind, &i.id, &i.config, &secret, &hub.cfg.ci.reach()).await
}

/// What `n` has the tracker do: comment on and link each issue of its that `n` names, or open
/// one for a quarantined test that names none.
async fn act(client: &Client, config: &Value, target: Option<&str>, n: &Note<'_>) -> Result<Option<String>, String> {
    let mine: Vec<&String> = n.refs.iter().filter(|k| takes(config, k)).collect();
    if n.event == "test.quarantined" && mine.is_empty() {
        let (what, field, kind) = if client.linear { ("team", "team", "linear") } else { ("project", "project", "jira") };
        let into = target
            .map(str::to_owned)
            .or_else(|| config.get(field).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty()).map(str::to_owned))
            .ok_or_else(|| format!("which {what} its issues go in: `{field}` in the integration, or `{kind}:<key>` in the rule"))?;
        let issue_type = config.get("issue_type").and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty()).unwrap_or("Bug");
        let key = again(|| client.open(&into, issue_type, n.title, n.line)).await?;
        again(|| client.link(&key, n.url, n.title)).await.map_err(|e| format!("{key}'s link: {e}"))?;
        return Ok(Some(format!("opened {key}")));
    }
    if mine.is_empty() {
        return Ok(None);
    }
    let text = format!("{}\n\n{}", n.title, n.line);
    let mut done = Vec::new();
    for k in mine {
        again(|| client.comment(k, &text)).await.map_err(|e| format!("{k}: {e}"))?;
        again(|| client.link(k, n.url, n.title)).await.map_err(|e| format!("{k}'s link: {e}"))?;
        done.push(k.as_str());
    }
    Ok(Some(format!("commented on {}", done.join(", "))))
}

/// `f` until it succeeds, three times at most (now, after 2 s, after 15 s). Each is one change,
/// so a retry doesn't repeat what already went through: not a comment on an issue before it.
async fn again<T, F, Fut>(mut f: F) -> Result<T, String>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, String>>,
{
    let waits: &[u64] = if cfg!(test) { &[0, 0, 0] } else { &[0, 2, 15] };
    let mut last = Err(String::new());
    for wait in waits {
        if *wait > 0 {
            crate::rt::sleep(Duration::from_secs(*wait)).await;
        }
        last = Box::pin(f()).await;
        if last.is_ok() {
            break;
        }
    }
    last
}

/// Apps' tokens, by integration, token endpoint and credentials: the token and until when.
fn tokens() -> &'static parking_lot::Mutex<HashMap<String, (String, Instant)>> {
    static T: OnceLock<parking_lot::Mutex<HashMap<String, (String, Instant)>>> = OnceLock::new();
    T.get_or_init(Default::default)
}

/// A tracker's API, as one set of credentials reaches it.
struct Client {
    /// Linear's (GraphQL), or Jira's (REST).
    linear: bool,
    /// Linear's GraphQL endpoint, or Jira's REST API (`…/rest/api/3`).
    api: String,
    /// The `Authorization` header.
    auth: String,
    reach: Reach,
}

impl Client {
    async fn of(kind: &str, id: &str, config: &Value, secret: &str, reach: &Reach) -> Result<Client, String> {
        let cfg = |k: &str| config.get(k).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty()).map(|v| v.trim_end_matches('/').to_owned());
        match kind {
            "linear" => {
                let api = cfg("api").unwrap_or_else(|| integrations::endpoint("linear:api", || "https://api.linear.app/graphql".into()));
                let auth = match secret.split_once(':') {
                    // An OAuth app's client credentials: its app actor's token.
                    Some((client_id, client_secret)) if !secret.starts_with("lin_") => {
                        let url = format!("{}/oauth/token", origin(&api));
                        format!("Bearer {}", app_token(id, &url, client_id, client_secret, true, reach).await?)
                    }
                    _ => secret.to_owned(),
                };
                Ok(Client { linear: true, api, auth, reach: reach.clone() })
            }
            "jira" => {
                let site = cfg("site").ok_or("a Jira integration needs its `site`: https://acme.atlassian.net")?;
                match (cfg("email"), secret.split_once(':')) {
                    (Some(email), _) => {
                        use base64::Engine as _;
                        let basic = base64::engine::general_purpose::STANDARD.encode(format!("{email}:{secret}"));
                        Ok(Client { linear: false, api: format!("{site}/rest/api/3"), auth: format!("Basic {basic}"), reach: reach.clone() })
                    }
                    (None, Some((client_id, client_secret))) => {
                        let url = integrations::endpoint("atlassian:auth", || "https://auth.atlassian.com".into()) + "/oauth/token";
                        let token = app_token(id, &url, client_id, client_secret, false, reach).await?;
                        let base = integrations::endpoint("atlassian:api", || "https://api.atlassian.com".into());
                        let cloud = cloud_of(&base, &token, &site, reach).await?;
                        Ok(Client { linear: false, api: format!("{base}/ex/jira/{cloud}/rest/api/3"), auth: format!("Bearer {token}"), reach: reach.clone() })
                    }
                    (None, None) => Err("a Jira API token needs the account's `email` (or store a service account's `<client id>:<client secret>`)".into()),
                }
            }
            other => Err(format!("`{other}` isn't an issue tracker")),
        }
    }

    fn name(&self) -> &'static str {
        if self.linear { "Linear" } else { "Jira" }
    }

    async fn whoami(&self) -> Result<String, String> {
        if self.linear {
            let v = self.graphql("query { viewer { name } }", json!({})).await?;
            Ok(format!("Linear as {}", v.pointer("/viewer/name").and_then(Value::as_str).unwrap_or("someone")))
        } else {
            let v = self.rest("GET", "/myself", None).await?;
            Ok(format!("Jira as {}", v.get("displayName").and_then(Value::as_str).unwrap_or("someone")))
        }
    }

    async fn comment(&self, key: &str, text: &str) -> Result<(), String> {
        if self.linear {
            let id = self.linear_issue(key).await?;
            let v = self.graphql("mutation($i: CommentCreateInput!) { commentCreate(input: $i) { success } }", json!({ "i": { "issueId": id, "body": text } })).await?;
            return v.pointer("/commentCreate/success").and_then(Value::as_bool).filter(|ok| *ok).map(|_| ()).ok_or_else(|| "Linear didn't take the comment".into());
        }
        self.rest("POST", &format!("/issue/{key}/comment"), Some(json!({ "body": adf(text) }))).await.map(|_| ())
    }

    /// Links `url` to issue `key`: once per issue and link, however often it's linked.
    async fn link(&self, key: &str, url: &str, title: &str) -> Result<(), String> {
        if self.linear {
            let id = self.linear_issue(key).await?;
            // An attachment is one per issue and URL: linking again updates it.
            let v = self.graphql("mutation($issue: String!, $url: String!, $title: String) { attachmentLinkURL(issueId: $issue, url: $url, title: $title) { success } }", json!({ "issue": id, "url": url, "title": title })).await?;
            return v.pointer("/attachmentLinkURL/success").and_then(Value::as_bool).filter(|ok| *ok).map(|_| ()).ok_or_else(|| "Linear didn't take the link".into());
        }
        // A remote link whose global id is the URL: one per issue and URL.
        self.rest("POST", &format!("/issue/{key}/remotelink"), Some(json!({ "globalId": url, "object": { "url": url, "title": title } }))).await.map(|_| ())
    }

    /// Opens an issue in team or project `into`: its key.
    async fn open(&self, into: &str, issue_type: &str, title: &str, body: &str) -> Result<String, String> {
        if self.linear {
            let teams = self.graphql("query($k: String!) { teams(filter: { key: { eq: $k } }) { nodes { id } } }", json!({ "k": into })).await?;
            let team = teams.pointer("/teams/nodes/0/id").and_then(Value::as_str).ok_or_else(|| format!("Linear has no team {into}"))?.to_owned();
            let v = self.graphql("mutation($i: IssueCreateInput!) { issueCreate(input: $i) { success issue { identifier } } }", json!({ "i": { "teamId": team, "title": title, "description": body } })).await?;
            return v.pointer("/issueCreate/issue/identifier").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| "Linear didn't open the issue".into());
        }
        let v = self.rest("POST", "/issue", Some(json!({ "fields": { "project": { "key": into }, "summary": title, "issuetype": { "name": issue_type }, "description": adf(body) } }))).await?;
        v.get("key").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| "Jira didn't open the issue".into())
    }

    /// A Linear issue's id, from its key.
    async fn linear_issue(&self, key: &str) -> Result<String, String> {
        let v = self.graphql("query($id: String!) { issue(id: $id) { id } }", json!({ "id": key })).await?;
        v.pointer("/issue/id").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| format!("Linear has no issue {key}"))
    }

    async fn graphql(&self, query: &str, variables: Value) -> Result<Value, String> {
        let v = self.call("POST", &self.api, Some(&json!({ "query": query, "variables": variables }))).await?;
        if let Some(e) = v.get("errors").and_then(Value::as_array).and_then(|e| e.first()) {
            return Err(e.get("message").and_then(Value::as_str).unwrap_or("Linear refused it").to_owned());
        }
        Ok(v.get("data").cloned().unwrap_or(Value::Null))
    }

    async fn rest(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value, String> {
        self.call(method, &format!("{}{path}", self.api), body.as_ref()).await
    }

    async fn call(&self, method: &str, url: &str, body: Option<&Value>) -> Result<Value, String> {
        let client = crate::http::client::Client::public().map_err(|e| format!("{e:#}"))?;
        let mut req = if method == "POST" { client.post(url) } else { client.get(url) };
        req = req.header("authorization", &self.auth).header("accept", "application/json").timeout(Duration::from_secs(20)).reaching(&self.reach);
        if let Some(b) = body {
            req = req.header("content-type", "application/json").body(serde_json::to_vec(b).map_err(|e| e.to_string())?);
        }
        let resp = Box::pin(req.send()).await.map_err(|e| format!("{e:#}"))?;
        let status = resp.status();
        let text = String::from_utf8_lossy(&resp.bytes(1 << 20).await.unwrap_or_default()).into_owned();
        if !(200..300).contains(&status) {
            return Err(format!("{} answered {status}: {}", self.name(), crate::util::truncate(text.trim(), 300)));
        }
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }
}

/// `https://host[:port]` of a URL.
fn origin(url: &str) -> &str {
    let start = url.find("://").map_or(0, |i| i + 3);
    url.get(start..).and_then(|rest| rest.find('/')).and_then(|j| url.get(..start + j)).unwrap_or(url)
}

/// An app's token by the `client_credentials` grant (Linear's as a form, Atlassian's as JSON),
/// kept until a minute before it expires.
async fn app_token(id: &str, url: &str, client_id: &str, secret: &str, form: bool, reach: &Reach) -> Result<String, String> {
    let key = format!("{id}\u{0}{url}\u{0}{client_id}\u{0}{secret}");
    if let Some((t, until)) = tokens().lock().get(&key).cloned()
        && until > Instant::now()
    {
        return Ok(t);
    }
    let client = crate::http::client::Client::public().map_err(|e| format!("{e:#}"))?;
    let (ctype, body) = if form {
        ("application/x-www-form-urlencoded", format!("grant_type=client_credentials&client_id={}&client_secret={}&scope=read,write", urlencode(client_id), urlencode(secret)).into_bytes())
    } else {
        ("application/json", serde_json::to_vec(&json!({ "grant_type": "client_credentials", "client_id": client_id, "client_secret": secret })).map_err(|e| e.to_string())?)
    };
    let req = client.post(url).header("content-type", ctype).header("accept", "application/json").body(body).timeout(Duration::from_secs(20)).reaching(reach);
    let resp = Box::pin(req.send()).await.map_err(|e| format!("{e:#}"))?;
    let status = resp.status();
    let text = String::from_utf8_lossy(&resp.bytes(1 << 16).await.unwrap_or_default()).into_owned();
    if !(200..300).contains(&status) {
        return Err(format!("the app's token: {status} {}", crate::util::truncate(text.trim(), 200)));
    }
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("the app's token: {e}"))?;
    let token = v.get("access_token").and_then(Value::as_str).ok_or("the app's token: no access_token")?.to_owned();
    let secs = v.get("expires_in").and_then(Value::as_u64).unwrap_or(3600).saturating_sub(60).max(60);
    tokens().lock().insert(key, (token.clone(), Instant::now() + Duration::from_secs(secs)));
    Ok(token)
}

/// The Atlassian cloud id of `site`, as a service account's token sees it.
async fn cloud_of(base: &str, token: &str, site: &str, reach: &Reach) -> Result<String, String> {
    let client = crate::http::client::Client::public().map_err(|e| format!("{e:#}"))?;
    let req = client.get(&format!("{base}/oauth/token/accessible-resources")).bearer(token).header("accept", "application/json").timeout(Duration::from_secs(20)).reaching(reach);
    let resp = Box::pin(req.send()).await.map_err(|e| format!("{e:#}"))?;
    let status = resp.status();
    let text = String::from_utf8_lossy(&resp.bytes(1 << 16).await.unwrap_or_default()).into_owned();
    if !(200..300).contains(&status) {
        return Err(format!("Atlassian's sites: {status} {}", crate::util::truncate(text.trim(), 200)));
    }
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("Atlassian's sites: {e}"))?;
    v.as_array()
        .into_iter()
        .flatten()
        .find(|r| r.get("url").and_then(Value::as_str).is_some_and(|u| u.trim_end_matches('/').eq_ignore_ascii_case(site)))
        .and_then(|r| r.get("id").and_then(Value::as_str))
        .map(str::to_owned)
        .ok_or_else(|| format!("the service account doesn't reach {site}"))
}

fn urlencode(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

/// Text as Atlassian's document format: a paragraph a line.
fn adf(text: &str) -> Value {
    let paragraphs: Vec<Value> = text.lines().filter(|l| !l.trim().is_empty()).map(|l| json!({ "type": "paragraph", "content": [{ "type": "text", "text": l }] })).collect();
    json!({ "type": "doc", "version": 1, "content": paragraphs })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::http::server::{self as srv, Request, Response};

    /// W11-R07: what a message's trailers say it's for, keys only from the trailers that do.
    #[test]
    fn w11_r07_references_from_trailers() {
        let m = "Parse dates\n\nThe parser took ENG-9 into account.\n\nFixes: ENG-123, ENG-124\nRefs: OPS-42\nChange-Id: I0123\nSee: https://linear.app/acme/issue/ABC-1/dates\nSigned-off-by: a <a@b>\nCloses: #12 and WEB-7.";
        assert_eq!(references(m), vec!["ENG-123", "ENG-124", "OPS-42", "ABC-1", "WEB-7"]);
        assert!(references("Fixes: not-a-key, E-1, ENGINEERING_TOO_LONG-1, ENG-1234567890").is_empty());
        assert_eq!(references("Issue: https://acme.atlassian.net/browse/OPS-7"), vec!["OPS-7"]);
        assert!(takes(&json!({ "prefixes": "eng, ops" }), "OPS-7") && !takes(&json!({ "prefixes": "ENG" }), "OPS-7") && takes(&json!({}), "OPS-7"));
        assert_eq!(adf("a\n\nb").pointer("/content/1/content/0/text"), Some(&json!("b")));
        assert_eq!((origin("https://api.linear.app/graphql"), origin("http://127.0.0.1:9/x/y"), origin("https://h")), ("https://api.linear.app", "http://127.0.0.1:9", "https://h"));
    }

    /// What the fake trackers were asked: method, path, `Authorization`, body.
    type Asked = Arc<parking_lot::Mutex<Vec<(String, String, String, Value)>>>;

    struct Fake(Asked);

    impl srv::Handler for Fake {
        async fn handle(self: Arc<Self>, req: Request) -> Response {
            let (method, path) = (req.method().to_string(), req.uri().path().to_owned());
            let auth = srv::header_str(&req, &http::header::AUTHORIZATION).unwrap_or("").to_owned();
            let raw = srv::body_bytes(req.into_body(), 1 << 20).await.unwrap_or_default();
            let body: Value = serde_json::from_slice(&raw).unwrap_or_else(|_| json!(String::from_utf8_lossy(&raw)));
            self.0.lock().push((method.clone(), path.clone(), auth.clone(), body.clone()));
            let ok = |v: Value| srv::json(http::StatusCode::OK, &v);
            let query = body.get("query").and_then(Value::as_str).unwrap_or_default();
            match (method.as_str(), path.as_str()) {
                // Linear: its app's token, then GraphQL as that app.
                ("POST", "/oauth/token") if body.as_str().is_some_and(|f| f.contains("grant_type=client_credentials") && f.contains("client_id=app")) => ok(json!({ "access_token": "lin-app", "expires_in": 2_592_000 })),
                ("POST", "/graphql") if auth == "Bearer lin-app" => match query {
                    q if q.contains("viewer") => ok(json!({ "data": { "viewer": { "name": "kiln (app)" } } })),
                    q if q.contains("issue(id") => ok(json!({ "data": { "issue": { "id": "uuid-eng-7" } } })),
                    q if q.contains("commentCreate") || q.contains("attachmentLinkURL") => ok(json!({ "data": { "commentCreate": { "success": true }, "attachmentLinkURL": { "success": true } } })),
                    q if q.contains("teams(") => ok(json!({ "data": { "teams": { "nodes": [{ "id": "team-eng" }] } } })),
                    q if q.contains("issueCreate") => ok(json!({ "data": { "issueCreate": { "success": true, "issue": { "identifier": "ENG-8" } } } })),
                    _ => ok(json!({ "errors": [{ "message": "unknown query" }] })),
                },
                // Atlassian: a service account's token, the sites it reaches, then the cloud's API.
                ("POST", "/oauth/token") if body.get("client_id") == Some(&json!("svc")) => ok(json!({ "access_token": "atl-svc", "expires_in": 3600 })),
                ("GET", "/oauth/token/accessible-resources") if auth == "Bearer atl-svc" => ok(json!([{ "id": "other", "url": "https://else.atlassian.net" }, { "id": "cloud-1", "url": "https://acme.atlassian.net/" }])),
                ("GET", "/ex/jira/cloud-1/rest/api/3/myself") if auth == "Bearer atl-svc" => ok(json!({ "displayName": "kiln (service)" })),
                ("POST", "/ex/jira/cloud-1/rest/api/3/issue") if auth == "Bearer atl-svc" => ok(json!({ "key": "OPS-9" })),
                ("POST", p) if auth == "Bearer atl-svc" && p.starts_with("/ex/jira/cloud-1/rest/api/3/issue/") => ok(json!({ "id": "1" })),
                _ => srv::json(http::StatusCode::UNAUTHORIZED, &json!({ "error": format!("{method} {path} as {auth}") })),
            }
        }
    }

    /// W11-R07: an OAuth app's and a service account's client credentials, exchanged for tokens
    /// once, then a run's issues commented on and linked, and a quarantined test's issue opened.
    #[test]
    fn w11_r07_trackers_as_apps() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let tcp = hyper_rt::tcp::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)), 16).map_err(|e| anyhow::anyhow!("{e}"))?;
            let addr = tcp.local_addr().map_err(|e| anyhow::anyhow!("{e}"))?;
            let asked: Asked = Arc::default();
            let fake = Arc::new(Fake(Arc::clone(&asked)));
            crate::rt::spawn(async move {
                while let Ok(s) = tcp.accept().await {
                    let (h, r) = (Arc::clone(&fake), s.peer_addr().ok());
                    crate::rt::spawn(srv::serve_connection(crate::http::io::RtIo::new(s), h, r));
                }
            });
            let base = format!("http://{addr}");
            let reach = Reach { allow: vec!["127.0.0.1".into()] };
            let refs = vec!["ENG-7".to_owned(), "OPS-3".to_owned()];
            let note = |event: &'static str| Note { event, title: "✗ ci #4 failed on main", line: "acme/app · 1234abcd Parse dates · ada · https://kiln/runs/r1", url: "https://kiln/runs/r1", refs: &refs };

            // Linear, as an OAuth app: the app actor's token, then only its own keys.
            let linear_cfg = json!({ "api": format!("{base}/graphql"), "prefixes": "ENG", "team": "ENG" });
            let linear = Client::of("linear", "i-lin", &linear_cfg, "app:shh", &reach).await.map_err(anyhow::Error::msg)?;
            assert_eq!(linear.whoami().await.map_err(anyhow::Error::msg)?, "Linear as kiln (app)");
            assert_eq!(act(&linear, &linear_cfg, None, &note("run.failed")).await.map_err(anyhow::Error::msg)?.as_deref(), Some("commented on ENG-7"));
            assert_eq!(act(&linear, &linear_cfg, None, &Note { refs: &[], ..note("run.failed") }).await.map_err(anyhow::Error::msg)?, None, "nothing to do");
            assert_eq!(act(&linear, &linear_cfg, None, &Note { refs: &[], ..note("test.quarantined") }).await.map_err(anyhow::Error::msg)?.as_deref(), Some("opened ENG-8"));
            let cached = Client::of("linear", "i-lin", &linear_cfg, "app:shh", &reach).await.map_err(anyhow::Error::msg)?;
            assert_eq!(cached.auth, "Bearer lin-app");

            // Jira, as a service account: the token, the site's cloud, then its REST API.
            integrations::override_endpoint("atlassian:auth", &base);
            integrations::override_endpoint("atlassian:api", &base);
            let jira_cfg = json!({ "site": "https://acme.atlassian.net", "prefixes": "OPS", "project": "OPS" });
            let jira = Client::of("jira", "i-jira", &jira_cfg, "svc:secret", &reach).await.map_err(anyhow::Error::msg)?;
            assert_eq!(jira.whoami().await.map_err(anyhow::Error::msg)?, "Jira as kiln (service)");
            assert_eq!(act(&jira, &jira_cfg, None, &note("run.failed")).await.map_err(anyhow::Error::msg)?.as_deref(), Some("commented on OPS-3"));
            assert_eq!(act(&jira, &jira_cfg, Some("WEB"), &Note { refs: &[], ..note("test.quarantined") }).await.map_err(anyhow::Error::msg)?.as_deref(), Some("opened OPS-9"));

            let asked = asked.lock().clone();
            let calls = |m: &str, p: &str| asked.iter().filter(|(am, ap, _, _)| am == m && ap == p).count();
            assert_eq!(calls("POST", "/oauth/token"), 2, "one token each: {asked:?}");
            let comment = asked.iter().find(|(_, p, _, _)| p == "/ex/jira/cloud-1/rest/api/3/issue/OPS-3/comment").map(|(_, _, _, b)| b.clone()).unwrap_or_default();
            assert_eq!(comment.pointer("/body/content/0/content/0/text"), Some(&json!("✗ ci #4 failed on main")), "{comment}");
            let link = asked.iter().find(|(_, p, _, _)| p == "/ex/jira/cloud-1/rest/api/3/issue/OPS-3/remotelink").map(|(_, _, _, b)| b.clone()).unwrap_or_default();
            assert_eq!(link.get("globalId"), Some(&json!("https://kiln/runs/r1")), "{link}");
            let opened = asked.iter().find(|(m, p, _, _)| m == "POST" && p == "/ex/jira/cloud-1/rest/api/3/issue").map(|(_, _, _, b)| b.clone()).unwrap_or_default();
            assert_eq!((opened.pointer("/fields/project/key"), opened.pointer("/fields/issuetype/name")), (Some(&json!("WEB")), Some(&json!("Bug"))), "the rule's project: {opened}");
            assert!(!asked.iter().any(|(_, p, _, b)| p == "/graphql" && b.to_string().contains("OPS-3")), "Linear isn't asked of OPS-3");
            Ok(())
        })
    }
}
