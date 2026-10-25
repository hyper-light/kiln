//! Where monitors' messages go (`docs/observe.md` §10): named integrations, each a service's
//! webhook, and the delivery of notifications to them.
//!
//! A message names its integrations with Datadog's mentions: `@slack-ops` is the Slack integration
//! named `ops`; likewise `@discord-…`, `@teams-…`, `@pagerduty-…`, `@opsgenie-…`, `@email-…`,
//! `@jira-…`, `@servicenow-…` and `@webhook-…`. Each speaks its service's published format:
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
//! - Jira (Cloud's REST API 3, Data Center's 2): an issue a group, found again by a label of its
//!   key (JQL), made while it is wrong, each change a comment, moved to a done status as it
//!   recovers;
//! - ServiceNow (ITSM's incidents, the Table API): an incident a group, found again by its
//!   correlation id, made while it is wrong, each change a work note, resolved as it recovers;
//! - a generic webhook: kiln's own JSON, signed with HMAC-SHA256 over the body
//!   (`X-Kiln-Signature: sha256=<hex>`), as GitHub signs its webhooks.
//!
//! Delivery is off the caller: a bounded queue, each notification tried again with exponential
//! backoff (1 s doubling, five tries) on a network error, a 429 (after its `Retry-After`) or a 5xx,
//! and its outcome recorded as an event. A group's notifications to one integration go one at a
//! time in the order they were made (its lane), so its recovery never overtakes its alert.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::http::client::{Client, Method};

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
/// A Jira issue's type unless an integration says another: every software project has it.
pub const JIRA_ISSUE_TYPE: &str = "Task";
/// What a resolved ServiceNow incident's close code is unless an integration says another (one of
/// an instance's own choices out of the box).
pub const SERVICENOW_CLOSE_CODE: &str = "Resolved by caller";
/// A group's notifications to one integration waiting behind the one being delivered, at the most;
/// past it, the oldest waiting is dropped (a newer state says more) and said.
const LANE: usize = 32;
/// What a service's answer may come to, read (a search's, an issue's).
const ANSWER: usize = 1 << 20;

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
    /// An issue a monitor's group (an incident), in a project of Jira's.
    Jira {
        /// The site: `https://<site>.atlassian.net`, or a Data Center's base URL.
        url: String,
        #[serde(default)]
        edition: JiraEdition,
        /// The project's key, and its issues' type (`Task` unless said).
        project: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        issue_type: Option<String>,
        /// Cloud: the account's email, the token an API token of it. Data Center: a user's name,
        /// the token its password; none, the token a personal access token.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user: Option<String>,
        token: String,
        /// Whether an issue is moved to a done status when its group recovers (yes unless said).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        close: Option<bool>,
    },
    /// An incident a monitor's group (an incident), in ServiceNow's ITSM.
    Servicenow {
        /// The instance: `https://<instance>.service-now.com`.
        url: String,
        /// A user's name and password, or an OAuth access token alone.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        password: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<String>,
        /// The group incidents are assigned to and the user they are raised for (each a sys_id or a
        /// name), when said.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        assignment_group: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caller: Option<String>,
        /// Whether an incident is resolved when its group recovers (yes unless said), and its close
        /// code then (the instance's own; `Resolved by caller` unless said).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        close: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        close_code: Option<String>,
    },
}

/// Which Jira a site is: Cloud speaks REST API 3 (rich text in Atlassian's document format), Data
/// Center REST API 2 (wiki markup), and each finds issues by JQL its own way.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JiraEdition {
    #[default]
    Cloud,
    DataCenter,
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
            Self::Jira { .. } => "jira",
            Self::Servicenow { .. } => "servicenow",
        }
    }

    /// Whether it can be kept: an email's addresses and relay, an Opsgenie key, Jira's site,
    /// project and credentials, ServiceNow's instance and credentials.
    pub fn check(&self) -> Result<(), String> {
        let site = |url: &str, what: &str| {
            let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")).unwrap_or("");
            if rest.is_empty() || rest.starts_with('/') || url.contains(|c: char| c.is_whitespace() || c.is_control()) {
                return Err(format!("{url:?}: {what}, as https://…"));
            }
            Ok(())
        };
        let blank = |s: &Option<String>| s.as_deref().is_none_or(|s| s.trim().is_empty());
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
            Self::Jira { url, edition, project, issue_type, user, token, .. } => {
                site(url, "the Jira site")?;
                // Jira's own rule for a project's key: a capital letter, then capitals, digits, `_`.
                if !project.starts_with(|c: char| c.is_ascii_uppercase()) || !project.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_') || project.len() > 255 {
                    return Err(format!("{project:?}: a project's key, as OPS"));
                }
                if issue_type.as_deref().is_some_and(|t| t.trim().is_empty() || t.len() > 255) {
                    return Err("issue_type: an issue type's name, as Task".into());
                }
                if token.trim().is_empty() {
                    return Err("a Jira integration's token".into());
                }
                if *edition == JiraEdition::Cloud && blank(user) {
                    return Err("user: Jira Cloud's account email, its token an API token".into());
                }
                Ok(())
            }
            Self::Servicenow { url, user, password, token, .. } => {
                site(url, "the ServiceNow instance")?;
                match (blank(user), blank(password), blank(token)) {
                    (false, false, true) | (true, true, false) => Ok(()),
                    _ => Err("a ServiceNow integration's user and password, or an OAuth token alone".into()),
                }
            }
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
            Self::Jira { url, edition, project, issue_type, user, close, .. } => {
                let mut v = json!({ "kind": "jira", "url": url, "edition": edition, "project": project, "issue_type": issue_type.as_deref().unwrap_or(JIRA_ISSUE_TYPE), "token": "…", "close": close.unwrap_or(true) });
                if let (Some(o), Some(u)) = (v.as_object_mut(), user) {
                    o.insert("user".into(), json!(u));
                }
                v
            }
            Self::Servicenow { url, user, password, token, assignment_group, caller, close, close_code } => {
                let mut v = json!({ "kind": "servicenow", "url": url, "close": close.unwrap_or(true), "close_code": close_code.as_deref().unwrap_or(SERVICENOW_CLOSE_CODE) });
                let secret = |s: &Option<String>| s.as_ref().map(|_| "…".to_owned());
                if let Some(o) = v.as_object_mut() {
                    for (k, given) in [("user", user.clone()), ("password", secret(password)), ("token", secret(token)), ("assignment_group", assignment_group.clone()), ("caller", caller.clone())] {
                        if let Some(given) = given {
                            o.insert(k.into(), json!(given));
                        }
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
        // Not over HTTP: `deliver_email`; not one request: `deliver_jira`, `deliver_servicenow`.
        Integration::Email { .. } | Integration::Jira { .. } | Integration::Servicenow { .. } => (String::new(), Vec::new(), Vec::new()),
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
            // A group's notifications to an integration go one at a time, in the order they were
            // made: a recovery never overtakes its alert while that is tried again (PagerDuty would
            // open the incident after its resolve, and keep it open), nor does Jira's search for an
            // issue overtake the creation of it. Other groups' and integrations' go at once.
            let lanes: Arc<Mutex<HashMap<(String, String), VecDeque<Job>>>> = Arc::default();
            while let Ok(job) = rx.recv().await {
                let key = (job.name.clone(), dedup_key(&job.notice));
                {
                    let mut l = lock(&lanes);
                    if let Some(waiting) = l.get_mut(&key) {
                        if waiting.len() >= LANE
                            && let Some(old) = waiting.pop_front()
                        {
                            tracing::warn!(integration = %old.name, monitor = %old.notice.monitor, "a group's notifications waited past {LANE} behind one being delivered: the oldest waiting was dropped");
                            record_delivery(&old, false, &format!("dropped: {LANE} of the group's notifications waited behind one being delivered"));
                        }
                        waiting.push_back(job);
                        continue;
                    }
                    l.insert(key.clone(), VecDeque::new());
                }
                let (http, lanes) = (http.clone(), Arc::clone(&lanes));
                crate::rt::spawn(async move {
                    let mut job = job;
                    loop {
                        deliver(&http, job).await;
                        let next = {
                            let mut l = lock(&lanes);
                            let next = l.get_mut(&key).and_then(VecDeque::pop_front);
                            if next.is_none() {
                                l.remove(&key);
                            }
                            next
                        };
                        match next {
                            Some(n) => job = n,
                            None => break,
                        }
                    }
                });
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
        Integration::Jira { .. } => deliver_jira(http, &job).await,
        Integration::Servicenow { .. } => deliver_servicenow(http, &job).await,
        _ => deliver_http(http, &job).await,
    };
    if !delivered {
        tracing::warn!(integration = %job.name, outcome = %outcome, "a monitor's notification was not delivered");
    }
    record_delivery(&job, delivered, &outcome);
}

/// What a service answered a call it took: its status and body.
struct Answer {
    status: u16,
    body: Vec<u8>,
    /// The try it went on.
    attempt: u32,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    /// Its status, and the try it went on past the first.
    fn said(&self) -> String {
        if self.attempt == 1 { self.status.to_string() } else { format!("{} on try {}", self.status, self.attempt) }
    }
}

/// One call to a service, tried as a notification is: again after a network error, a 429 (after
/// its `Retry-After`) or a 5xx, the wait doubling from a second, five tries; any other 4xx is the
/// service refusing it (trying again changes nothing), said in its words.
async fn call(http: &Client, method: Method, url: &str, headers: &[(String, String)], body: Option<&[u8]>) -> Result<Answer, String> {
    let mut wait = BACKOFF;
    let mut outcome = String::new();
    for attempt in 1..=TRIES {
        let mut req = http.request(method, url).timeout(TIMEOUT);
        if let Some(b) = body {
            req = req.body(b.to_vec());
        }
        for (k, v) in headers {
            req = req.header(k, v);
        }
        match req.send().await {
            Ok(r) if r.is_success() => {
                let status = r.status();
                let body = r.bytes(ANSWER).await.map(|b| b.to_vec()).unwrap_or_default();
                return Ok(Answer { status, body, attempt });
            }
            Ok(r) => {
                let status = r.status();
                if (400..500).contains(&status) && status != 429 {
                    let said = r.bytes(4096).await.map(|b| String::from_utf8_lossy(&b).split_whitespace().collect::<Vec<_>>().join(" ")).unwrap_or_default();
                    return Err(if said.is_empty() { status.to_string() } else { format!("{status} {}", clip(&said, 300)) });
                }
                outcome = status.to_string();
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
    Err(outcome)
}

async fn deliver_http(http: &Client, job: &Job) -> (bool, String) {
    let (url, body, headers) = request(&job.integration, &job.notice);
    match call(http, Method::Post, &url, &headers, Some(&body)).await {
        Ok(a) => (true, a.said()),
        Err(outcome) => (false, outcome),
    }
}

/// The credentials and the JSON a service is called with: `user`'s Basic, else the token's Bearer.
fn credentials(user: Option<&str>, password: Option<&str>, token: Option<&str>) -> Vec<(String, String)> {
    let auth = match (user.filter(|u| !u.trim().is_empty()), password, token) {
        (Some(u), Some(p), _) => Some(format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{u}:{p}")))),
        (_, _, Some(t)) => Some(format!("Bearer {t}")),
        _ => None,
    };
    let mut headers: Vec<(String, String)> = auth.map(|a| ("authorization".to_owned(), a)).into_iter().collect();
    headers.push(("content-type".into(), "application/json".into()));
    headers.push(("accept".into(), "application/json".into()));
    headers
}

/// A group's key as a label of Jira's and a ServiceNow correlation id: its dedup key, any byte but
/// letters, digits, `-`, `_` and `.` as `_` (a label holds no space; JQL and an encoded query
/// quote nothing else), 100 bytes at the most (a correlation id's room).
pub fn group_label(n: &Notice) -> String {
    let mut s: String = dedup_key(n).chars().map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '_' }).collect();
    s.truncate(100);
    s
}

/// The facts a notice shows beside its message, and its group's tags.
fn all_facts(n: &Notice) -> Vec<(String, String)> {
    let mut facts = n.facts();
    facts.extend(n.group.iter().cloned());
    facts
}

/// What a notice says as plain text: its title, message, facts and link.
fn plain(n: &Notice) -> String {
    let facts = all_facts(n).iter().map(|(k, v)| format!("{k}: {v}")).collect::<Vec<_>>().join("\n");
    let body = n.body();
    let mut s = n.title();
    for part in [body.as_str(), facts.as_str()] {
        if !part.is_empty() {
            s.push_str("\n\n");
            s.push_str(part);
        }
    }
    s.push_str("\n\nOpen in kiln: ");
    s.push_str(&n.link);
    s
}

/// What a notice says in an issue or a comment of Jira's: Cloud's in Atlassian's document format
/// (its text as it is, every line kept, its facts a list, a link to kiln), Data Center's in wiki
/// markup (the message and facts as they are, in a `{noformat}` block, then the link).
pub fn jira_text(cloud: bool, n: &Notice) -> Value {
    let facts = all_facts(n);
    if !cloud {
        let mut text = n.body();
        for (k, v) in &facts {
            text.push_str(&format!("\n{k}: {v}"));
        }
        let text = text.trim().replace("{noformat}", "{ noformat}");
        return Value::String(format!("{{noformat}}\n{text}\n{{noformat}}\n[Open in kiln|{}]", n.link.replace(['|', ']'], "")));
    }
    // A text node holds some text: none is empty.
    let text = |t: &str| json!({ "type": "text", "text": t });
    let mut content = Vec::new();
    let body = n.body();
    let mut line: Vec<Value> = Vec::new();
    for (i, l) in body.lines().enumerate() {
        if i > 0 {
            line.push(json!({ "type": "hardBreak" }));
        }
        if !l.is_empty() {
            line.push(text(l));
        }
    }
    if !body.trim().is_empty() {
        content.push(json!({ "type": "paragraph", "content": line }));
    }
    let items: Vec<Value> = facts
        .iter()
        .filter(|(k, v)| !k.is_empty() && !v.is_empty())
        .map(|(k, v)| json!({ "type": "listItem", "content": [{ "type": "paragraph", "content": [{ "type": "text", "text": format!("{k}: "), "marks": [{ "type": "strong" }] }, text(v)] }] }))
        .collect();
    if !items.is_empty() {
        content.push(json!({ "type": "bulletList", "content": items }));
    }
    if n.link.contains("://") {
        content.push(json!({ "type": "paragraph", "content": [{ "type": "text", "text": "Open in kiln", "marks": [{ "type": "link", "attrs": { "href": n.link } }] }] }));
    } else if !n.link.is_empty() {
        content.push(json!({ "type": "paragraph", "content": [text(&n.link)] }));
    }
    json!({ "type": "doc", "version": 1, "content": content })
}

/// The JQL that finds a group's open issue: in its project, labelled with the group's key, not
/// done, the newest first.
pub fn jira_jql(project: &str, label: &str) -> String {
    format!("project = \"{project}\" AND labels = \"{label}\" AND statusCategory != Done ORDER BY created DESC")
}

/// A group's new issue: its summary the notice's title (one line, 255 characters at the most), its
/// description what it says, labelled `kiln` and with the group's key, which finds it again.
pub fn jira_issue(cloud: bool, project: &str, issue_type: &str, label: &str, n: &Notice) -> Value {
    json!({ "fields": {
        "project": { "key": project },
        "issuetype": { "name": issue_type },
        "summary": clip(&n.title().replace(['\r', '\n'], " "), 255),
        "description": jira_text(cloud, n),
        "labels": ["kiln", label],
    } })
}

/// A notice to Jira: the group's open issue found by its label (none: one made, while it is
/// wrong); each change said on it as a comment, and on its recovery the issue moved, by the first
/// of its transitions that goes there, to a done status (unless the integration says not to).
async fn deliver_jira(http: &Client, job: &Job) -> (bool, String) {
    let Integration::Jira { url, edition, project, issue_type, user, token, close } = &job.integration else { return (false, "not a Jira integration".into()) };
    let n = &job.notice;
    let cloud = *edition == JiraEdition::Cloud;
    let base = format!("{}/rest/api/{}", url.trim_end_matches('/'), if cloud { 3 } else { 2 });
    let headers = credentials(user.as_deref(), Some(token.as_str()), Some(token.as_str()));
    let label = group_label(n);
    // Cloud's search is `search/jql` (its `search` is gone); Data Center's is `search`.
    let search = format!("{base}/{}?jql={}&fields=status&maxResults=1", if cloud { "search/jql" } else { "search" }, super::cloud::encode(&jira_jql(project, &label)));
    let found = match call(http, Method::Get, &search, &headers, None).await {
        Ok(a) => a.json().get("issues").and_then(|i| i.get(0)).and_then(|i| i.get("key")).and_then(Value::as_str).map(str::to_owned),
        Err(e) => return (false, format!("looking for the group's issue: {e}")),
    };
    let recovered = n.state == "OK";
    let Some(key) = found else {
        if recovered {
            return (true, "no issue open".into());
        }
        let body = jira_issue(cloud, project, issue_type.as_deref().unwrap_or(JIRA_ISSUE_TYPE), &label, n);
        return match call(http, Method::Post, &format!("{base}/issue"), &headers, Some(&serde_json::to_vec(&body).unwrap_or_default())).await {
            Ok(a) => (true, format!("{} made {}", a.said(), a.json().get("key").and_then(Value::as_str).unwrap_or("an issue"))),
            Err(e) => (false, format!("making the issue: {e}")),
        };
    };
    let comment = serde_json::to_vec(&json!({ "body": jira_text(cloud, n) })).unwrap_or_default();
    if let Err(e) = call(http, Method::Post, &format!("{base}/issue/{key}/comment"), &headers, Some(&comment)).await {
        return (false, format!("commenting on {key}: {e}"));
    }
    if !recovered || *close == Some(false) {
        return (true, format!("commented on {key}"));
    }
    let transitions = match call(http, Method::Get, &format!("{base}/issue/{key}/transitions"), &headers, None).await {
        Ok(a) => a.json(),
        Err(e) => return (false, format!("commented on {key}; its transitions: {e}")),
    };
    let done = transitions
        .get("transitions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|t| t.pointer("/to/statusCategory/key").and_then(Value::as_str) == Some("done"))
        .and_then(|t| t.get("id").and_then(Value::as_str))
        .map(str::to_owned);
    let Some(id) = done else { return (true, format!("commented on {key}; none of its transitions goes to a done status")) };
    let body = serde_json::to_vec(&json!({ "transition": { "id": id } })).unwrap_or_default();
    match call(http, Method::Post, &format!("{base}/issue/{key}/transitions"), &headers, Some(&body)).await {
        Ok(_) => (true, format!("commented on {key} and closed it")),
        Err(e) => (false, format!("commented on {key}; closing it: {e}")),
    }
}

/// A notice's impact and urgency in ServiceNow (1 high, 3 low; the instance makes its priority of
/// them): its priority's (P1 1 and 1, P2 1 and 2, P3 2 and 2, P4 2 and 3, P5 3 and 3), else an
/// alert's P3 and a warning's P4, as Opsgenie's.
pub fn servicenow_priority(n: &Notice) -> (u8, u8) {
    match n.priority.filter(|p| (1..=5).contains(p)).unwrap_or(if n.state == "ALERT" { 3 } else { 4 }) {
        1 => (1, 1),
        2 => (1, 2),
        3 => (2, 2),
        4 => (2, 3),
        _ => (3, 3),
    }
}

/// A group's new incident: its short description the title (160 characters at the most), its
/// description what it says, its correlation id the group's key, which finds it again.
pub fn servicenow_incident(n: &Notice, correlation: &str, assignment_group: Option<&str>, caller: Option<&str>) -> Value {
    let (impact, urgency) = servicenow_priority(n);
    let mut v = json!({
        "short_description": clip(&n.title().replace(['\r', '\n'], " "), 160),
        "description": clip(&plain(n), 4000),
        "correlation_id": correlation,
        "correlation_display": "kiln",
        "impact": impact.to_string(),
        "urgency": urgency.to_string(),
    });
    if let Some(o) = v.as_object_mut() {
        if let Some(g) = assignment_group.filter(|g| !g.trim().is_empty()) {
            o.insert("assignment_group".into(), json!(g));
        }
        if let Some(c) = caller.filter(|c| !c.trim().is_empty()) {
            o.insert("caller_id".into(), json!(c));
        }
    }
    v
}

/// The encoded query that finds a group's open incident: its correlation id, not resolved, closed
/// or canceled (6, 7, 8: a resolved incident is still active until it closes), the newest first.
pub fn servicenow_query(correlation: &str) -> String {
    format!("correlation_id={correlation}^stateNOT IN6,7,8^ORDERBYDESCsys_created_on")
}

/// A notice to ServiceNow: the group's open incident found by its correlation id (none: one made,
/// while it is wrong); each change a work note on it, its impact and urgency the new state's; on
/// its recovery the incident resolved, with the integration's close code and what the notice says
/// as its close notes (unless the integration says not to).
async fn deliver_servicenow(http: &Client, job: &Job) -> (bool, String) {
    let Integration::Servicenow { url, user, password, token, assignment_group, caller, close, close_code } = &job.integration else { return (false, "not a ServiceNow integration".into()) };
    let n = &job.notice;
    let table = format!("{}/api/now/table/incident", url.trim_end_matches('/'));
    let headers = credentials(user.as_deref(), password.as_deref(), token.as_deref());
    let correlation = group_label(n);
    let search = format!("{table}?sysparm_query={}&sysparm_fields=sys_id,number&sysparm_limit=1", super::cloud::encode(&servicenow_query(&correlation)));
    let found = match call(http, Method::Get, &search, &headers, None).await {
        Ok(a) => {
            let v = a.json();
            let r = v.get("result").and_then(|r| r.get(0));
            r.and_then(|r| r.get("sys_id")).and_then(Value::as_str).map(|id| (id.to_owned(), r.and_then(|r| r.get("number")).and_then(Value::as_str).unwrap_or("the incident").to_owned()))
        }
        Err(e) => return (false, format!("looking for the group's incident: {e}")),
    };
    let recovered = n.state == "OK";
    let Some((id, number)) = found else {
        if recovered {
            return (true, "no incident open".into());
        }
        let body = servicenow_incident(n, &correlation, assignment_group.as_deref(), caller.as_deref());
        return match call(http, Method::Post, &table, &headers, Some(&serde_json::to_vec(&body).unwrap_or_default())).await {
            Ok(a) => (true, format!("{} made {}", a.said(), a.json().pointer("/result/number").and_then(Value::as_str).unwrap_or("an incident"))),
            Err(e) => (false, format!("making the incident: {e}")),
        };
    };
    let text = clip(&plain(n), 4000);
    let (change, done) = if recovered && *close != Some(false) {
        (json!({ "state": "6", "close_code": close_code.as_deref().unwrap_or(SERVICENOW_CLOSE_CODE), "close_notes": text }), format!("resolved {number}"))
    } else if recovered {
        (json!({ "work_notes": text }), format!("noted on {number}"))
    } else {
        let (impact, urgency) = servicenow_priority(n);
        (json!({ "work_notes": text, "impact": impact.to_string(), "urgency": urgency.to_string() }), format!("noted on {number}"))
    };
    match call(http, Method::Patch, &format!("{table}/{id}"), &headers, Some(&serde_json::to_vec(&change).unwrap_or_default())).await {
        Ok(_) => (true, done),
        Err(e) => (false, format!("updating {number}: {e}")),
    }
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
        // Jira: its site, a project's key, a token; Cloud's account email.
        let jira = |edition, project: &str, user: Option<&str>, token: &str| Integration::Jira {
            url: "https://acme.atlassian.net".into(),
            edition,
            project: project.into(),
            issue_type: None,
            user: user.map(str::to_owned),
            token: token.into(),
            close: None,
        };
        assert_eq!(jira(JiraEdition::Cloud, "OPS", Some("ops@example.com"), "t").check(), Ok(()));
        assert_eq!(jira(JiraEdition::DataCenter, "OPS_2", None, "pat").check(), Ok(()), "a personal access token alone");
        assert!(jira(JiraEdition::Cloud, "OPS", None, "t").check().is_err(), "Cloud's token is an account's");
        assert!(jira(JiraEdition::Cloud, "ops", Some("ops@example.com"), "t").check().is_err(), "a key is capitals");
        assert!(jira(JiraEdition::Cloud, "2OPS", Some("ops@example.com"), "t").check().is_err());
        assert!(jira(JiraEdition::Cloud, "OPS", Some("ops@example.com"), " ").check().is_err());
        assert!(
            Integration::Jira { url: "acme.atlassian.net".into(), edition: JiraEdition::Cloud, project: "OPS".into(), issue_type: None, user: Some("a@b.c".into()), token: "t".into(), close: None }
                .check()
                .is_err()
        );
        let shown = jira(JiraEdition::Cloud, "OPS", Some("ops@example.com"), "s3cr3t").redacted();
        assert!(!shown.to_string().contains("s3cr3t") && at(&shown, &["issue_type"]) == JIRA_ISSUE_TYPE && at(&shown, &["edition"]) == "cloud" && at(&shown, &["close"]) == true, "{shown}");
        // ServiceNow: its instance, a user and password or a token alone.
        let snow = |user: Option<&str>, password: Option<&str>, token: Option<&str>| Integration::Servicenow {
            url: "https://acme.service-now.com".into(),
            user: user.map(str::to_owned),
            password: password.map(str::to_owned),
            token: token.map(str::to_owned),
            assignment_group: None,
            caller: None,
            close: None,
            close_code: None,
        };
        assert_eq!(snow(Some("kiln"), Some("pw"), None).check(), Ok(()));
        assert_eq!(snow(None, None, Some("oauth")).check(), Ok(()));
        assert!(snow(Some("kiln"), None, None).check().is_err() && snow(Some("kiln"), Some("pw"), Some("oauth")).check().is_err() && snow(None, None, None).check().is_err());
        let shown = snow(Some("kiln"), Some("pw-s3cr3t"), None).redacted();
        assert!(!shown.to_string().contains("pw-s3cr3t") && at(&shown, &["password"]) == "…" && at(&shown, &["close_code"]) == SERVICENOW_CLOSE_CODE, "{shown}");
    }

    /// Whether `v` is a document Jira Cloud takes: a `doc` of version 1 whose nodes are each a
    /// kind it knows, a text node never empty, a list never without its items.
    fn adf_ok(v: &Value) -> bool {
        fn node(v: &Value) -> bool {
            let content = || v.get("content").and_then(Value::as_array);
            match v.get("type").and_then(Value::as_str) {
                Some("text") => v.get("text").and_then(Value::as_str).is_some_and(|t| !t.is_empty()),
                Some("hardBreak") => true,
                Some("paragraph") => content().is_some_and(|c| c.iter().all(node)),
                Some("bulletList" | "listItem") => content().is_some_and(|c| !c.is_empty() && c.iter().all(node)),
                _ => false,
            }
        }
        v.get("type").and_then(Value::as_str) == Some("doc") && v.get("version").and_then(Value::as_u64) == Some(1) && v.get("content").and_then(Value::as_array).is_some_and(|c| c.iter().all(node))
    }

    #[test]
    fn jira_and_servicenow_are_told_in_their_own_formats() -> anyhow::Result<()> {
        let mut n = notice("ALERT");
        n.message = "CPU on web-1 is 93.5\n\nsee the runbook @jira-ops".into();
        let label = group_label(&n);
        anyhow::ensure!(label == dedup_key(&n) && label.len() <= 100 && label.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)), "{label}");
        let weird = Notice { monitor_id: "m 1\"x".into(), ..notice("ALERT") };
        anyhow::ensure!(!group_label(&weird).contains([' ', '"']), "a label holds no space and JQL no stray quote: {}", group_label(&weird));
        // Cloud: the summary the title, the description a document of the message's lines, the
        // facts a list, the link a link; labelled with the group's key.
        let issue = jira_issue(true, "OPS", "Task", &label, &n);
        anyhow::ensure!(
            at(&issue, &["fields", "summary"]) == "[Triggered] CPU high on host:web-1" && at(&issue, &["fields", "project", "key"]) == "OPS" && at(&issue, &["fields", "issuetype", "name"]) == "Task",
            "{issue}"
        );
        anyhow::ensure!(at(&issue, &["fields", "labels"]) == &json!(["kiln", label]), "{issue}");
        let doc = at(&issue, &["fields", "description"]);
        anyhow::ensure!(adf_ok(doc), "a document Jira takes: {doc}");
        let words = doc.to_string();
        anyhow::ensure!(
            words.contains("CPU on web-1 is 93.5")
                && words.contains("see the runbook")
                && !words.contains("@jira-ops")
                && words.contains("Threshold: ")
                && words.contains("https://kiln.example/observe/monitors/m1"),
            "{doc}"
        );
        // A title past 255 characters, or of two lines, is one line of 255.
        let long = Notice { monitor: format!("{}\nsecond", "x".repeat(300)), ..notice("ALERT") };
        let summary = at(&jira_issue(true, "OPS", "Task", &label, &long), &["fields", "summary"]).as_str().unwrap_or("").to_owned();
        anyhow::ensure!(summary.chars().count() == 255 && !summary.contains('\n'), "{summary}");
        // A notice with no message is still a document Jira takes (no empty text, no empty list).
        let bare = Notice { message: String::new(), group: Vec::new(), facts: Some(Vec::new()), link: String::new(), ..notice("ALERT") };
        anyhow::ensure!(adf_ok(&jira_text(true, &bare)), "{}", jira_text(true, &bare));
        // Data Center: wiki markup, the message as it is in a block of its own, then the link.
        let wiki = jira_text(false, &Notice { message: "a {noformat} b [x|y] *z*".into(), ..notice("ALERT") });
        let wiki = wiki.as_str().unwrap_or("");
        anyhow::ensure!(wiki.starts_with("{noformat}\na { noformat} b [x|y] *z*\nValue: 93.5") && wiki.ends_with("{noformat}\n[Open in kiln|https://kiln.example/observe/monitors/m1]"), "{wiki}");
        anyhow::ensure!(jira_jql("OPS", &label) == format!("project = \"OPS\" AND labels = \"{label}\" AND statusCategory != Done ORDER BY created DESC"));
        // ServiceNow: its priority's impact and urgency, else an alert's P3 and a warning's P4.
        let p = |priority, state| servicenow_priority(&Notice { priority, ..notice(state) });
        anyhow::ensure!(
            p(Some(1), "ALERT") == (1, 1) && p(Some(2), "ALERT") == (1, 2) && p(Some(5), "WARN") == (3, 3) && p(None, "ALERT") == (2, 2) && p(None, "WARN") == (2, 3) && p(Some(9), "ALERT") == (2, 2)
        );
        let inc = servicenow_incident(&n, &label, Some("0a52d3dcd7011200f2d224837e6103f2"), None);
        anyhow::ensure!(
            at(&inc, &["short_description"]) == "[Triggered] CPU high on host:web-1"
                && at(&inc, &["correlation_id"]) == label.as_str()
                && at(&inc, &["impact"]) == "1"
                && at(&inc, &["urgency"]) == "1",
            "{inc}"
        );
        anyhow::ensure!(at(&inc, &["assignment_group"]) == "0a52d3dcd7011200f2d224837e6103f2" && at(&inc, &["caller_id"]).is_null(), "{inc}");
        anyhow::ensure!(
            at(&inc, &["description"])
                .as_str()
                .is_some_and(|d| d.contains("CPU on web-1 is 93.5") && d.contains("host: web-1") && d.ends_with("Open in kiln: https://kiln.example/observe/monitors/m1")),
            "{inc}"
        );
        anyhow::ensure!(servicenow_query(&label) == format!("correlation_id={label}^stateNOT IN6,7,8^ORDERBYDESCsys_created_on"));
        Ok(())
    }

    /// A service on a port of its own, answering each request as `answer` says (its method, its
    /// path and query, its headers named in lowercase, its body), keeping what came in order.
    struct Standin {
        port: u16,
        got: Arc<Mutex<Vec<(String, String, Vec<u8>)>>>,
    }

    type Answerer = dyn Fn(&str, &str, &[(String, String)], &[u8]) -> (u16, String) + Send + Sync;

    impl Standin {
        fn start(answer: Arc<Answerer>) -> anyhow::Result<Self> {
            use std::io::{Read as _, Write as _};
            let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
            let port = listener.local_addr()?.port();
            let got: Arc<Mutex<Vec<(String, String, Vec<u8>)>>> = Arc::default();
            let g = Arc::clone(&got);
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut s) = stream else { continue };
                    let (g, answer) = (Arc::clone(&g), Arc::clone(&answer));
                    std::thread::spawn(move || {
                        let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
                        let (mut buf, mut chunk) = (Vec::new(), [0u8; 8192]);
                        let head_end = loop {
                            let Ok(n) = s.read(&mut chunk) else { return };
                            if n == 0 {
                                return;
                            }
                            buf.extend_from_slice(chunk.get(..n).unwrap_or_default());
                            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                                break i;
                            }
                        };
                        let head = String::from_utf8_lossy(buf.get(..head_end).unwrap_or_default()).into_owned();
                        let mut lines = head.lines();
                        let mut first = lines.next().unwrap_or("").split_whitespace();
                        let (method, path) = (first.next().unwrap_or("").to_owned(), first.next().unwrap_or("").to_owned());
                        let headers: Vec<(String, String)> = lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned())).collect();
                        let len: usize = headers.iter().find(|(k, _)| k == "content-length").and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
                        let mut body = buf.get(head_end + 4..).unwrap_or_default().to_vec();
                        while body.len() < len {
                            let Ok(n) = s.read(&mut chunk) else { break };
                            if n == 0 {
                                break;
                            }
                            body.extend_from_slice(chunk.get(..n).unwrap_or_default());
                        }
                        let (status, out) = answer(&method, &path, &headers, &body);
                        lock(&g).push((method, path, body));
                        let after = if status == 429 { "Retry-After: 1\r\n" } else { "" };
                        let _ = write!(s, "HTTP/1.1 {status} Status\r\n{after}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}", out.len());
                    });
                }
            });
            Ok(Self { port, got })
        }

        fn url(&self) -> String {
            format!("http://127.0.0.1:{}", self.port)
        }

        fn got(&self) -> Vec<(String, String, Vec<u8>)> {
            lock(&self.got).clone()
        }
    }

    /// `%XX` as its byte.
    fn decode(s: &str) -> String {
        let b = s.as_bytes();
        let (mut out, mut i) = (Vec::with_capacity(b.len()), 0);
        while let Some(&c) = b.get(i) {
            match b.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok()) {
                Some(v) if c == b'%' => {
                    out.push(v);
                    i += 3;
                }
                _ => {
                    out.push(c);
                    i += 1;
                }
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// The value of `name` in a path's query, decoded.
    fn param(path: &str, name: &str) -> String {
        path.split_once('?').and_then(|(_, q)| q.split('&').find_map(|kv| kv.strip_prefix(name).and_then(|v| v.strip_prefix('=')))).map(decode).unwrap_or_default()
    }

    /// What a Jira stand-in keeps of an issue: its key, labels, whether it is done, the comments on it.
    #[derive(Debug, Clone)]
    struct Issue {
        key: String,
        labels: Vec<String>,
        done: bool,
        comments: usize,
    }

    /// A Jira site as Jira answers (Cloud's REST API 3 with ADF, or Data Center's 2 with wiki
    /// markup): its credentials checked, issues made and found by JQL's label, commented on, moved
    /// by their transitions; a field it would refuse refused (400, in its words).
    fn jira_standin(cloud: bool, auth: String) -> anyhow::Result<(Standin, Arc<Mutex<Vec<Issue>>>)> {
        let issues: Arc<Mutex<Vec<Issue>>> = Arc::default();
        let kept = Arc::clone(&issues);
        let api = if cloud { "/rest/api/3" } else { "/rest/api/2" };
        let answer = move |method: &str, path: &str, headers: &[(String, String)], body: &[u8]| -> (u16, String) {
            if !headers.iter().any(|(k, v)| k == "authorization" && *v == auth) {
                return (401, json!({ "errorMessages": ["You are not authenticated."] }).to_string());
            }
            let Some(rest) = path.strip_prefix(api) else { return (404, String::new()) };
            let route = rest.split('?').next().unwrap_or("");
            let body: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
            let mut all = lock(&kept);
            match (method, route) {
                ("GET", "/search/jql") if cloud => {}
                ("GET", "/search") if !cloud => {}
                ("POST", "/issue") => {
                    let f = body.get("fields").cloned().unwrap_or_default();
                    let summary = f.get("summary").and_then(Value::as_str).unwrap_or("");
                    let description_ok = if cloud { f.get("description").is_some_and(adf_ok) } else { f.get("description").is_some_and(Value::is_string) };
                    let labels: Vec<String> = f.get("labels").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
                    if f.pointer("/project/key").and_then(Value::as_str) != Some("OPS") || f.pointer("/issuetype/name").and_then(Value::as_str) != Some("Task") {
                        return (400, json!({ "errorMessages": [], "errors": { "project": "valid project is required" } }).to_string());
                    }
                    if summary.is_empty() || summary.chars().count() > 255 || summary.contains('\n') || !description_ok || labels.iter().any(|l| l.contains(' ')) {
                        return (400, json!({ "errorMessages": [], "errors": { "summary": "Field 'summary' cannot be set: it is not one line of 255" } }).to_string());
                    }
                    let key = format!("OPS-{}", all.len() + 1);
                    all.push(Issue { key: key.clone(), labels, done: false, comments: 0 });
                    return (201, json!({ "id": format!("1000{}", all.len()), "key": key, "self": format!("{api}/issue/{key}") }).to_string());
                }
                _ => {
                    let Some((key, what)) = route.strip_prefix("/issue/").and_then(|r| r.split_once('/')) else { return (404, String::new()) };
                    let Some(issue) = all.iter_mut().find(|i| i.key == key) else { return (404, json!({ "errorMessages": ["Issue does not exist"] }).to_string()) };
                    return match (method, what) {
                        ("POST", "comment") if (cloud && body.get("body").is_some_and(adf_ok)) || (!cloud && body.get("body").is_some_and(Value::is_string)) => {
                            issue.comments += 1;
                            (201, json!({ "id": "1" }).to_string())
                        }
                        ("GET", "transitions") => (
                            200,
                            json!({ "transitions": [
                                { "id": "21", "name": "In Progress", "to": { "statusCategory": { "key": "indeterminate" } } },
                                { "id": "31", "name": "Done", "to": { "statusCategory": { "key": "done" } } },
                            ] })
                            .to_string(),
                        ),
                        ("POST", "transitions") => {
                            issue.done = body.pointer("/transition/id").and_then(Value::as_str) == Some("31");
                            (204, String::new())
                        }
                        _ => (400, json!({ "errorMessages": ["not as Jira takes it"] }).to_string()),
                    };
                }
            }
            // A search: the project's open issues of the label JQL names, the newest first.
            let jql = param(path, "jql");
            let label = jql.split("labels = \"").nth(1).and_then(|r| r.split('"').next()).unwrap_or("");
            if !jql.starts_with("project = \"OPS\" AND ") || !jql.contains("statusCategory != Done") {
                return (400, json!({ "errorMessages": [format!("Error in the JQL Query: {jql}")] }).to_string());
            }
            let found: Vec<Value> = all.iter().rev().filter(|i| !i.done && i.labels.iter().any(|l| l == label)).map(|i| json!({ "id": "1", "key": i.key })).take(1).collect();
            (200, json!({ "issues": found }).to_string())
        };
        Ok((Standin::start(Arc::new(answer))?, issues))
    }

    fn job(name: &str, integration: &Integration, notice: Notice) -> Job {
        Job { name: name.into(), integration: integration.clone(), notice }
    }

    /// A group's notices to Jira, as they come: its first alert an issue (labelled with the
    /// group's key), each change after a comment on that issue, its recovery a comment and the
    /// issue moved to done; an alert after that a new issue, another group's its own; on Data
    /// Center alike, in wiki markup and with a personal access token. A refusal is said in Jira's
    /// words, and wrong credentials stop it at the search.
    #[test]
    fn a_group_is_one_jira_issue_made_commented_on_and_closed() -> anyhow::Result<()> {
        let basic = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode("ops@example.com:T0K3N"));
        let (site, issues) = jira_standin(true, basic)?;
        let (dc, dc_issues) = jira_standin(false, "Bearer P4T".into())?;
        // The site's integration: one that closes issues, one that keeps them open, one whose token is wrong.
        let on_site = |token: &str, close: Option<bool>| Integration::Jira {
            url: format!("{}/", site.url()),
            edition: JiraEdition::Cloud,
            project: "OPS".into(),
            issue_type: None,
            user: Some("ops@example.com".into()),
            token: token.into(),
            close,
        };
        let (cloud, open, wrong) = (on_site("T0K3N", None), on_site("T0K3N", Some(false)), on_site("wrong", None));
        let center = Integration::Jira { url: dc.url(), edition: JiraEdition::DataCenter, project: "OPS".into(), issue_type: None, user: None, token: "P4T".into(), close: None };
        let other = Notice { group: vec![("host".into(), "web-2".into())], ..notice("ALERT") };
        let said = crate::rt::block_on(async move {
            let http = Client::public()?;
            let mut said = Vec::new();
            for (i, n) in [
                (&cloud, notice("ALERT")),
                (&cloud, notice("ALERT")),
                (&cloud, notice("WARN")),
                (&cloud, notice("OK")),
                (&cloud, notice("OK")),
                (&cloud, notice("ALERT")),
                (&cloud, other),
                (&open, notice("OK")),
                (&wrong, notice("ALERT")),
                (&center, notice("ALERT")),
                (&center, notice("OK")),
            ] {
                said.push(deliver_jira(&http, &job("ops", i, n)).await);
            }
            anyhow::Ok(said)
        })?;
        let outcomes: Vec<&str> = said.iter().map(|(_, o)| o.as_str()).collect();
        anyhow::ensure!(said.iter().take(8).all(|(ok, _)| *ok), "{said:?}");
        anyhow::ensure!(
            outcomes.get(..8)
                == Some(
                    &["201 made OPS-1", "commented on OPS-1", "commented on OPS-1", "commented on OPS-1 and closed it", "no issue open", "201 made OPS-2", "201 made OPS-3", "commented on OPS-2"][..]
                ),
            "{outcomes:?}"
        );
        let kept = lock(&issues).clone();
        let label = group_label(&notice("ALERT"));
        anyhow::ensure!(kept.len() == 3 && kept.iter().all(|i| i.labels.first().map(String::as_str) == Some("kiln")), "{kept:?}");
        anyhow::ensure!(kept.first().is_some_and(|i| i.done && i.comments == 3 && i.labels.contains(&label)), "the first issue commented on thrice and closed: {kept:?}");
        anyhow::ensure!(kept.get(1).is_some_and(|i| !i.done && i.comments == 1), "the one kept open (close: false) commented on: {kept:?}");
        anyhow::ensure!(kept.get(2).is_some_and(|i| !i.done && !i.labels.contains(&label)), "another group's issue its own: {kept:?}");
        anyhow::ensure!(said.get(8).is_some_and(|(ok, o)| !ok && o.starts_with("looking for the group's issue: 401 ") && o.contains("not authenticated")), "{said:?}");
        anyhow::ensure!(outcomes.get(9..) == Some(&["201 made OPS-1", "commented on OPS-1 and closed it"][..]) && lock(&dc_issues).first().is_some_and(|i| i.done), "Data Center: {outcomes:?}");
        // Each search names the project and the group's label; Cloud's at its own path.
        let searches: Vec<String> = site.got().into_iter().filter(|(m, p, _)| m == "GET" && p.starts_with("/rest/api/3/search/jql?")).map(|(_, p, _)| param(&p, "jql")).collect();
        anyhow::ensure!(searches.len() == 9 && searches.iter().take(5).all(|q| *q == jira_jql("OPS", &label)), "{searches:?}");
        Ok(())
    }

    /// What a ServiceNow stand-in keeps of an incident.
    #[derive(Debug, Clone)]
    struct Incident {
        sys_id: String,
        number: String,
        correlation: String,
        state: String,
        impact: String,
        urgency: String,
        notes: usize,
        close_code: String,
    }

    /// A ServiceNow instance's Table API as it answers: its credentials checked, incidents made,
    /// found by an encoded query (a correlation id, states not among those given), changed by
    /// PATCH; resolving one without its close code and notes refused as its data policy does.
    fn servicenow_standin(auth: String) -> anyhow::Result<(Standin, Arc<Mutex<Vec<Incident>>>)> {
        let incidents: Arc<Mutex<Vec<Incident>>> = Arc::default();
        let kept = Arc::clone(&incidents);
        let answer = move |method: &str, path: &str, headers: &[(String, String)], body: &[u8]| -> (u16, String) {
            if !headers.iter().any(|(k, v)| k == "authorization" && *v == auth) {
                return (401, json!({ "error": { "message": "User Not Authenticated", "detail": "Required to provide Auth information" }, "status": "failure" }).to_string());
            }
            let route = path.split('?').next().unwrap_or("");
            let body: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
            let text = |k: &str| body.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
            let mut all = lock(&kept);
            match (method, route) {
                ("GET", "/api/now/table/incident") => {
                    let q = param(path, "sysparm_query");
                    let parts: Vec<&str> = q.split('^').collect();
                    let correlation = parts.iter().find_map(|p| p.strip_prefix("correlation_id=")).unwrap_or("");
                    let not_in: Vec<&str> = parts.iter().find_map(|p| p.strip_prefix("stateNOT IN")).map(|s| s.split(',').collect()).unwrap_or_default();
                    if parts.last() != Some(&"ORDERBYDESCsys_created_on") || not_in != ["6", "7", "8"] {
                        return (400, json!({ "error": { "message": format!("not the query expected: {q}") } }).to_string());
                    }
                    let found: Vec<Value> =
                        all.iter().rev().filter(|i| i.correlation == correlation && !not_in.contains(&i.state.as_str())).map(|i| json!({ "sys_id": i.sys_id, "number": i.number })).take(1).collect();
                    (200, json!({ "result": found }).to_string())
                }
                ("POST", "/api/now/table/incident") => {
                    let short = text("short_description");
                    let level = |k: &str| ["1", "2", "3"].contains(&text(k).as_str());
                    if short.is_empty() || short.chars().count() > 160 || text("correlation_id").len() > 100 || !level("impact") || !level("urgency") {
                        return (400, json!({ "error": { "message": "Invalid field values" } }).to_string());
                    }
                    let number = format!("INC00100{:02}", all.len() + 1);
                    let sys_id = format!("{:032x}", all.len() + 1);
                    all.push(Incident {
                        sys_id: sys_id.clone(),
                        number: number.clone(),
                        correlation: text("correlation_id"),
                        state: "1".into(),
                        impact: text("impact"),
                        urgency: text("urgency"),
                        notes: 0,
                        close_code: String::new(),
                    });
                    (201, json!({ "result": { "sys_id": sys_id, "number": number, "state": "1" } }).to_string())
                }
                ("PATCH", r) => {
                    let Some(i) = r.strip_prefix("/api/now/table/incident/").and_then(|id| all.iter_mut().find(|i| i.sys_id == id)) else {
                        return (404, json!({ "error": { "message": "No Record found" } }).to_string());
                    };
                    if text("state") == "6" && (text("close_code").is_empty() || text("close_notes").is_empty()) {
                        return (
                            403,
                            json!({ "error": { "message": "Operation Failed", "detail": "Data Policy Exception: Resolution code, Resolution notes are mandatory" }, "status": "failure" }).to_string(),
                        );
                    }
                    if !text("work_notes").is_empty() {
                        i.notes += 1;
                    }
                    for (k, field) in [("state", &mut i.state), ("impact", &mut i.impact), ("urgency", &mut i.urgency), ("close_code", &mut i.close_code)] {
                        if body.get(k).is_some() {
                            *field = text(k);
                        }
                    }
                    (200, json!({ "result": { "sys_id": i.sys_id, "number": i.number, "state": i.state } }).to_string())
                }
                _ => (404, String::new()),
            }
        };
        Ok((Standin::start(Arc::new(answer))?, incidents))
    }

    /// A group's notices to ServiceNow, as they come: its first alert an incident (its correlation
    /// id the group's key, its impact and urgency its priority's), each change a work note and
    /// its impact and urgency the new state's, its recovery the incident resolved with the close
    /// code and notes the data policy wants; an alert after that a new incident (a resolved one
    /// is still active, so not found as open), and an OAuth token taken as a user's password.
    #[test]
    fn a_group_is_one_servicenow_incident_made_noted_and_resolved() -> anyhow::Result<()> {
        let basic = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode("kiln:pw"));
        let (instance, incidents) = servicenow_standin(basic)?;
        let (oauth, oauth_incidents) = servicenow_standin("Bearer 0AUTH".into())?;
        let snow =
            Integration::Servicenow { url: instance.url(), user: Some("kiln".into()), password: Some("pw".into()), token: None, assignment_group: None, caller: None, close: None, close_code: None };
        let token = Integration::Servicenow {
            url: oauth.url(),
            user: None,
            password: None,
            token: Some("0AUTH".into()),
            assignment_group: None,
            caller: None,
            close: None,
            close_code: Some("Solution provided".into()),
        };
        let warn = Notice { priority: None, ..notice("WARN") };
        let said = crate::rt::block_on(async move {
            let http = Client::public()?;
            let mut said = Vec::new();
            for (i, n) in [(&snow, warn), (&snow, notice("ALERT")), (&snow, notice("OK")), (&snow, notice("OK")), (&snow, notice("ALERT")), (&token, notice("ALERT")), (&token, notice("OK"))] {
                said.push(deliver_servicenow(&http, &job("itsm", i, n)).await);
            }
            anyhow::Ok(said)
        })?;
        let outcomes: Vec<&str> = said.iter().map(|(_, o)| o.as_str()).collect();
        anyhow::ensure!(
            said.iter().all(|(ok, _)| *ok)
                && outcomes == ["201 made INC0010001", "noted on INC0010001", "resolved INC0010001", "no incident open", "201 made INC0010002", "201 made INC0010001", "resolved INC0010001"],
            "{said:?}"
        );
        let kept = lock(&incidents).clone();
        let first = kept.first().cloned();
        anyhow::ensure!(
            first.as_ref().is_some_and(|i| i.state == "6" && i.close_code == SERVICENOW_CLOSE_CODE && i.notes == 1 && i.impact == "1" && i.urgency == "1"),
            "noted, its priority the alert's P1, then resolved: {kept:?}"
        );
        anyhow::ensure!(kept.get(1).is_some_and(|i| i.state == "1" && Some(&i.correlation) == first.as_ref().map(|f| &f.correlation)), "an alert after its resolve a new incident: {kept:?}");
        anyhow::ensure!(lock(&oauth_incidents).first().is_some_and(|i| i.state == "6" && i.close_code == "Solution provided"), "{:?}", lock(&oauth_incidents));
        Ok(())
    }

    /// A group's recovery waits for its alert while that is tried again (429, Retry-After: 1): the
    /// service is told the trigger, then the resolve. Another group's alert goes at once, not
    /// behind them.
    #[test]
    fn a_group_s_recovery_waits_for_its_alert_tried_again() -> anyhow::Result<()> {
        let refused = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let r = Arc::clone(&refused);
        let service = Standin::start(Arc::new(move |_: &str, _: &str, _: &[(String, String)], _: &[u8]| -> (u16, String) {
            if r.swap(true, std::sync::atomic::Ordering::SeqCst) { (202, "{}".into()) } else { (429, String::new()) }
        }))?;
        let pd = Integration::Pagerduty { routing_key: "R0UT1NG".into(), url: Some(format!("{}/v2/enqueue", service.url())) };
        let other = Notice { group: vec![("host".into(), "web-2".into())], ..notice("ALERT") };
        let got = Arc::clone(&service.got);
        let started = std::time::Instant::now();
        crate::rt::block_on(async move {
            let notifier = Notifier::start(Arc::new(Client::public()?));
            notifier.send("oncall", pd.clone(), notice("ALERT"));
            crate::rt::sleep(Duration::from_millis(100)).await;
            notifier.send("oncall", pd.clone(), notice("OK"));
            notifier.send("oncall", pd, other);
            let end = std::time::Instant::now() + Duration::from_secs(20);
            while lock(&got).len() < 4 && std::time::Instant::now() < end {
                crate::rt::sleep(Duration::from_millis(20)).await;
            }
            anyhow::Ok(())
        })?;
        let told: Vec<(String, String)> = service
            .got()
            .iter()
            .map(|(_, _, b)| {
                let v: Value = serde_json::from_slice(b).unwrap_or(Value::Null);
                (at(&v, &["event_action"]).as_str().unwrap_or("").to_owned(), at(&v, &["payload", "source"]).as_str().unwrap_or("").to_owned())
            })
            .collect();
        let pair = |a: &str, h: &str| (a.to_owned(), h.to_owned());
        let web1: Vec<_> = told.iter().filter(|(_, h)| h == "web-1").cloned().collect();
        anyhow::ensure!(web1 == [pair("trigger", "web-1"), pair("trigger", "web-1"), pair("resolve", "web-1")], "the trigger, tried again, then its resolve: {told:?}");
        let second = told.iter().position(|t| *t == pair("trigger", "web-2"));
        let retried = told.iter().enumerate().filter(|(_, t)| **t == pair("trigger", "web-1")).nth(1).map(|(i, _)| i);
        anyhow::ensure!(second.is_some() && second < retried, "another group's alert at once, before the first's is tried again: {told:?}");
        anyhow::ensure!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
        Ok(())
    }
}
