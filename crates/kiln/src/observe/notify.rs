//! Where monitors' messages go (`docs/observe.md` §10): named integrations, each a service's
//! webhook, and the delivery of notifications to them.
//!
//! A message names its integrations with Datadog's mentions: `@slack-ops` is the Slack integration
//! named `ops`; likewise `@discord-…`, `@teams-…`, `@pagerduty-…` and `@webhook-…`. Each speaks its
//! service's published format:
//! - Slack: an incoming webhook's JSON (`text`, and Block Kit blocks in an attachment whose color
//!   says the state; api.slack.com "Sending messages using incoming webhooks");
//! - Discord: a webhook's embed (title ≤ 256, description ≤ 4,096, fields ≤ 25; discord.com
//!   "Webhook", "Embed Limits");
//! - Microsoft Teams: an Adaptive Card (1.4) message, what a Workflows webhook takes;
//! - PagerDuty: Events API v2 (`/v2/enqueue`): `trigger` while alerting, `resolve` when it
//!   recovers, on one dedup key a monitor's group, so one incident follows it;
//! - Opsgenie: the Alert API (v2): an alert created while alerting (`GenieKey`; its priority the
//!   monitor's, else P3, a warning's P4), closed when it recovers, by one alias a monitor's group;
//! - email: through an SMTP relay (`super::smtp`), to its recipients, a message of text and HTML,
//!   a monitor's group's messages one thread (`References`);
//! - a generic webhook: kiln's own JSON, signed with HMAC-SHA256 over the body
//!   (`X-Kiln-Signature: sha256=<hex>`), as GitHub signs its webhooks.
//!
//! Delivery is off the caller: a bounded queue, each notification tried again with exponential
//! backoff (1 s doubling, five tries) on a network error, a 429 (after its `Retry-After`) or a 5xx,
//! and its outcome recorded as an event.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::http::client::Client;

/// Notifications waiting to be delivered, at the most; past it, the newest is dropped and said.
const QUEUE: usize = 1024;
/// Tries a notification gets, and the first wait between them (doubling).
const TRIES: u32 = 5;
const BACKOFF: Duration = Duration::from_secs(1);
/// What a service is given to answer.
const TIMEOUT: Duration = Duration::from_secs(15);
/// PagerDuty's Events API (v2), unless an integration says another (its EU region's, or a test's).
pub const PAGERDUTY_EVENTS: &str = "https://events.pagerduty.com/v2/enqueue";
/// Opsgenie's API, unless an integration says another (`https://api.eu.opsgenie.com`, a test's).
pub const OPSGENIE: &str = "https://api.opsgenie.com";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Integration {
    Slack { url: String },
    Discord { url: String },
    Teams { url: String },
    Pagerduty {
        routing_key: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
    },
    Webhook {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        secret: Option<String>,
    },
    Opsgenie {
        api_key: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
    },
    Email {
        to: Vec<String>,
        from: String,
        /// The SMTP relay, and how it is reached (`super::smtp`).
        host: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        port: Option<u16>,
        #[serde(default)]
        security: super::smtp::Security,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        username: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        password: Option<String>,
        /// The CA (PEM) the relay's certificate is signed by, when not a public one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ca: Option<String>,
    },
}

impl Integration {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Slack { .. } => "slack",
            Self::Discord { .. } => "discord",
            Self::Teams { .. } => "teams",
            Self::Pagerduty { .. } => "pagerduty",
            Self::Webhook { .. } => "webhook",
            Self::Opsgenie { .. } => "opsgenie",
            Self::Email { .. } => "email",
        }
    }

    /// Whether it can be kept: an email's addresses and relay, an Opsgenie key.
    pub fn check(&self) -> Result<(), String> {
        match self {
            Self::Email { to, from, host, ca, .. } => {
                if to.is_empty() || to.len() > super::smtp::RECIPIENTS {
                    return Err(format!("an email integration has 1 to {} recipients", super::smtp::RECIPIENTS));
                }
                if let Some(bad) = to.iter().chain([from]).find(|a| !super::smtp::valid_address(a)) {
                    return Err(format!("{bad:?}: an address as local@domain"));
                }
                if host.is_empty() || host.len() > 253 || host.contains(|c: char| c.is_whitespace() || c.is_control() || c == '/') {
                    return Err(format!("{host:?}: the relay's host"));
                }
                if let Some(pem) = ca
                    && crate::http::tls::ClientTls::new(crate::http::client::Roots::Pem(pem.as_bytes().to_vec()), None).is_err()
                {
                    return Err("ca: a CA's certificates, PEM".into());
                }
                Ok(())
            }
            Self::Opsgenie { api_key, .. } if api_key.trim().is_empty() => Err("an Opsgenie integration's api_key".into()),
            _ => Ok(()),
        }
    }

    /// The integration as the API shows it: its secrets left out.
    pub fn redacted(&self) -> Value {
        let host = |u: &str| u.split('/').take(3).collect::<Vec<_>>().join("/");
        match self {
            Self::Slack { url } | Self::Discord { url } | Self::Teams { url } => json!({ "kind": self.kind(), "url": format!("{}/…", host(url)) }),
            Self::Pagerduty { url, .. } => json!({ "kind": "pagerduty", "routing_key": "…", "url": url }),
            Self::Webhook { url, secret } => json!({ "kind": "webhook", "url": url, "signed": secret.is_some() }),
            Self::Opsgenie { url, .. } => json!({ "kind": "opsgenie", "api_key": "…", "url": url }),
            Self::Email { to, from, host, port, security, username, password, ca } => {
                let mut v = json!({ "kind": "email", "to": to, "from": from, "host": host, "port": port.unwrap_or(security.port()), "security": security });
                if let Some(o) = v.as_object_mut() {
                    if let Some(u) = username {
                        o.insert("username".into(), json!(u));
                    }
                    if password.is_some() {
                        o.insert("password".into(), json!("…"));
                    }
                    if ca.is_some() {
                        o.insert("ca".into(), json!("…"));
                    }
                }
                v
            }
        }
    }
}

/// Integrations by name, kept in `<observe>/integrations.json` (it holds webhooks' secrets: the
/// owner's alone to read).
pub struct Integrations {
    path: PathBuf,
    all: Mutex<std::collections::BTreeMap<String, Integration>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Integrations {
    pub fn open(dir: &Path) -> Self {
        let path = dir.join("integrations.json");
        let all = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Self { path, all: Mutex::new(all) }
    }

    pub fn get(&self, name: &str) -> Option<Integration> {
        lock(&self.all).get(name).cloned()
    }

    pub fn list(&self) -> Vec<(String, Integration)> {
        lock(&self.all).iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    pub fn put(&self, name: &str, i: Integration) -> anyhow::Result<()> {
        anyhow::ensure!(!name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)), "an integration's name is letters, digits, '-', '_' and '.'");
        i.check().map_err(anyhow::Error::msg)?;
        let mut all = lock(&self.all);
        all.insert(name.to_owned(), i);
        save_private(&self.path, &serde_json::to_vec_pretty(&*all)?)
    }

    pub fn delete(&self, name: &str) -> anyhow::Result<bool> {
        let mut all = lock(&self.all);
        let had = all.remove(name).is_some();
        save_private(&self.path, &serde_json::to_vec_pretty(&*all)?)?;
        Ok(had)
    }

    /// The integration a mention names (`slack-ops` → the Slack integration `ops`).
    pub fn mentioned(&self, mention: &str) -> Option<(String, Integration)> {
        let (kind, name) = mention.split_once('-')?;
        let i = self.get(name)?;
        (i.kind() == kind).then(|| (name.to_owned(), i))
    }
}

fn save_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    crate::sync::group::save_atomic(path, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// What a notification says, whatever it goes to.
#[derive(Debug, Clone, Serialize)]
pub struct Notice {
    pub monitor_id: String,
    pub monitor: String,
    /// `ALERT`, `WARN`, `OK`, `NO DATA`.
    pub state: &'static str,
    pub previous: &'static str,
    pub group: Vec<(String, String)>,
    pub value: Option<f64>,
    pub threshold: f64,
    /// The rendered message, mentions and all.
    pub message: String,
    pub link: String,
    /// Unix ms.
    pub at: i64,
    pub priority: Option<u8>,
    pub tags: Vec<String>,
    /// What it is about (a webhook's `event`): `monitor.transition`, or an incident's change.
    pub event: &'static str,
    /// A title of its own (an incident's), in place of a monitor's state and name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headline: Option<String>,
    /// What it shows beside its message, in place of a monitor's value and threshold (an
    /// incident's severity and state).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub facts: Option<Vec<(String, String)>>,
}

impl Notice {
    /// What it shows beside its message: its own facts, or a monitor's value and threshold.
    fn facts(&self) -> Vec<(String, String)> {
        self.facts.clone().unwrap_or_else(|| vec![("Value".to_owned(), self.value_text()), ("Threshold".to_owned(), self.threshold.to_string())])
    }

    fn title(&self) -> String {
        if let Some(h) = &self.headline {
            return h.clone();
        }
        let group = if self.group.is_empty() { String::new() } else { format!(" on {}", self.group.iter().map(|(k, v)| format!("{k}:{v}")).collect::<Vec<_>>().join(",")) };
        let word = match self.state {
            "ALERT" => "Triggered",
            "WARN" => "Warning",
            "NO DATA" => "No data",
            _ => "Recovered",
        };
        format!("[{word}] {}{group}", self.monitor)
    }

    /// The message without its mentions (`@slack-ops`: words that start with `@` and name a kind
    /// and an integration), for the services that show it.
    fn body(&self) -> String {
        let body = self.message.lines().map(|l| l.split_whitespace().filter(|w| !(w.starts_with('@') && w.contains('-'))).collect::<Vec<_>>().join(" ")).collect::<Vec<_>>().join("\n");
        body.trim().to_owned()
    }

    fn color(&self) -> (u32, &'static str, &'static str) {
        // Red, amber, grey, green (Slack's attachment color, Discord's integer, Teams' style).
        match self.state {
            "ALERT" => (0xE0_1E5A, "#E01E5A", "Attention"),
            "WARN" => (0xEC_B22E, "#ECB22E", "Warning"),
            "NO DATA" => (0x86_8686, "#868686", "Default"),
            _ => (0x2E_B67D, "#2EB67D", "Good"),
        }
    }

    fn value_text(&self) -> String {
        self.value.map_or_else(|| "no data".to_owned(), |v| format!("{v}"))
    }
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The request a notice makes of an integration: its URL, body and headers.
pub fn request(i: &Integration, n: &Notice) -> (String, Vec<u8>, Vec<(String, String)>) {
    let (int, hex, teams) = n.color();
    let group = n.group.iter().map(|(k, v)| format!("{k}:{v}")).collect::<Vec<_>>().join(", ");
    let json_headers = vec![("content-type".to_owned(), "application/json".to_owned())];
    match i {
        Integration::Slack { url } => {
            let body = json!({
                "text": if n.facts.is_some() { n.title() } else { format!("{} — {}", n.title(), n.value_text()) },
                "attachments": [{
                    "color": hex,
                    "blocks": [
                        { "type": "section", "text": { "type": "mrkdwn", "text": format!("*<{}|{}>*\n{}", n.link, clip(&n.title(), 200), clip(&n.body(), 2900)) } },
                        { "type": "context", "elements": [{ "type": "mrkdwn", "text": match &n.facts {
                            Some(f) => f.iter().map(|(k, v)| format!("{k} *{v}*")).collect::<Vec<_>>().join(" · "),
                            None => format!("value *{}* · threshold {} · {}", n.value_text(), n.threshold, if group.is_empty() { "all".to_owned() } else { group.clone() }),
                        } }] },
                    ],
                }],
            });
            (url.clone(), serde_json::to_vec(&body).unwrap_or_default(), json_headers)
        }
        Integration::Discord { url } => {
            let mut fields: Vec<Value> = n.facts().into_iter().map(|(k, v)| json!({ "name": clip(&k, 256), "value": clip(&v, 1024), "inline": true })).collect();
            for (k, v) in n.group.iter().take(23) {
                fields.push(json!({ "name": clip(k, 256), "value": clip(v, 1024), "inline": true }));
            }
            let body = json!({
                "embeds": [{
                    "title": clip(&n.title(), 256),
                    "description": clip(&n.body(), 4096),
                    "url": n.link,
                    "color": int,
                    "fields": fields,
                    "timestamp": rfc3339(n.at),
                    "footer": { "text": "kiln" },
                }],
            });
            (url.clone(), serde_json::to_vec(&body).unwrap_or_default(), json_headers)
        }
        Integration::Teams { url } => {
            let mut facts: Vec<Value> = n.facts().into_iter().map(|(k, v)| json!({ "title": k, "value": v })).collect();
            for (k, v) in &n.group {
                facts.push(json!({ "title": k, "value": v }));
            }
            let body = json!({
                "type": "message",
                "attachments": [{
                    "contentType": "application/vnd.microsoft.card.adaptive",
                    "content": {
                        "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
                        "type": "AdaptiveCard",
                        "version": "1.4",
                        "body": [
                            { "type": "TextBlock", "size": "Medium", "weight": "Bolder", "color": teams, "text": n.title(), "wrap": true },
                            { "type": "TextBlock", "text": n.body(), "wrap": true },
                            { "type": "FactSet", "facts": facts },
                        ],
                        "actions": [{ "type": "Action.OpenUrl", "title": "Open in kiln", "url": n.link }],
                    },
                }],
            });
            (url.clone(), serde_json::to_vec(&body).unwrap_or_default(), json_headers)
        }
        Integration::Pagerduty { routing_key, url } => {
            let resolve = n.state == "OK";
            // An incident's by its severity (SEV-0 and SEV-1 critical, SEV-2 error, SEV-3 warning);
            // a monitor's by its state.
            let severity = match (n.facts.is_some(), n.priority, n.state) {
                (true, Some(0 | 1), _) => "critical",
                (true, Some(2), _) => "error",
                (true, Some(3), _) => "warning",
                (true, _, _) => "info",
                (false, _, "ALERT") => "critical",
                (false, _, "WARN") => "warning",
                _ => "info",
            };
            let source = n.group.iter().find(|(k, _)| k == "host").map_or("kiln", |(_, v)| v.as_str());
            let body = json!({
                "routing_key": routing_key,
                "event_action": if resolve { "resolve" } else { "trigger" },
                "dedup_key": dedup_key(n),
                "payload": {
                    "summary": clip(&n.title(), 1024),
                    "source": source,
                    "severity": severity,
                    "timestamp": rfc3339(n.at),
                    "group": group,
                    "class": if n.facts.is_some() { "kiln.incident" } else { "kiln.monitor" },
                    "custom_details": match &n.facts {
                        Some(f) => json!({ "message": n.body(), "facts": f.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect::<serde_json::Map<String, Value>>(), "incident": n.monitor_id, "tags": n.tags }),
                        None => json!({ "message": n.body(), "value": n.value, "threshold": n.threshold, "monitor_id": n.monitor_id, "state": n.state, "tags": n.tags }),
                    },
                },
                "links": [{ "href": n.link, "text": "Open in kiln" }],
                "client": "kiln",
                "client_url": n.link,
            });
            (url.clone().unwrap_or_else(|| PAGERDUTY_EVENTS.to_owned()), serde_json::to_vec(&body).unwrap_or_default(), json_headers)
        }
        Integration::Opsgenie { api_key, url } => {
            let base = url.as_deref().unwrap_or(OPSGENIE).trim_end_matches('/');
            let alias = dedup_key(n);
            let mut headers = json_headers;
            headers.push(("authorization".into(), format!("GenieKey {api_key}")));
            if n.state == "OK" {
                let body = json!({ "source": "kiln", "user": "kiln", "note": clip(&format!("{}: {}", n.title(), n.body()), 25_000) });
                return (format!("{base}/v2/alerts/{alias}/close?identifierType=alias"), serde_json::to_vec(&body).unwrap_or_default(), headers);
            }
            // The monitor's priority, else Opsgenie's own default (P3), a warning's lower.
            let priority = n.priority.filter(|p| (1..=5).contains(p)).map_or_else(|| if n.state == "ALERT" { "P3".to_owned() } else { "P4".to_owned() }, |p| format!("P{p}"));
            let mut details = serde_json::Map::new();
            details.insert("monitor_id".into(), json!(n.monitor_id));
            details.insert("state".into(), json!(n.state));
            for (k, v) in n.facts() {
                details.insert(clip(&k.to_lowercase(), 100), json!(clip(&v, 500)));
            }
            details.insert("link".into(), json!(n.link));
            for (k, v) in &n.group {
                details.insert(clip(k, 100), json!(clip(v, 500)));
            }
            let body = json!({
                "message": clip(&n.title(), 130),
                "alias": alias,
                "description": clip(&format!("{}\n\n{}\n{}", n.body(), n.facts().iter().map(|(k, v)| format!("{} {v}", k.to_lowercase())).collect::<Vec<_>>().join(" · "), n.link), 15_000),
                "tags": n.tags.iter().take(20).map(|t| clip(t, 50)).collect::<Vec<_>>(),
                "details": details,
                "entity": clip(if group.is_empty() { &n.monitor } else { &group }, 512),
                "source": "kiln",
                "priority": priority,
            });
            (format!("{base}/v2/alerts"), serde_json::to_vec(&body).unwrap_or_default(), headers)
        }
        // Not over HTTP: `deliver_email`.
        Integration::Email { .. } => (String::new(), Vec::new(), Vec::new()),
        Integration::Webhook { url, secret } => {
            let body = serde_json::to_vec(&json!({ "event": n.event, "notice": n })).unwrap_or_default();
            let mut headers = json_headers;
            headers.push(("x-kiln-event".into(), n.event.into()));
            headers.push(("x-kiln-delivery".into(), crate::util::new_id()));
            if let Some(s) = secret {
                headers.push(("x-kiln-signature".into(), format!("sha256={}", crate::util::hmac_hex(s.as_bytes(), &body))));
            }
            (url.clone(), body, headers)
        }
    }
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// A notice as an email: its subject the title; its text and HTML the message, the value, the
/// threshold, the group and a link; a monitor's group's messages one thread, by a `References`
/// they share.
pub fn email(n: &Notice, to: &[String], from: &str) -> super::smtp::Mail {
    let (_, color, _) = n.color();
    let mut facts = n.facts();
    facts.extend(n.group.iter().cloned());
    let text = format!("{}\n\n{}\n\n{}\n\nOpen in kiln: {}\n", n.title(), n.body(), facts.iter().map(|(k, v)| format!("{k}: {v}")).collect::<Vec<_>>().join("\n"), n.link);
    let rows: String = facts
        .iter()
        .map(|(k, v)| format!("<tr><td style=\"padding:2px 16px 2px 0;color:#6b6f76\">{}</td><td style=\"padding:2px 0\">{}</td></tr>", html_escape(k), html_escape(v)))
        .collect();
    let html = format!(
        "<!doctype html><html><body style=\"margin:0;padding:24px;background:#f6f7f9;font:14px/1.5 -apple-system,'Segoe UI',Helvetica,Arial,sans-serif;color:#1c1d21\">\
<table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" style=\"max-width:600px;margin:0 auto;background:#ffffff;border:1px solid #e4e5e7;border-radius:8px\"><tr>\
<td style=\"padding:20px 24px;border-left:4px solid {color};border-radius:8px\">\
<div style=\"font-size:18px;font-weight:600\">{title}</div>\
<div style=\"margin-top:12px;white-space:pre-wrap\">{body}</div>\
<table role=\"presentation\" cellpadding=\"0\" cellspacing=\"0\" style=\"margin-top:16px;font-size:13px\">{rows}</table>\
<div style=\"margin-top:20px\"><a href=\"{link}\" style=\"color:#1c1d21;font-weight:600\">Open in kiln</a></div>\
</td></tr></table></body></html>",
        title = html_escape(&n.title()),
        body = html_escape(&n.body()),
        link = html_escape(&n.link),
    );
    let domain = from.rsplit_once('@').map_or("kiln", |(_, d)| d);
    let headers = vec![("References".to_owned(), format!("<{}@{domain}>", dedup_key(n))), ("X-Kiln-Monitor".to_owned(), n.monitor_id.chars().filter(|c| !c.is_control()).collect()), ("X-Kiln-State".to_owned(), n.state.to_owned())];
    super::smtp::Mail { from: from.to_owned(), to: to.to_vec(), subject: n.title(), text, html, headers }
}

/// One incident a monitor's group, whatever its state: PagerDuty folds every trigger into it and
/// a resolve closes it.
pub fn dedup_key(n: &Notice) -> String {
    let group = n.group.iter().map(|(k, v)| format!("{k}:{v}")).collect::<Vec<_>>().join(",");
    format!("kiln-{}-{}", n.monitor_id, crate::util::sha256_hex(group.as_bytes()).get(..16).unwrap_or(""))
}

fn rfc3339(ms: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000).ok().and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok()).unwrap_or_default()
}

/// A delivery to make.
struct Job {
    name: String,
    integration: Integration,
    notice: Notice,
}

/// Delivers notifications, off the caller.
#[derive(Clone)]
pub struct Notifier {
    tx: async_channel::Sender<Job>,
}

impl Notifier {
    pub fn start(http: Arc<Client>) -> Self {
        let (tx, rx) = async_channel::bounded::<Job>(QUEUE);
        crate::rt::spawn(async move {
            while let Ok(job) = rx.recv().await {
                let http = http.clone();
                crate::rt::spawn(async move { deliver(&http, job).await });
            }
        });
        Self { tx }
    }

    /// Sends `n` to integration `name`.
    pub fn send(&self, name: &str, integration: Integration, notice: Notice) {
        if self.tx.try_send(Job { name: name.to_owned(), integration, notice }).is_err() {
            tracing::warn!(integration = %name, "observe's notification queue is full: a notification was dropped");
        }
    }
}

/// An email's delivery: tried again on what the relay says is transient, not on what it refuses.
async fn deliver_email(job: &Job) -> (bool, String) {
    let Integration::Email { to, from, host, port, security, username, password, ca } = &job.integration else { return (false, "not an email integration".into()) };
    let relay = super::smtp::Relay { host: host.clone(), port: port.unwrap_or(security.port()), security: *security, username: username.clone(), password: password.clone(), ca: ca.clone() };
    let mail = email(&job.notice, to, from);
    let mut wait = BACKOFF;
    let mut outcome = String::new();
    for attempt in 1..=TRIES {
        match super::smtp::send(&relay, &mail, crate::util::now_ms()).await {
            Ok(said) => return (true, if attempt == 1 { said } else { format!("{said} on try {attempt}") }),
            Err(super::smtp::Failure::Permanent(m)) => return (false, m),
            Err(super::smtp::Failure::Transient(m)) => outcome = m,
        }
        if attempt < TRIES {
            crate::rt::sleep(wait).await;
            wait = wait.saturating_mul(2);
        }
    }
    (false, outcome)
}

async fn deliver(http: &Client, job: Job) {
    let (delivered, outcome) = match &job.integration {
        Integration::Email { .. } => deliver_email(&job).await,
        _ => deliver_http(http, &job).await,
    };
    if !delivered {
        tracing::warn!(integration = %job.name, outcome = %outcome, "a monitor's notification was not delivered");
    }
    record_delivery(&job, delivered, &outcome);
}

async fn deliver_http(http: &Client, job: &Job) -> (bool, String) {
    let (url, body, headers) = request(&job.integration, &job.notice);
    let mut wait = BACKOFF;
    let mut outcome = String::new();
    let mut delivered = false;
    for attempt in 1..=TRIES {
        let mut req = http.post(&url).body(body.clone()).timeout(TIMEOUT);
        for (k, v) in &headers {
            req = req.header(k, v);
        }
        match req.send().await {
            Ok(r) if r.is_success() => {
                delivered = true;
                outcome = if attempt == 1 { r.status().to_string() } else { format!("{} on try {attempt}", r.status()) };
                break;
            }
            Ok(r) => {
                let status = r.status();
                outcome = format!("{status}");
                // 4xx but 429: the service refuses this notification; trying again changes nothing.
                if (400..500).contains(&status) && status != 429 {
                    break;
                }
                if let Some(after) = r.header("retry-after").and_then(|v| v.trim().parse::<u64>().ok()) {
                    wait = wait.max(Duration::from_secs(after.min(300)));
                }
            }
            Err(e) => outcome = format!("{e:#}"),
        }
        if attempt < TRIES {
            crate::rt::sleep(wait).await;
            wait = wait.saturating_mul(2);
        }
    }
    (delivered, outcome)
}

/// The delivery's record: an event, and a count by integration and result.
fn record_delivery(job: &Job, delivered: bool, outcome: &str) {
    if let Some(o) = super::api::get() {
        let result = if delivered { "delivered" } else { "failed" };
        let ts = crate::util::now_ms();
        if let Ok(k) = kiln_obs::batch::SeriesKey::new("kiln.monitor.notifications", [("integration", job.name.as_str()), ("kind", job.integration.kind()), ("result", result)], kiln_obs::batch::Kind::Count) {
            o.write(vec![(k, vec![(ts, 1.0)])]);
        }
        o.log(vec![kiln_obs::logs::record::Record {
            ts: ts.saturating_mul(1_000_000),
            severity: if delivered { 9 } else { 17 },
            service: "kiln.monitor".into(),
            host: String::new(),
            source: "kiln.event".into(),
            message: format!("notification of {} to {} ({}): {result}, {outcome}", job.notice.monitor, job.name, job.integration.kind()),
            attrs: vec![("event.type".into(), "monitor.notification".into()), ("monitor_id".into(), job.notice.monitor_id.clone()), ("integration".into(), job.name.clone()), ("result".into(), result.into())],
            trace_id: None,
            span_id: None,
        }]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The value at `path` (keys, or indexes into arrays), null where there is none.
    fn at<'a>(v: &'a Value, path: &[&str]) -> &'a Value {
        static NULL: Value = Value::Null;
        path.iter().try_fold(v, |v, k| match k.parse::<usize>() {
            Ok(i) => v.get(i),
            Err(_) => v.get(*k),
        })
        .unwrap_or(&NULL)
    }

    fn notice(state: &'static str) -> Notice {
        Notice {
            monitor_id: "m1".into(),
            monitor: "CPU high".into(),
            state,
            previous: "OK",
            group: vec![("host".into(), "web-1".into())],
            value: Some(93.5),
            threshold: 90.0,
            message: "CPU on web-1 is 93.5 @slack-ops @pagerduty-oncall".into(),
            link: "https://kiln.example/observe/monitors/m1".into(),
            at: 1_700_000_000_000,
            priority: Some(1),
            tags: vec!["team:infra".into()],
            event: "monitor.transition",
            headline: None,
            facts: None,
        }
    }

    #[test]
    fn each_service_gets_its_own_format() -> anyhow::Result<()> {
        let n = notice("ALERT");
        let (_, body, _) = request(&Integration::Slack { url: "https://hooks.slack.com/services/T/B/x".into() }, &n);
        let v: Value = serde_json::from_slice(&body)?;
        anyhow::ensure!(at(&v, &["text"]).as_str().is_some_and(|t| t.contains("[Triggered] CPU high on host:web-1")), "{v}");
        anyhow::ensure!(at(&v, &["attachments", "0", "color"]) == "#E01E5A");
        let section = at(&v, &["attachments", "0", "blocks", "0", "text", "text"]).as_str().unwrap_or("");
        anyhow::ensure!(section.contains("CPU on web-1 is 93.5") && !section.contains("@slack-ops"), "mentions are not shown: {section}");
        let (_, body, _) = request(&Integration::Discord { url: "https://discord.com/api/webhooks/1/x".into() }, &n);
        let v: Value = serde_json::from_slice(&body)?;
        anyhow::ensure!(at(&v, &["embeds", "0", "color"]) == 0xE01E5A && at(&v, &["embeds", "0", "timestamp"]) == "2023-11-14T22:13:20Z", "{v}");
        let (url, body, _) = request(&Integration::Pagerduty { routing_key: "R0UT1NG".into(), url: None }, &n);
        let v: Value = serde_json::from_slice(&body)?;
        anyhow::ensure!(url == PAGERDUTY_EVENTS && at(&v, &["event_action"]) == "trigger" && at(&v, &["payload", "severity"]) == "critical" && at(&v, &["payload", "source"]) == "web-1", "{v}");
        let (_, rbody, _) = request(&Integration::Pagerduty { routing_key: "R0UT1NG".into(), url: None }, &notice("OK"));
        let r: Value = serde_json::from_slice(&rbody)?;
        anyhow::ensure!(at(&r, &["event_action"]) == "resolve" && at(&r, &["dedup_key"]) == at(&v, &["dedup_key"]), "the resolve closes the trigger's incident");
        let (_, body, headers) = request(&Integration::Webhook { url: "https://x".into(), secret: Some("s3cret".into()) }, &n);
        let sig = headers.iter().find(|(k, _)| k == "x-kiln-signature").map(|(_, v)| v.clone()).unwrap_or_default();
        anyhow::ensure!(sig == format!("sha256={}", crate::util::hmac_hex(b"s3cret", &body)));
        let (_, body, _) = request(&Integration::Teams { url: "https://x".into() }, &n);
        let v: Value = serde_json::from_slice(&body)?;
        anyhow::ensure!(at(&v, &["attachments", "0", "content", "type"]) == "AdaptiveCard");
        // Opsgenie: an alert by alias while alerting, the monitor's priority; closed by it.
        let og = Integration::Opsgenie { api_key: "G3N13".into(), url: None };
        let (url, body, headers) = request(&og, &n);
        let v: Value = serde_json::from_slice(&body)?;
        anyhow::ensure!(url == "https://api.opsgenie.com/v2/alerts" && headers.contains(&("authorization".to_owned(), "GenieKey G3N13".to_owned())), "{url} {headers:?}");
        anyhow::ensure!(at(&v, &["alias"]).as_str() == Some(dedup_key(&n).as_str()) && at(&v, &["priority"]) == "P1" && at(&v, &["details", "host"]) == "web-1" && at(&v, &["entity"]) == "host:web-1", "{v}");
        anyhow::ensure!(at(&v, &["message"]).as_str().is_some_and(|m| m.chars().count() <= 130 && m.starts_with("[Triggered] CPU high")), "{v}");
        let warn = Notice { priority: None, ..notice("WARN") };
        let (_, body, _) = request(&og, &warn);
        anyhow::ensure!(at(&serde_json::from_slice(&body)?, &["priority"]) == "P4", "a warning without a priority is P4");
        let (url, _, _) = request(&Integration::Opsgenie { api_key: "G3N13".into(), url: Some("https://api.eu.opsgenie.com/".into()) }, &notice("OK"));
        anyhow::ensure!(url == format!("https://api.eu.opsgenie.com/v2/alerts/{}/close?identifierType=alias", dedup_key(&n)), "{url}");
        Ok(())
    }

    /// An email: its subject the title, the message without mentions, its facts and link; its HTML
    /// escaped; a group's messages one thread.
    #[test]
    fn a_notice_is_an_email_of_text_and_escaped_html() {
        let mut n = notice("ALERT");
        n.message = "disk <script>alert(1)</script> & more @email-ops".into();
        let m = email(&n, &["ops@example.com".into()], "kiln@alerts.example.com");
        assert_eq!(m.subject, "[Triggered] CPU high on host:web-1");
        assert!(m.text.contains("disk <script>alert(1)</script> & more") && !m.text.contains("@email-ops") && m.text.contains("Threshold: 90") && m.text.contains("host: web-1"), "{}", m.text);
        assert!(m.text.ends_with("Open in kiln: https://kiln.example/observe/monitors/m1\n"), "{}", m.text);
        assert!(m.html.contains("disk &lt;script&gt;alert(1)&lt;/script&gt; &amp; more") && !m.html.contains("<script>"), "{}", m.html);
        assert!(m.html.contains("border-left:4px solid #E01E5A") && m.html.contains("href=\"https://kiln.example/observe/monitors/m1\""), "{}", m.html);
        let thread = format!("<{}@alerts.example.com>", dedup_key(&n));
        assert!(m.headers.contains(&("References".to_owned(), thread.clone())));
        let ok = email(&notice("OK"), &["ops@example.com".into()], "kiln@alerts.example.com");
        assert!(ok.headers.contains(&("References".to_owned(), thread)), "its recovery in the same thread");
    }

    #[test]
    fn an_integration_is_checked_before_it_is_kept() {
        let mail = |to: Vec<&str>, from: &str, host: &str| Integration::Email { to: to.into_iter().map(str::to_owned).collect(), from: from.into(), host: host.into(), port: None, security: super::super::smtp::Security::Starttls, username: None, password: None, ca: None };
        assert_eq!(mail(vec!["ops@example.com"], "kiln@example.com", "smtp.example.com").check(), Ok(()));
        assert!(mail(vec![], "kiln@example.com", "smtp.example.com").check().is_err());
        assert!(mail(vec!["ops@example.com\r\nBcc: x@y.z"], "kiln@example.com", "smtp.example.com").check().is_err());
        assert!(mail(vec!["ops@example.com"], "kiln", "smtp.example.com").check().is_err());
        assert!(mail(vec!["ops@example.com"], "kiln@example.com", "smtp example").check().is_err());
        assert!(Integration::Opsgenie { api_key: " ".into(), url: None }.check().is_err());
        let shown = mail(vec!["ops@example.com"], "kiln@example.com", "smtp.example.com").redacted();
        assert_eq!(at(&shown, &["port"]), 587);
        let secret = Integration::Email {
            to: vec!["ops@example.com".into()],
            from: "kiln@example.com".into(),
            host: "h".into(),
            port: Some(2525),
            security: super::super::smtp::Security::None,
            username: Some("u".into()),
            password: Some("hunter2".into()),
            ca: None,
        };
        let shown = secret.redacted();
        assert!(!shown.to_string().contains("hunter2") && at(&shown, &["password"]) == "…" && at(&shown, &["port"]) == 2525, "{shown}");
    }
}
