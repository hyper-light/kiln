//! Synthetic checks (`docs/observe.md` §10.4): Datadog's Synthetic API tests, an endpoint tried on a
//! schedule from the machines that are its locations (the server, runners by name, or every
//! machine), each run judged by its assertions and written as Datadog writes it.
//!
//! - Checks: **HTTP** (a request, its redirects followed or not; its time cut into DNS, connection,
//!   TLS, first byte and download, each measured on the way), **TCP** (a connection), **DNS** (a
//!   name resolved by the machine's resolver) and **TLS** (a handshake, the certificate verified
//!   for the host against the public roots or a CA given, and the days until it expires).
//! - Assertions, Datadog's: the status code, the response time, a header, the body, the
//!   certificate's days left, an address resolved; each `is`, `is_not`, `less_than`, `more_than`,
//!   `contains`, `does_not_contain`, `matches`, `does_not_match` as it applies. A run that fails is
//!   tried again (`retries`, `retry_ms` apart) before it counts.
//! - Each run writes Datadog's series of it (`synthetics.test_runs` by `status`;
//!   `synthetics.http.response.time`, `.dns.time`, `.connect.time`, `.ssl.time`, `.firstbyte.time`,
//!   `.download.time`, `.redirect.time`, `.response.size`; `synthetics.ssl.handshake.time`,
//!   `.time_to_expiry` in minutes; `synthetics.dns.response.time`; `synthetics.tcp.connect.time`,
//!   ...), tagged `test`, `location` and `subtype`, and kiln's own `synthetics.failing` (1 or 0); a
//!   location that starts failing, or recovers, is an event once.
//! - Alerting, Datadog's: a test that says `alert` owns a monitor (`syn-<name>`) that alerts when at
//!   least `min_locations` locations have failed for `for_s`, its message the test's (mentions and
//!   all); it is changed and removed with the test.
//! - Runs are spread through their interval by a hash of the test (as scrapes are) and kept to their
//!   schedule. The server keeps the tests (`<observe>/synthetics.json`, a bearer token sealed under
//!   the master key) and tells each runner its own; each machine says how its runs go.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use base64::Engine as _;
use bytes::Bytes;
use http_body_util::BodyExt as _;
use kiln_obs::batch::{Kind, SeriesKey};
use kiln_obs::logs::record::Record;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The longest interval (a week, Datadog's) and the shortest (kiln's: Datadog's API tests run every
/// minute at the most often; a site's own may run more often).
const EVERY_MAX_S: u64 = 7 * 86_400;
const EVERY_MIN_S: u64 = 10;
/// What a run waits for at the most, and by default (Datadog's: 60 s, unless said).
const TIMEOUT_MAX_S: u64 = 60;
/// Redirects followed at the most (Datadog's: 10).
const REDIRECTS: usize = 10;
/// A body read at the most: what assertions read of it.
const BODY_MAX: usize = 4 << 20;
/// Retries at the most (Datadog's: 3).
const RETRIES_MAX: u8 = 3;
const SEALED: &str = "sealed:v1:";
/// The machine that is the server, and every machine.
pub const SERVER: &str = "server";
pub const EVERY: &str = "*";

fn every_default() -> u64 {
    60
}

fn timeout_default() -> u64 {
    30
}

fn retry_ms_default() -> u64 {
    300
}

fn locations_default() -> Vec<String> {
    vec![SERVER.to_owned()]
}

fn yes() -> bool {
    true
}

fn get() -> String {
    "GET".into()
}

fn port_443() -> u16 {
    443
}

/// What a test tries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Check {
    Http {
        url: String,
        #[serde(default = "get")]
        method: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        headers: Vec<(String, String)>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        body: Option<String>,
        #[serde(default = "yes")]
        follow_redirects: bool,
        /// A bearer token the endpoint asks for: sealed at rest, never shown.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bearer: Option<String>,
        /// A CA (PEM) the endpoint's certificate is signed by, when not a public one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ca: Option<String>,
    },
    Tcp {
        host: String,
        port: u16,
    },
    Dns {
        host: String,
    },
    Tls {
        host: String,
        #[serde(default = "port_443")]
        port: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ca: Option<String>,
    },
}

impl Check {
    pub fn subtype(&self) -> &'static str {
        match self {
            Self::Http { .. } => "http",
            Self::Tcp { .. } => "tcp",
            Self::Dns { .. } => "dns",
            Self::Tls { .. } => "ssl",
        }
    }
}

/// An assertion on a run, Datadog's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assertion {
    /// `status_code`, `response_time` (ms), `header`, `body`, `certificate_days`, `address`.
    #[serde(rename = "type")]
    pub kind: String,
    pub operator: String,
    /// A header's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub property: Option<String>,
    pub target: Value,
}

/// When a test's own monitor alerts: at least `min_locations` failing for `for_s`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Alerting {
    /// Its message: mentions (`@slack-ops`) and Datadog's template variables.
    #[serde(default)]
    pub message: String,
    #[serde(default = "one")]
    pub min_locations: u32,
    #[serde(default)]
    pub for_s: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<u8>,
}

fn one() -> u32 {
    1
}

/// A synthetic test.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Test {
    #[serde(flatten)]
    pub check: Check,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assertions: Vec<Assertion>,
    /// `server`, runners by name, or `*` (every machine).
    #[serde(default = "locations_default")]
    pub locations: Vec<String>,
    #[serde(default = "every_default")]
    pub every_s: u64,
    #[serde(default = "timeout_default")]
    pub timeout_s: u64,
    #[serde(default)]
    pub retries: u8,
    #[serde(default = "retry_ms_default")]
    pub retry_ms: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub paused: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alert: Option<Alerting>,
}

/// The kinds of assertion each check takes.
fn kinds_for(check: &Check) -> &'static [&'static str] {
    match check {
        Check::Http { .. } => &["status_code", "response_time", "header", "body", "certificate_days"],
        Check::Tcp { .. } => &["response_time"],
        Check::Dns { .. } => &["response_time", "address"],
        Check::Tls { .. } => &["response_time", "certificate_days"],
    }
}

fn valid_host(h: &str) -> bool {
    !h.is_empty() && h.len() <= 253 && !h.contains(|c: char| c.is_whitespace() || c.is_control() || "/@?#".contains(c))
}

impl Test {
    pub fn check(&self) -> Result<(), String> {
        match &self.check {
            Check::Http { url, method, headers, ca, .. } => {
                let uri: http::Uri = url.parse().map_err(|_| format!("a test's URL: {url}"))?;
                if !matches!(uri.scheme_str(), Some("http" | "https")) || uri.host().is_none_or(str::is_empty) {
                    return Err(format!("a test's URL is http:// or https:// with a host: {url}"));
                }
                if http::Method::from_bytes(method.as_bytes()).is_err() {
                    return Err(format!("method {method:?}"));
                }
                if let Some((k, _)) = headers.iter().find(|(k, v)| http::HeaderName::from_bytes(k.as_bytes()).is_err() || http::HeaderValue::from_str(v).is_err()) {
                    return Err(format!("header {k:?}: a header's name and a value without line breaks"));
                }
                if let Some(pem) = ca {
                    crate::http::tls::ClientTls::new(crate::http::client::Roots::Pem(pem.as_bytes().to_vec()), None).map_err(|_| "ca: a CA's certificates, PEM".to_owned())?;
                }
            }
            Check::Tcp { host, port } | Check::Tls { host, port, .. } => {
                if !valid_host(host) || *port == 0 {
                    return Err(format!("{host:?}:{port}: a host and a port"));
                }
            }
            Check::Dns { host } => {
                if !valid_host(host) {
                    return Err(format!("{host:?}: a name to resolve"));
                }
            }
        }
        if !(EVERY_MIN_S..=EVERY_MAX_S).contains(&self.every_s) {
            return Err(format!("every_s: {EVERY_MIN_S} to {EVERY_MAX_S}"));
        }
        // A run longer than its interval puts the next an interval after it: no bound between them.
        if self.timeout_s == 0 || self.timeout_s > TIMEOUT_MAX_S {
            return Err(format!("timeout_s: 1 to {TIMEOUT_MAX_S}"));
        }
        if self.retries > RETRIES_MAX || self.retry_ms > 60_000 {
            return Err(format!("retries: 0 to {RETRIES_MAX}, retry_ms up to 60,000"));
        }
        if self.locations.is_empty() || self.locations.len() > 64 || self.locations.iter().any(|l| l.is_empty() || l.len() > 255) {
            return Err("locations: `server`, runners' names, or `*`".into());
        }
        let kinds = kinds_for(&self.check);
        for a in &self.assertions {
            if !kinds.contains(&a.kind.as_str()) {
                return Err(format!("a {} test's assertions: {}", self.check.subtype(), kinds.join(", ")));
            }
            let numeric = matches!(a.kind.as_str(), "status_code" | "response_time" | "certificate_days");
            let ops: &[&str] = if numeric { &["is", "is_not", "less_than", "more_than"] } else { &["is", "is_not", "contains", "does_not_contain", "matches", "does_not_match"] };
            if !ops.contains(&a.operator.as_str()) {
                return Err(format!("{}'s operators: {}", a.kind, ops.join(", ")));
            }
            if numeric && a.target.as_f64().is_none() {
                return Err(format!("{}'s target is a number", a.kind));
            }
            if !numeric && a.target.as_str().is_none() {
                return Err(format!("{}'s target is text", a.kind));
            }
            if a.kind == "header" && a.property.as_deref().is_none_or(str::is_empty) {
                return Err("a header assertion names its header (property)".into());
            }
            if a.operator.ends_with("match") || a.operator == "matches" {
                regex::Regex::new(a.target.as_str().unwrap_or("")).map_err(|e| format!("{}'s pattern: {e}", a.kind))?;
            }
        }
        for t in &self.tags {
            if t.split_once(':').is_none_or(|(k, v)| k.is_empty() || v.is_empty()) {
                return Err(format!("a tag is key:value: {t}"));
            }
        }
        if let Some(a) = &self.alert
            && (a.min_locations == 0 || a.for_s > 86_400)
        {
            return Err("alert: min_locations 1 or more, for_s up to a day".into());
        }
        Ok(())
    }

    /// Whether `machine` runs it (`server` for the server).
    pub fn run_by(&self, machine: &str) -> bool {
        self.locations.iter().any(|l| l == EVERY || l == machine)
    }

    /// As the API shows it: its token `"…"`.
    pub fn redacted(&self) -> Value {
        let mut t = self.clone();
        if let Check::Http { bearer: Some(b), .. } = &mut t.check {
            *b = "…".into();
        }
        serde_json::to_value(&t).unwrap_or(Value::Null)
    }

    fn bearer_mut(&mut self) -> Option<&mut Option<String>> {
        match &mut self.check {
            Check::Http { bearer, .. } => Some(bearer),
            _ => None,
        }
    }

    /// The monitor it owns, if it alerts: `min(last_<W>):sum:synthetics.failing{test:<name>}
    /// .rollup(max, <every>) >= <min_locations>`, each location's failing a bucket summed, every
    /// bucket of the window at least `min_locations`.
    pub fn monitor(&self, name: &str) -> Option<super::monitors::Monitor> {
        let a = self.alert.as_ref()?;
        let window_m = a.for_s.max(self.every_s).div_ceil(60).max(1);
        let query = format!("min(last_{window_m}m):sum:synthetics.failing{{test:{name}}}.rollup(max, {}) >= {}", self.every_s, a.min_locations);
        let message = if a.message.trim().is_empty() { format!("{{{{#is_alert}}}}Synthetic test {name} is failing in {{{{value}}}} locations.{{{{/is_alert}}}}{{{{#is_recovery}}}}Synthetic test {name} recovered.{{{{/is_recovery}}}}") } else { a.message.clone() };
        let mut tags = vec![format!("synthetics:{name}")];
        tags.extend(self.tags.iter().cloned());
        Some(super::monitors::Monitor {
            id: monitor_id(name),
            name: format!("Synthetic test {name}"),
            kind: None,
            query,
            message,
            tags,
            priority: a.priority,
            // Judged as often as the test runs (10 s to a minute).
            options: super::monitors::Options { thresholds: super::monitors::ThresholdsIn { critical: Some(f64::from(a.min_locations)), ..Default::default() }, every: Some(u32::try_from(self.every_s.clamp(10, 60)).unwrap_or(60)), ..Default::default() },
            muted_until: None,
            created: 0,
            modified: 0,
        })
    }
}

/// The id of the monitor test `name` owns.
pub fn monitor_id(name: &str) -> String {
    format!("syn-{name}")
}

// ------------------------------------------------------------------------------------- outcome

/// What a run came to.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Outcome {
    pub passed: bool,
    /// Why not: the error, or each assertion that failed.
    pub failures: Vec<String>,
    /// Milliseconds: the whole, and its parts where measured.
    pub total_ms: f64,
    pub dns_ms: Option<f64>,
    pub connect_ms: Option<f64>,
    pub tls_ms: Option<f64>,
    pub firstbyte_ms: Option<f64>,
    pub download_ms: Option<f64>,
    pub redirect_ms: Option<f64>,
    pub status: Option<u16>,
    pub size: Option<u64>,
    /// Days until the certificate expires (fractions and all).
    pub certificate_days: Option<f64>,
    pub addresses: Vec<String>,
    pub attempts: u32,
}

/// What a check saw, before its assertions: the response's head and body where there is one.
#[derive(Debug, Default)]
struct Seen {
    headers: Vec<(String, String)>,
    body: Bytes,
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// The leaf certificate's days until it expires.
fn days_left(der: &[u8]) -> Option<f64> {
    let (_, cert) = x509_parser::parse_x509_certificate(der).ok()?;
    let until = cert.validity().not_after.timestamp();
    Some((until as f64 - crate::util::now_ms() as f64 / 1000.0) / 86_400.0)
}

fn tls_config(ca: Option<&str>) -> anyhow::Result<crate::http::tls::ClientTls> {
    let roots = ca.map_or(crate::http::client::Roots::WebPki, |pem| crate::http::client::Roots::Pem(pem.as_bytes().to_vec()));
    crate::http::tls::ClientTls::new(roots, None)
}

/// A connection, timed: its addresses resolved (DNS), the first that answers connected.
async fn connect(host: &str, port: u16, out: &mut Outcome) -> anyhow::Result<hyper_rt::tcp::TcpStream> {
    let t = Instant::now();
    let mut addrs = crate::rt::resolve_host(host, port).await?;
    out.dns_ms = Some(ms(t.elapsed()));
    addrs.sort_by_key(|a| !a.is_ipv4());
    out.addresses = addrs.iter().map(|a| a.ip().to_string()).collect();
    let t = Instant::now();
    let mut last = None;
    for addr in addrs {
        match hyper_rt::tcp::TcpStream::connect(addr).await {
            Ok(s) => {
                out.connect_ms = Some(ms(t.elapsed()));
                return Ok(s);
            }
            Err(e) => last = Some(format!("connect {addr}: {}", crate::rt::to_io(e))),
        }
    }
    Err(anyhow::anyhow!(last.unwrap_or_else(|| format!("{host}: no address"))))
}

/// One HTTP exchange, timed, on a connection of its own: the response's status, head and body.
async fn exchange(url: &str, check: &Check, out: &mut Outcome) -> anyhow::Result<(u16, Seen, Option<String>)> {
    let Check::Http { method, headers, body, bearer, ca, .. } = check else { anyhow::bail!("not an HTTP test") };
    let uri: http::Uri = url.parse()?;
    let https = uri.scheme_str() == Some("https");
    let host = uri.host().ok_or_else(|| anyhow::anyhow!("no host in {url}"))?.to_owned();
    let port = uri.port_u16().unwrap_or(if https { 443 } else { 80 });
    let authority = uri.authority().map_or_else(|| host.clone(), |a| a.as_str().to_owned());
    let path = uri.path_and_query().map_or("/", http::uri::PathAndQuery::as_str).to_owned();
    let tcp = connect(&host, port, out).await?;
    let mut req = http::Request::builder().method(method.as_str()).uri(path).header(http::header::HOST, authority).header(http::header::USER_AGENT, "kiln-synthetics");
    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }
    if let Some(b) = bearer {
        req = req.header(http::header::AUTHORIZATION, format!("Bearer {b}"));
    }
    let req = req.body(body.clone().map_or_else(crate::http::server::empty, crate::http::server::full))?;
    let t = Instant::now();
    let response = if https {
        let tls = tls_config(ca.as_deref())?;
        let stream = crate::http::tls::TlsStream::connect(crate::http::io::RtFuturesIo::new(tcp), tls.config()?, crate::http::tls::server_name(&host)?).await.map_err(|e| anyhow::anyhow!("TLS to {host}: {e}"))?;
        out.tls_ms = Some(ms(t.elapsed()));
        out.certificate_days = stream.peer_certificates().and_then(|c| c.first()).and_then(|c| days_left(c));
        let (mut sender, conn) = hyper::client::conn::http1::handshake::<_, crate::http::server::Body>(stream).await?;
        crate::rt::spawn_local(async move {
            let _ = conn.await;
        })
        .map_err(|e| anyhow::anyhow!("spawn connection task: {e}"))?;
        let t = Instant::now();
        let r = sender.send_request(req).await?;
        out.firstbyte_ms = Some(ms(t.elapsed()));
        r
    } else {
        let (mut sender, conn) = hyper::client::conn::http1::handshake::<_, crate::http::server::Body>(crate::http::io::RtIo::new(tcp)).await?;
        crate::rt::spawn_local(async move {
            let _ = conn.await;
        })
        .map_err(|e| anyhow::anyhow!("spawn connection task: {e}"))?;
        let t = Instant::now();
        let r = sender.send_request(req).await?;
        out.firstbyte_ms = Some(ms(t.elapsed()));
        r
    };
    let status = response.status().as_u16();
    let seen_headers: Vec<(String, String)> = response.headers().iter().filter_map(|(k, v)| Some((k.as_str().to_owned(), v.to_str().ok()?.to_owned()))).collect();
    let location = response.headers().get(http::header::LOCATION).and_then(|v| v.to_str().ok()).map(str::to_owned);
    let t = Instant::now();
    let body = http_body_util::Limited::new(response.into_body(), BODY_MAX).collect().await.map_err(|e| anyhow::anyhow!("the body: {e}"))?.to_bytes();
    out.download_ms = Some(ms(t.elapsed()));
    out.size = Some(body.len() as u64);
    Ok((status, Seen { headers: seen_headers, body }, location))
}

/// `location` (a redirect's) against `base`: absolute, or a path on the same origin.
fn resolve(base: &str, location: &str) -> String {
    if location.starts_with("http://") || location.starts_with("https://") {
        return location.to_owned();
    }
    let origin = base.splitn(4, '/').take(3).collect::<Vec<_>>().join("/");
    if location.starts_with('/') { format!("{origin}{location}") } else { format!("{}/{location}", base.rsplit_once('/').map_or(origin.as_str(), |(b, _)| b)) }
}

/// One try of a test: what it saw, its assertions judged.
async fn try_once(t: &Test) -> Outcome {
    let started = Instant::now();
    let mut out = Outcome::default();
    let seen: anyhow::Result<Seen> = async {
        match &t.check {
            Check::Http { url, follow_redirects, .. } => {
                let mut url = url.clone();
                let mut hops = 0;
                let redirects_started = Instant::now();
                loop {
                    let hop_started = Instant::now();
                    let (status, seen, location) = exchange(&url, &t.check, &mut out).await?;
                    out.status = Some(status);
                    match location {
                        Some(next) if *follow_redirects && (300..400).contains(&status) && hops < REDIRECTS => {
                            hops += 1;
                            url = resolve(&url, &next);
                        }
                        _ => {
                            if hops > 0 {
                                out.redirect_ms = Some(ms(redirects_started.elapsed()) - ms(hop_started.elapsed()));
                            }
                            return Ok(seen);
                        }
                    }
                }
            }
            Check::Tcp { host, port } => {
                connect(host, *port, &mut out).await?;
                Ok(Seen::default())
            }
            Check::Dns { host } => {
                let t0 = Instant::now();
                let addrs = crate::rt::resolve_host(host, 0).await?;
                out.dns_ms = Some(ms(t0.elapsed()));
                out.addresses = addrs.iter().map(|a| a.ip().to_string()).collect();
                anyhow::ensure!(!out.addresses.is_empty(), "{host} resolves to no address");
                Ok(Seen::default())
            }
            Check::Tls { host, port, ca } => {
                let tcp = connect(host, *port, &mut out).await?;
                let tls = tls_config(ca.as_deref())?;
                let t0 = Instant::now();
                let stream = crate::http::tls::TlsStream::connect(crate::http::io::RtFuturesIo::new(tcp), tls.config()?, crate::http::tls::server_name(host)?).await.map_err(|e| anyhow::anyhow!("TLS to {host}: {e}"))?;
                out.tls_ms = Some(ms(t0.elapsed()));
                out.certificate_days = stream.peer_certificates().and_then(|c| c.first()).and_then(|c| days_left(c));
                Ok(Seen::default())
            }
        }
    }
    .await;
    out.total_ms = ms(started.elapsed());
    match seen {
        Ok(seen) => {
            out.failures = judge(t, &out, &seen);
            out.passed = out.failures.is_empty();
        }
        Err(e) => {
            out.failures = vec![format!("{e:#}")];
            out.passed = false;
        }
    }
    out
}

/// A test run, its timeout over all of it, tried again as it says.
pub async fn run(t: &Test) -> Outcome {
    let mut attempts = 0;
    loop {
        attempts += 1;
        let timed = crate::rt::timeout(Duration::from_secs(t.timeout_s), try_once(t)).await;
        let mut out = timed.unwrap_or_else(|_| Outcome { failures: vec![format!("no answer within {} s", t.timeout_s)], total_ms: t.timeout_s as f64 * 1000.0, ..Outcome::default() });
        out.attempts = attempts;
        if out.passed || attempts > u32::from(t.retries) {
            return out;
        }
        crate::rt::sleep(Duration::from_millis(t.retry_ms)).await;
    }
}

/// Each assertion that fails, said; an HTTP test without assertions asks for a status under 400.
fn judge(t: &Test, out: &Outcome, seen: &Seen) -> Vec<String> {
    let default = [Assertion { kind: "status_code".into(), operator: "less_than".into(), property: None, target: json!(400) }];
    let asked: &[Assertion] = if t.assertions.is_empty() && matches!(t.check, Check::Http { .. }) { &default } else { &t.assertions };
    let mut failures = Vec::new();
    for a in asked {
        let actual: Option<Value> = match a.kind.as_str() {
            "status_code" => out.status.map(Value::from),
            "response_time" => Some(json!(out.total_ms)),
            "certificate_days" => out.certificate_days.map(Value::from),
            "header" => {
                let name = a.property.as_deref().unwrap_or("");
                seen.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| Value::from(v.clone()))
            }
            "body" => Some(Value::from(String::from_utf8_lossy(&seen.body).into_owned())),
            "address" => Some(Value::from(out.addresses.join(" "))),
            _ => None,
        };
        let holds = match (&actual, a.target.as_f64(), a.target.as_str()) {
            (Some(v), Some(target), _) if v.is_number() => {
                let v = v.as_f64().unwrap_or(f64::NAN);
                match a.operator.as_str() {
                    "is" => v == target,
                    "is_not" => v != target,
                    "less_than" => v < target,
                    "more_than" => v > target,
                    _ => false,
                }
            }
            (Some(v), _, Some(target)) => {
                let text = v.as_str().unwrap_or("");
                // An address assertion is of any address resolved.
                let any = |f: &dyn Fn(&str) -> bool| if a.kind == "address" { out.addresses.iter().any(|x| f(x)) } else { f(text) };
                match a.operator.as_str() {
                    "is" => any(&|x| x == target),
                    "is_not" => !any(&|x| x == target),
                    "contains" => any(&|x| x.contains(target)),
                    "does_not_contain" => !any(&|x| x.contains(target)),
                    "matches" => regex::Regex::new(target).is_ok_and(|re| any(&|x| re.is_match(x))),
                    "does_not_match" => regex::Regex::new(target).is_ok_and(|re| !any(&|x| re.is_match(x))),
                    _ => false,
                }
            }
            _ => false,
        };
        if !holds {
            let said = match &actual {
                Some(Value::String(s)) if s.len() > 120 => format!("{}…", s.chars().take(120).collect::<String>()),
                Some(Value::String(s)) => format!("{s:?}"),
                Some(v) => v.to_string(),
                None => "nothing".into(),
            };
            let what = a.property.as_deref().map_or_else(|| a.kind.clone(), |p| format!("{} {p}", a.kind));
            failures.push(format!("{what} {} {}: was {said}", a.operator.replace('_', " "), a.target));
        }
    }
    failures
}

// ---------------------------------------------------------------------------------------- store

/// How a machine's runs of a test go, as its status says.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// The last run's: none before the first.
    pub passed: Option<bool>,
    /// Unix ms.
    pub last_ms: Option<i64>,
    /// The last run's time (µs), status code, certificate's minutes left, tries, and why it failed.
    pub took_us: u64,
    pub status: Option<u16>,
    pub certificate_minutes: Option<i64>,
    pub attempts: u32,
    pub failed: Vec<String>,
    pub runs: u64,
    pub failures: u64,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The server's tests by name, kept in `<observe>/synthetics.json` (a token sealed under the
/// master key, the file the owner's alone).
pub struct Tests {
    path: PathBuf,
    master: PathBuf,
    all: Mutex<BTreeMap<String, Test>>,
}

fn aad(name: &str) -> Vec<u8> {
    format!("kiln observe synthetics\0{name}\0bearer").into_bytes()
}

impl Tests {
    pub fn open(dir: &Path, master: &Path) -> Self {
        let path = dir.join("synthetics.json");
        let mut all: BTreeMap<String, Test> = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        for (name, t) in &mut all {
            if let Some(b) = t.bearer_mut()
                && let Some(s) = b.as_deref().and_then(|s| s.strip_prefix(SEALED))
            {
                *b = match unseal(master, name, s) {
                    Ok(plain) => Some(plain),
                    Err(e) => {
                        tracing::warn!(test = %name, error = %format!("{e:#}"), "a synthetic test's token does not open: give it again");
                        None
                    }
                };
            }
        }
        Self { path, master: master.to_owned(), all: Mutex::new(all) }
    }

    pub fn list(&self) -> Vec<(String, Test)> {
        lock(&self.all).iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    pub fn get(&self, name: &str) -> Option<Test> {
        lock(&self.all).get(name).cloned()
    }

    /// Keeps `t` as `name` (a token left out, or `"…"`, is the old one's).
    pub fn put(&self, name: &str, mut t: Test) -> Result<(), String> {
        if name.is_empty() || name.len() > 64 || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)) {
            return Err("a test's name is 1 to 64 letters, digits, '-', '_' and '.'".into());
        }
        t.check()?;
        let mut all = lock(&self.all);
        let old = all.get(name).cloned();
        if let Some(b) = t.bearer_mut()
            && (b.is_none() || b.as_deref() == Some("…"))
        {
            *b = old.and_then(|mut o| o.bearer_mut().and_then(|x| x.clone()));
        }
        all.insert(name.to_owned(), t);
        self.save(&all).map_err(|e| format!("{e:#}"))
    }

    pub fn delete(&self, name: &str) -> anyhow::Result<bool> {
        let mut all = lock(&self.all);
        let had = all.remove(name).is_some();
        self.save(&all)?;
        Ok(had)
    }

    fn save(&self, all: &BTreeMap<String, Test>) -> anyhow::Result<()> {
        let mut sealed = all.clone();
        for (name, t) in &mut sealed {
            if let Some(b) = t.bearer_mut()
                && let Some(plain) = b.as_deref()
            {
                let (ct, nonce) = crate::ci::secrets::seal_with(&self.master, plain.as_bytes(), &aad(name))?;
                let b64 = base64::engine::general_purpose::STANDARD_NO_PAD;
                *b = Some(format!("{SEALED}{}:{}", b64.encode(nonce), b64.encode(ct)));
            }
        }
        crate::util::write_secret(&self.path, &serde_json::to_vec_pretty(&sealed)?, 0o600)?;
        Ok(())
    }
}

fn unseal(master: &Path, name: &str, s: &str) -> anyhow::Result<String> {
    let b64 = base64::engine::general_purpose::STANDARD_NO_PAD;
    let (nonce, ct) = s.split_once(':').ok_or_else(|| anyhow::anyhow!("not a sealed token"))?;
    let plain = crate::ci::secrets::open_with(master, &b64.decode(ct)?, &b64.decode(nonce)?, &aad(name))?;
    Ok(String::from_utf8(plain)?)
}

// --------------------------------------------------------------------------------------- runner

/// A machine's synthetic tests: each it runs, its own task.
pub struct Runner {
    machine: String,
    running: Mutex<HashMap<String, Running>>,
    stop: crate::rt::Shutdown,
}

struct Running {
    test: Test,
    stop: crate::rt::Shutdown,
    now: Arc<crate::rt::Notify>,
    state: Arc<Mutex<State>>,
}

static RUNNER: OnceLock<Arc<Runner>> = OnceLock::new();

/// This process's runner, started once: `machine` the location its tests name it (`server` for the
/// server).
pub fn start(machine: &str, stop: crate::rt::Shutdown) -> &'static Arc<Runner> {
    RUNNER.get_or_init(|| Arc::new(Runner { machine: machine.to_owned(), running: Mutex::new(HashMap::new()), stop }))
}

pub fn runner() -> Option<&'static Arc<Runner>> {
    RUNNER.get()
}

impl Runner {
    pub fn machine(&self) -> &str {
        &self.machine
    }

    /// Runs `tests` (of every machine: those this one runs are kept) from now: one unchanged goes on
    /// as it was, a changed one starts again, one gone (or paused) stops.
    pub fn set(&self, tests: &[(String, Test)]) {
        let mine: HashMap<&str, &Test> = tests.iter().filter(|(_, t)| t.run_by(&self.machine) && !t.paused).map(|(n, t)| (n.as_str(), t)).collect();
        let mut running = lock(&self.running);
        running.retain(|name, r| {
            let keep = mine.get(name.as_str()).is_some_and(|t| **t == r.test);
            if !keep {
                r.stop.cancel();
            }
            keep
        });
        for (name, t) in mine {
            if running.contains_key(name) {
                continue;
            }
            let r = Running { test: t.clone(), stop: crate::rt::Shutdown::new(), now: Arc::new(crate::rt::Notify::new()), state: Arc::new(Mutex::new(State::default())) };
            let (stop, server, now, state, test, name_owned, machine) = (r.stop.clone(), self.stop.clone(), r.now.clone(), r.state.clone(), t.clone(), name.to_owned(), self.machine.clone());
            crate::rt::spawn(async move { run_loop(name_owned, machine, test, stop, server, now, state).await });
            running.insert(name.to_owned(), r);
        }
    }

    /// How each of this machine's tests goes.
    pub fn states(&self) -> Vec<(String, State)> {
        let mut out: Vec<(String, State)> = lock(&self.running).iter().map(|(n, r)| (n.clone(), lock(&r.state).clone())).collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Runs `name` now, not at its time: whether this machine runs it.
    pub fn run_now(&self, name: &str) -> bool {
        lock(&self.running).get(name).map(|r| r.now.notify()).is_some()
    }
}

/// The test's offset into its interval: a hash of it, so each keeps its place across restarts
/// and tests spread through their interval.
fn offset(name: &str, every: Duration) -> Duration {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.bytes() {
        h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
    }
    Duration::from_millis(h % u64::try_from(every.as_millis()).unwrap_or(u64::MAX).max(1))
}

/// A test's task: a run at each of its times (sooner when asked) until it is changed, gone, or the
/// process stops.
async fn run_loop(name: String, machine: String, t: Test, stop: crate::rt::Shutdown, server: crate::rt::Shutdown, now: Arc<crate::rt::Notify>, state: Arc<Mutex<State>>) {
    let every = Duration::from_secs(t.every_s);
    let mut next = Instant::now() + offset(&name, every);
    loop {
        let wait = next.saturating_duration_since(Instant::now());
        let slept = std::pin::pin!(stop.sleep(wait));
        let asked = std::pin::pin!(now.notified());
        let ended = std::pin::pin!(server.cancelled());
        match futures::future::select(futures::future::select(slept, asked), ended).await {
            futures::future::Either::Left((futures::future::Either::Left((false, _)), _)) | futures::future::Either::Right(_) => return,
            _ => {}
        }
        if stop.is_cancelled() || server.is_cancelled() {
            return;
        }
        let at = super::clock_ms();
        let out = run(&t).await;
        if stop.is_cancelled() {
            return;
        }
        let was = {
            let mut s = lock(&state);
            let was = s.passed;
            s.passed = Some(out.passed);
            s.last_ms = Some(at);
            s.runs += 1;
            if !out.passed {
                s.failures += 1;
            }
            s.took_us = (out.total_ms * 1000.0) as u64;
            s.status = out.status;
            s.certificate_minutes = out.certificate_days.map(|d| (d * 1440.0).floor() as i64);
            s.attempts = out.attempts;
            s.failed.clone_from(&out.failures);
            was
        };
        write(&name, &machine, &t, &out, at);
        if was != Some(out.passed) && (was.is_some() || !out.passed) {
            event(&name, &machine, &t, &out);
        }
        // Kept to its schedule; a run asked for, or one past its time, puts the next an interval on.
        next += every;
        let n = Instant::now();
        if next < n {
            next = n + every;
        }
    }
}

/// A run's series, Datadog's names and units (ms; the certificate's expiry in minutes).
fn write(name: &str, machine: &str, t: &Test, out: &Outcome, at: i64) {
    let Some(o) = super::api::get() else { return };
    let subtype = t.check.subtype();
    let mut tags: Vec<(String, String)> = vec![("test".into(), name.into()), ("location".into(), machine.into()), ("subtype".into(), subtype.into())];
    tags.extend(t.tags.iter().filter_map(|x| x.split_once(':').map(|(k, v)| (k.to_owned(), v.to_owned()))));
    let pairs: Vec<(&str, &str)> = tags.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let mut series: Vec<(SeriesKey, Vec<(i64, f64)>)> = Vec::new();
    let mut put = |metric: &str, kind: Kind, v: Option<f64>, extra: Option<(&str, &str)>| {
        let Some(v) = v.filter(|v| v.is_finite()) else { return };
        let mut p = pairs.clone();
        p.extend(extra);
        if let Ok(k) = SeriesKey::new(metric, p, kind) {
            series.push((k, vec![(at, v)]));
        }
    };
    put("synthetics.test_runs", Kind::Count, Some(1.0), Some(("status", if out.passed { "passed" } else { "failed" })));
    put("synthetics.failing", Kind::Gauge, Some(if out.passed { 0.0 } else { 1.0 }), None);
    match subtype {
        "http" => {
            put("synthetics.http.response.time", Kind::Gauge, Some(out.total_ms), None);
            put("synthetics.http.dns.time", Kind::Gauge, out.dns_ms, None);
            put("synthetics.http.connect.time", Kind::Gauge, out.connect_ms, None);
            put("synthetics.http.ssl.time", Kind::Gauge, out.tls_ms, None);
            put("synthetics.http.firstbyte.time", Kind::Gauge, out.firstbyte_ms, None);
            put("synthetics.http.download.time", Kind::Gauge, out.download_ms, None);
            put("synthetics.http.redirect.time", Kind::Gauge, out.redirect_ms, None);
            put("synthetics.http.response.size", Kind::Gauge, out.size.map(|s| s as f64), None);
            if out.status.is_some() {
                put("synthetics.http.response", Kind::Count, Some(1.0), None);
            }
            put("synthetics.ssl.time_to_expiry", Kind::Gauge, out.certificate_days.map(|d| d * 1440.0), None);
        }
        "ssl" => {
            put("synthetics.ssl.response.time", Kind::Gauge, Some(out.total_ms), None);
            put("synthetics.ssl.dns.time", Kind::Gauge, out.dns_ms, None);
            put("synthetics.ssl.handshake.time", Kind::Gauge, out.tls_ms, None);
            put("synthetics.ssl.time_to_expiry", Kind::Gauge, out.certificate_days.map(|d| d * 1440.0), None);
        }
        "dns" => put("synthetics.dns.response.time", Kind::Gauge, out.dns_ms.or(Some(out.total_ms)), None),
        _ => {
            put("synthetics.tcp.response.time", Kind::Gauge, Some(out.total_ms), None);
            put("synthetics.tcp.dns.time", Kind::Gauge, out.dns_ms, None);
            put("synthetics.tcp.connect.time", Kind::Gauge, out.connect_ms, None);
        }
    }
    o.write(series);
}

/// A location that starts failing, or recovers: an event (`source:kiln.event
/// @event.type:synthetics`).
fn event(name: &str, machine: &str, t: &Test, out: &Outcome) {
    let Some(o) = super::api::get() else { return };
    let (severity, result, message) = if out.passed {
        (9, "passed", format!("synthetic test {name} passes again from {machine}"))
    } else {
        (17, "failed", format!("synthetic test {name} fails from {machine}: {}", out.failures.join("; ")))
    };
    o.log(vec![Record {
        ts: super::clock_ms().saturating_mul(1_000_000),
        severity,
        service: "kiln.synthetics".into(),
        host: machine.to_owned(),
        source: "kiln.event".into(),
        message,
        attrs: vec![("event.type".into(), "synthetics".into()), ("test".into(), name.into()), ("location".into(), machine.into()), ("subtype".into(), t.check.subtype().into()), ("result".into(), result.into())],
        trace_id: None,
        span_id: None,
    }]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http(assertions: Vec<Assertion>) -> anyhow::Result<Test> {
        Ok(serde_json::from_value(json!({ "type": "http", "url": "https://example.com/health", "assertions": assertions }))?)
    }

    fn a(kind: &str, op: &str, target: Value) -> Assertion {
        Assertion { kind: kind.into(), operator: op.into(), property: None, target }
    }

    #[test]
    fn assertions_judge_what_a_run_saw() -> anyhow::Result<()> {
        let out = Outcome { status: Some(503), total_ms: 120.0, certificate_days: Some(12.5), addresses: vec!["10.0.0.1".into(), "10.0.0.2".into()], ..Outcome::default() };
        let seen = Seen { headers: vec![("Content-Type".into(), "application/json".into())], body: Bytes::from_static(b"{\"status\":\"degraded\"}") };
        let t = http(vec![a("status_code", "is", json!(200)), a("response_time", "less_than", json!(500)), a("body", "contains", json!("\"degraded\"")), a("certificate_days", "more_than", json!(30))])?;
        let f = judge(&t, &out, &seen);
        anyhow::ensure!(f == vec!["status_code is 200: was 503".to_owned(), "certificate_days more than 30: was 12.5".to_owned()], "{f:?}");
        let mut header = a("header", "matches", json!("^application/(json|xml)$"));
        header.property = Some("content-type".into());
        anyhow::ensure!(judge(&http(vec![header])?, &out, &seen).is_empty(), "a header's name is matched without its case");
        // No assertions: an HTTP test passes under 400.
        anyhow::ensure!(judge(&http(vec![])?, &out, &seen) == vec!["status_code less than 400: was 503".to_owned()]);
        let dns: Test = serde_json::from_value(json!({ "type": "dns", "host": "example.com", "assertions": [{ "type": "address", "operator": "is", "target": "10.0.0.2" }] }))?;
        anyhow::ensure!(judge(&dns, &out, &Seen::default()).is_empty(), "any address resolved");
        Ok(())
    }

    #[test]
    fn tests_are_checked_and_shown_without_their_token() -> anyhow::Result<()> {
        let ok: Test = serde_json::from_value(json!({ "type": "http", "url": "https://example.com", "bearer": "s3cret", "alert": { "message": "@slack-ops", "min_locations": 2, "for_s": 300 } }))?;
        anyhow::ensure!(ok.check() == Ok(()) && ok.every_s == 60);
        anyhow::ensure!(!ok.redacted().to_string().contains("s3cret"));
        let m = ok.monitor("api").ok_or_else(|| anyhow::anyhow!("no monitor"))?;
        anyhow::ensure!(m.query == "min(last_5m):sum:synthetics.failing{test:api}.rollup(max, 60) >= 2", "{}", m.query);
        anyhow::ensure!(m.id == "syn-api" && m.message == "@slack-ops" && m.options.every == Some(60));
        anyhow::ensure!(m.compiled().is_ok(), "its monitor's query parses");
        for bad in [
            json!({ "type": "http", "url": "ftp://x" }),
            json!({ "type": "http", "url": "https://x", "headers": [["X-A", "a\r\nb"]] }),
            json!({ "type": "tcp", "host": "", "port": 22 }),
            json!({ "type": "http", "url": "https://x", "assertions": [{ "type": "address", "operator": "is", "target": "1.1.1.1" }] }),
            json!({ "type": "http", "url": "https://x", "assertions": [{ "type": "status_code", "operator": "contains", "target": 200 }] }),
            json!({ "type": "http", "url": "https://x", "assertions": [{ "type": "body", "operator": "matches", "target": "(" }] }),
            json!({ "type": "dns", "host": "x", "every_s": 5 }),
            json!({ "type": "dns", "host": "x", "timeout_s": 120 }),
        ] {
            let t: Test = serde_json::from_value(bad.clone())?;
            anyhow::ensure!(t.check().is_err(), "{bad}");
        }
        anyhow::ensure!(resolve("https://a.example/x/y?q", "/z") == "https://a.example/z");
        anyhow::ensure!(resolve("https://a.example/x/y", "z") == "https://a.example/x/z");
        anyhow::ensure!(resolve("https://a.example/x", "http://b.example/") == "http://b.example/");
        Ok(())
    }
}
