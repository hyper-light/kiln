//! Clouds' metrics and logs, pulled (`docs/observe.md` §10.1): Amazon CloudWatch, Google Cloud
//! Monitoring and Azure Monitor, each an integration with its credentials, read every `interval_s`
//! and written as series named as Datadog's integrations name them (`aws.ec2.cpuutilization`,
//! `gcp.compute.instance.cpu.utilization`, `azure.compute_virtualmachines.percentage_cpu`); and
//! the logs asked of each (CloudWatch Logs' log groups, Cloud Logging's entries, Log Analytics'
//! tables) as log records, each record once though every pull reads its lookback again.
//!
//! - Credentials are the integration's (keys, a service account's key, a client secret: sealed at
//!   rest under the server's master key, never shown again), or the server's own where it runs, as
//!   each cloud's SDKs find them: the environment, their files, a workload's identity, the
//!   instance's metadata service. Bare metal, a container, a VM or Kubernetes alike.
//! - A pull reads back `lookback_s` (three intervals unless said): a point that came late to the
//!   cloud, or a write refused while observe was busy, is read again. The store keeps a point once,
//!   and of two values at one time the later.
//! - A call throttled or failed on the service's side (429, 5xx, a throttling error) is tried again
//!   after its `Retry-After`, else 1 s doubling; a pull that failed is tried again after 1 min
//!   doubling, up to its interval.
//! - What each integration did is its status (the API's), kiln's own series (`kiln.cloud.points`,
//!   `.calls`, `.errors`, `.series`, `.pull.duration`), and an event when it first pulls, starts
//!   failing or recovers.

pub mod aws;
pub mod azure;
pub mod eventhubs;
pub mod gcp;
pub mod push;
pub mod sigv4;

use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash as _, Hasher as _};
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use base64::Engine as _;
use bytes::Bytes;
use kiln_obs::batch::{Kind, MAX_TAGS, SeriesKey};
use kiln_obs::logs::record::Record;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::http::client::{Client, Method};

/// Calls tried at the most, and the first wait between them (doubling) unless the service says.
const TRIES: u32 = 4;
const BACKOFF: Duration = Duration::from_secs(1);
/// The longest wait a service's `Retry-After` is given, and a failed pull's.
const RETRY_MAX: Duration = Duration::from_secs(60);
/// A response's bytes at the most.
pub const BODY_MAX: usize = 64 << 20;
/// What a cloud's API is given to answer; and a metadata service, which is local or not there.
pub const TIMEOUT: Duration = Duration::from_secs(30);
pub const METADATA_TIMEOUT: Duration = Duration::from_secs(2);
/// How often a cloud's list of what there is (metrics, metric types, resources) is read again.
pub const DISCOVER_EVERY: Duration = Duration::from_secs(15 * 60);
/// Points a write to the store holds at the most: a big pull is taken in parts.
const WRITE_POINTS: usize = 100_000;
/// How long a pull's write waits for observe while it is busy, before leaving it to the next pull.
const BUSY_WAIT: Duration = Duration::from_secs(10);
/// Credentials are got again this long before they expire (AWS's SDKs: 5 min).
pub const REFRESH_MS: i64 = 5 * 60_000;
/// What a sealed secret starts with, at rest.
const SEALED: &str = "sealed:v1:";

// ------------------------------------------------------------------------------------- settings

fn interval() -> u64 {
    300
}

fn max_series() -> usize {
    20_000
}

/// What every cloud integration says besides its cloud's settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Common {
    /// Seconds between pulls (at least 10).
    #[serde(default = "interval")]
    pub interval_s: u64,
    /// Seconds a pull reads back; three intervals unless said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lookback_s: Option<u64>,
    /// Tags every series gets (`key:value`, or `key`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Series written at the most; past it the rest are not asked for, and the status says so.
    #[serde(default = "max_series")]
    pub max_series: usize,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub paused: bool,
}

impl Default for Common {
    fn default() -> Self {
        Self { interval_s: interval(), lookback_s: None, tags: Vec::new(), max_series: max_series(), paused: false }
    }
}

impl Common {
    pub fn lookback_ms(&self) -> i64 {
        i64::try_from(self.lookback_s.unwrap_or(self.interval_s.saturating_mul(3)).saturating_mul(1000)).unwrap_or(i64::MAX)
    }

    /// The tags as pairs.
    pub fn tag_pairs(&self) -> Vec<(String, String)> {
        self.tags.iter().map(|t| t.split_once(':').map_or((t.clone(), String::new()), |(k, v)| (k.to_owned(), v.to_owned()))).collect()
    }

    fn check(&self) -> Result<(), String> {
        if !(10..=86_400).contains(&self.interval_s) {
            return Err(format!("interval_s {}: from 10 to 86400", self.interval_s));
        }
        if let Some(l) = self.lookback_s
            && !(self.interval_s..=14 * 86_400).contains(&l)
        {
            return Err(format!("lookback_s {l}: from interval_s to 14 days"));
        }
        if self.max_series == 0 || self.max_series > 1_000_000 {
            return Err(format!("max_series {}: from 1 to 1000000", self.max_series));
        }
        if let Some(t) = self.tags.iter().find(|t| t.is_empty() || t.starts_with(':') || t.len() > 200) {
            return Err(format!("tag {t:?}: key:value, its key not empty"));
        }
        Ok(())
    }
}

/// A cloud integration's settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Cloud {
    Aws(aws::Config),
    Gcp(gcp::Config),
    Azure(azure::Config),
}

impl Cloud {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Aws(_) => "aws",
            Self::Gcp(_) => "gcp",
            Self::Azure(_) => "azure",
        }
    }

    pub fn common(&self) -> &Common {
        match self {
            Self::Aws(c) => &c.common,
            Self::Gcp(c) => &c.common,
            Self::Azure(c) => &c.common,
        }
    }

    /// Its secrets, by name: what is sealed at rest and never shown.
    fn secrets_mut(&mut self) -> Vec<(&'static str, &mut Option<String>)> {
        match self {
            Self::Aws(c) => vec![("secret_access_key", &mut c.secret_access_key), ("session_token", &mut c.session_token)],
            Self::Gcp(c) => vec![("service_account", &mut c.service_account)],
            Self::Azure(c) => {
                let mut v = vec![("client_secret", &mut c.client_secret)];
                if let Some(h) = c.event_hubs.as_mut() {
                    v.push(("event_hubs.connection_string", &mut h.connection_string));
                }
                v
            }
        }
    }

    pub fn check(&self) -> Result<(), String> {
        self.common().check()?;
        match self {
            Self::Aws(c) => c.check(),
            Self::Gcp(c) => c.check(),
            Self::Azure(c) => c.check(),
        }
    }

    /// As the API shows it: each secret it holds `"…"`.
    pub fn redacted(&self) -> Value {
        let mut c = self.clone();
        for (_, s) in c.secrets_mut() {
            if s.is_some() {
                *s = Some(HIDDEN.to_owned());
            }
        }
        serde_json::to_value(&c).unwrap_or(Value::Null)
    }

    /// Secrets left out (or `"…"`, as shown) are `old`'s, when it is of the same cloud: a change of
    /// settings need not give them again. An empty one is none: a secret removed (the server's own
    /// credentials from now).
    fn keep_secrets(&mut self, old: Option<&Self>) {
        let mut old = old.filter(|o| o.kind() == self.kind()).cloned();
        let kept: HashMap<&'static str, Option<String>> = old.as_mut().map(|o| o.secrets_mut().into_iter().map(|(k, v)| (k, v.clone())).collect()).unwrap_or_default();
        for (k, v) in self.secrets_mut() {
            match v.as_deref() {
                None | Some(HIDDEN) => *v = kept.get(k).cloned().flatten(),
                Some("") => *v = None,
                Some(_) => {}
            }
        }
    }
}

/// A secret as the API shows it.
pub const HIDDEN: &str = "…";

// --------------------------------------------------------------------------------------- names

/// A cloud's name as Datadog's integrations write it: a capital after a lower-case letter starts a
/// word (`NetworkIn` → `network_in`, while `CPUUtilization` → `cpuutilization` and
/// `HTTPCode_ELB_5XX` → `httpcode_elb_5xx`), in lower case, each run of other characters than
/// ASCII letters and digits one `_`, none at the ends.
pub fn snake(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    let (mut lower, mut gap) = (false, false);
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            if !out.is_empty() && (gap || (lower && c.is_ascii_uppercase())) {
                out.push('_');
            }
            gap = false;
            lower = c.is_ascii_lowercase();
            out.push(c.to_ascii_lowercase());
        } else {
            gap = true;
            lower = false;
        }
    }
    out
}

/// A name in lower case, each run of other characters than ASCII letters and digits one `_`
/// (AWS's namespaces: `ApplicationELB` → `applicationelb`, Datadog's).
pub fn lower(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut gap = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            if gap && !out.is_empty() {
                out.push('_');
            }
            gap = false;
            out.push(c.to_ascii_lowercase());
        } else {
            gap = true;
        }
    }
    out
}

/// A series' key: a tag whose key came before is left out (the first wins: the cloud's own tags,
/// then the integration's), as are empty keys and those past [`MAX_TAGS`].
pub fn key(metric: &str, tags: &[(&str, &str)], kind: Kind) -> Option<SeriesKey> {
    let mut kept: Vec<(&str, &str)> = Vec::with_capacity(tags.len().min(MAX_TAGS));
    for &(k, v) in tags {
        if k.is_empty() || kept.iter().any(|(s, _)| *s == k) {
            continue;
        }
        if kept.len() == MAX_TAGS {
            break;
        }
        kept.push((k, v));
    }
    SeriesKey::new(metric, kept, kind).ok()
}

// ------------------------------------------------------------------------------------ encoding

/// RFC 3986's unreserved characters as they are, every other byte `%XX`: a URL's query value, a
/// form's.
pub fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(char::from(b)),
            _ => {
                const HEX: &[u8; 16] = b"0123456789ABCDEF";
                out.push('%');
                out.push(char::from(HEX.get(usize::from(b >> 4)).copied().unwrap_or(b'0')));
                out.push(char::from(HEX.get(usize::from(b & 15)).copied().unwrap_or(b'0')));
            }
        }
    }
    out
}

/// `k=v&k=v`, each encoded: a query string, or an `application/x-www-form-urlencoded` body.
pub fn form(pairs: &[(&str, &str)]) -> String {
    pairs.iter().map(|(k, v)| format!("{}={}", encode(k), encode(v))).collect::<Vec<_>>().join("&")
}

/// Unix ms as RFC 3339 (`2026-10-18T12:00:00Z`).
pub fn rfc3339(ms: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000).ok().and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok()).unwrap_or_default()
}

/// An RFC 3339 time in unix ms.
pub fn parse_time(s: &str) -> Option<i64> {
    let t = time::OffsetDateTime::parse(s.trim(), &time::format_description::well_known::Rfc3339).ok()?;
    i64::try_from(t.unix_timestamp_nanos() / 1_000_000).ok()
}

/// An RFC 3339 time in unix ns (a log record's).
pub fn parse_time_ns(s: &str) -> Option<i64> {
    let t = time::OffsetDateTime::parse(s.trim(), &time::format_description::well_known::Rfc3339).ok()?;
    i64::try_from(t.unix_timestamp_nanos()).ok()
}

/// A URL's authority (`host[:port]`) and path, as a request to it sends and signs them.
pub fn authority_and_path(url: &str) -> anyhow::Result<(String, String)> {
    let uri: http::Uri = url.parse().map_err(|e| anyhow::anyhow!("{url}: {e}"))?;
    let host = uri.authority().map(|a| a.as_str().to_owned()).ok_or_else(|| anyhow::anyhow!("{url} names no host"))?;
    Ok((host, uri.path().to_owned()))
}

/// A number JSON gives as one or as text (Google's int64s, Azure's `expires_in`).
pub fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// `$HOME` (`%USERPROFILE%` on Windows).
pub fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).filter(|h| !h.is_empty()).map(PathBuf::from)
}

/// An environment variable, unless unset or empty.
pub fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

// ----------------------------------------------------------------------------------------- api

/// A cloud's answer: its status and body, and what it said of waiting (`Retry-After`) and of its
/// error (AWS's `x-amzn-query-error`, `x-amzn-errortype`).
pub struct Answer {
    pub status: u16,
    pub body: Bytes,
    pub retry_after: Option<Duration>,
    pub error_type: Option<String>,
}

impl Answer {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The body as JSON.
    pub fn json<T: serde::de::DeserializeOwned>(&self) -> anyhow::Result<T> {
        serde_json::from_slice(&self.body).map_err(|e| anyhow::anyhow!("an answer that is not what was asked: {e}"))
    }

    /// The body's start, as text: what an error says.
    pub fn text(&self) -> String {
        crate::util::truncate(&String::from_utf8_lossy(&self.body), 500).trim().to_owned()
    }
}

/// A request to a cloud: its method, URL, headers and body.
pub struct Call<'a> {
    pub method: Method,
    pub url: &'a str,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
    pub timeout: Duration,
}

impl<'a> Call<'a> {
    pub fn get(url: &'a str) -> Self {
        Self { method: Method::Get, url, headers: Vec::new(), body: Bytes::new(), timeout: TIMEOUT }
    }

    pub fn post(url: &'a str, content_type: &str, body: impl Into<Bytes>) -> Self {
        Self { method: Method::Post, url, headers: vec![("content-type".into(), content_type.into())], body: body.into(), timeout: TIMEOUT }
    }

    pub fn put(url: &'a str, content_type: &str, body: impl Into<Bytes>) -> Self {
        Self { method: Method::Put, url, headers: vec![("content-type".into(), content_type.into())], body: body.into(), timeout: TIMEOUT }
    }

    pub fn header(mut self, k: &str, v: &str) -> Self {
        self.headers.push((k.to_owned(), v.to_owned()));
        self
    }

    pub fn bearer(self, token: &str) -> Self {
        let v = format!("Bearer {token}");
        self.header("authorization", &v)
    }

    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = d;
        self
    }
}

/// A pull's calls of a cloud's API, counted.
pub struct Api<'a> {
    http: &'a Client,
    calls: AtomicU64,
}

impl<'a> Api<'a> {
    pub fn new(http: &'a Client) -> Self {
        Self { http, calls: AtomicU64::new(0) }
    }

    pub fn calls(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }

    /// The call, tried again (up to [`TRIES`] times) when it failed to arrive, was throttled (429,
    /// or what `throttled` says of an answer) or failed on the service's side (5xx): its answer,
    /// of whatever status.
    pub async fn call(&self, c: &Call<'_>, throttled: fn(&Answer) -> bool) -> anyhow::Result<Answer> {
        self.tries(c, throttled, TRIES).await
    }

    /// The call once: a metadata service, which answers at once or is not there.
    pub async fn once(&self, c: &Call<'_>) -> anyhow::Result<Answer> {
        self.tries(c, |_| false, 1).await
    }

    async fn tries(&self, c: &Call<'_>, throttled: fn(&Answer) -> bool, tries: u32) -> anyhow::Result<Answer> {
        let mut wait = BACKOFF;
        let mut attempt = 0;
        loop {
            attempt += 1;
            self.calls.fetch_add(1, Ordering::Relaxed);
            let mut req = self.http.request(c.method, c.url).timeout(c.timeout);
            for (k, v) in &c.headers {
                req = req.header(k, v);
            }
            if !c.body.is_empty() || matches!(c.method, Method::Post | Method::Put) {
                req = req.body(c.body.clone());
            }
            let pause = match send(req, c.timeout).await {
                Ok(a) => {
                    let again = a.status == 429 || a.status >= 500 || (a.status >= 400 && throttled(&a));
                    if !again || attempt >= tries {
                        return Ok(a);
                    }
                    a.retry_after.map_or(wait, |r| r.min(RETRY_MAX))
                }
                Err(e) if attempt >= tries => return Err(e),
                Err(_) => wait,
            };
            crate::rt::sleep(pause).await;
            wait = wait.saturating_mul(2).min(RETRY_MAX);
        }
    }
}

/// The request sent and its answer read, its body within `within` too (the request's own time is
/// to its answer's head): a service that answers and stalls fails the call, to be tried again.
async fn send(req: crate::http::client::RequestBuilder<'_>, within: Duration) -> anyhow::Result<Answer> {
    let r = req.send().await?;
    let status = r.status();
    let retry_after = r.header("retry-after").and_then(|v| v.trim().parse::<u64>().ok()).map(Duration::from_secs);
    let error_type = r.header("x-amzn-query-error").or_else(|| r.header("x-amzn-errortype")).map(str::to_owned);
    let body = crate::rt::timeout(within, r.bytes(BODY_MAX)).await.map_err(|_| anyhow::anyhow!("its answer's body did not come within {within:?}"))??;
    Ok(Answer { status, body, retry_after, error_type })
}

// ---------------------------------------------------------------------------------------- pulls

/// What a pull brought.
#[derive(Debug, Default)]
pub struct Pulled {
    pub series: Vec<(SeriesKey, Vec<(i64, f64)>)>,
    /// Log records with their ids ([`id_of`]): what the pull read of its lookback, records taken by
    /// a pull before among them ([`Seen`] leaves those out).
    pub logs: Vec<(u64, Record)>,
    /// Series asked for each pull, and those past `max_series`, not asked for.
    pub known: usize,
    pub over: usize,
    /// What went wrong without failing the pull (a region refused, a resource gone), for the status.
    pub warnings: Vec<String>,
}

impl Pulled {
    pub fn points(&self) -> u64 {
        self.series.iter().map(|(_, p)| p.len() as u64).sum()
    }
}

/// A cloud's puller: what it found and the credentials it holds, kept from pull to pull.
pub enum Puller {
    Aws(Box<aws::Puller>),
    Gcp(Box<gcp::Puller>),
    Azure(Box<azure::Puller>),
}

impl Puller {
    pub fn new(c: &Cloud) -> Self {
        match c {
            Cloud::Aws(c) => Self::Aws(Box::new(aws::Puller::new(c.clone()))),
            Cloud::Gcp(c) => Self::Gcp(Box::new(gcp::Puller::new(c.clone()))),
            Cloud::Azure(c) => Self::Azure(Box::new(azure::Puller::new(c.clone()))),
        }
    }

    /// One pull, at `now` (unix ms).
    pub async fn pull(&mut self, api: &Api<'_>, now: i64) -> anyhow::Result<Pulled> {
        match self {
            Self::Aws(p) => p.pull(api, now).await,
            Self::Gcp(p) => p.pull(api, now).await,
            Self::Azure(p) => p.pull(api, now).await,
        }
    }
}

/// What an integration is doing, as the API shows it.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Status {
    /// `starting`, `ok`, `failing` or `paused`.
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// Unix ms.
    pub last_pull: Option<i64>,
    pub last_ok: Option<i64>,
    pub next_pull: Option<i64>,
    /// Series asked for each pull; those past `max_series`, not asked for.
    pub series: usize,
    pub over: usize,
    /// The last pull's points written, points left to the next pull (observe was busy), calls made
    /// and time taken.
    pub points: u64,
    pub deferred: u64,
    pub calls: u64,
    pub took_ms: u64,
    /// Pulls and failed pulls since the server started.
    pub pulls: u64,
    pub failures: u64,
    /// The last pull's log records written (each once), and those left to the next pull to read
    /// again (observe was busy); and the records written since the server started.
    pub records: u64,
    pub records_deferred: u64,
    pub records_total: u64,
    /// What is read as it comes (an Azure integration's event hubs), each stream's.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub streams: Vec<Stream>,
}

/// A stream's reading (an event hub's), as the API shows it.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Stream {
    pub hub: String,
    /// `starting`, `reading` or `failing`.
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    pub partitions: usize,
    /// Events not yet read, every partition's together.
    pub lag: i64,
    /// Events read, log records written of them, and metric records passed over, since the server
    /// started.
    pub events: u64,
    pub records: u64,
    pub skipped: u64,
    /// Unix ms of the last record written.
    pub last_ms: Option<i64>,
}

// ----------------------------------------------------------------------------------------- logs

/// Log records' ids a pull loop keeps at the most ([`Seen`]): a lookback's records at a thousand a
/// second (15 minutes': 900,000), some 20 MB. Past it the oldest half is forgotten, and a record of
/// theirs read again would be taken again (the status counts them).
pub const SEEN_MAX: usize = 1 << 20;
/// Log records a write to the store holds at the most: a big pull is taken in parts.
const WRITE_RECORDS: usize = 10_000;

/// A record's id: the cloud's own and what it is unique within, hashed (64 bits: among a million
/// ids, a collision once in some 30 million lookbacks).
pub fn id_of(parts: &[&str]) -> u64 {
    let mut h = std::hash::DefaultHasher::new();
    for p in parts {
        p.hash(&mut h);
    }
    h.finish()
}

/// The ids of the records a pull loop took, by their times: a pull reads its lookback again (a
/// cloud takes records late) and takes each record once.
#[derive(Debug, Default)]
pub struct Seen {
    at: HashMap<u64, i64>,
    /// Ids forgotten past [`SEEN_MAX`] before their time.
    pub forgotten: u64,
}

impl Seen {
    pub fn has(&self, id: u64) -> bool {
        self.at.contains_key(&id)
    }

    /// Keeps `id`, its record's time `ms`.
    pub fn add(&mut self, id: u64, ms: i64) {
        self.at.insert(id, ms);
    }

    /// Forgets the ids of records before `from` (unix ms), none of which a pull reads again; past
    /// [`SEEN_MAX`], the older half of the rest.
    pub fn prune(&mut self, from: i64) {
        self.at.retain(|_, t| *t >= from);
        if self.at.len() > SEEN_MAX {
            let mut times: Vec<i64> = self.at.values().copied().collect();
            let mid = times.len() / 2;
            let cut = *times.select_nth_unstable(mid).1;
            let before = self.at.len();
            self.at.retain(|_, t| *t >= cut);
            self.forgotten += (before - self.at.len()) as u64;
        }
    }

    pub fn len(&self) -> usize {
        self.at.len()
    }

    pub fn is_empty(&self) -> bool {
        self.at.is_empty()
    }
}

/// A record's level from its message, as a log pipeline's status remapper reads it: a JSON object's
/// `level`, `severity`, `levelname`, `log.level` or `status`; else a level word among its first
/// four words (`ERROR`, `[WARN]`, `level=info`, Lambda's `\tERROR\t`); else info.
pub fn severity_of(message: &str) -> u8 {
    let m = message.trim_start();
    if m.starts_with('{')
        && let Ok(Value::Object(o)) = serde_json::from_str::<Value>(m)
        && let Some(l) = LEVEL_FIELDS.iter().find_map(|k| o.get(*k).and_then(Value::as_str))
        && let Some(r) = kiln_obs::logs::record::severities(l)
    {
        return *r.start();
    }
    severity_of_words(m)
}

/// The fields a JSON message's level is read from by [`severity_of`]: the first of them that is a
/// string.
pub const LEVEL_FIELDS: [&str; 5] = ["level", "severity", "levelname", "log.level", "status"];

/// [`severity_of`] of a message's first words alone: a level in capitals, or a `level=` one.
pub fn severity_of_words(m: &str) -> u8 {
    let m = m.trim_start();
    for word in m.split(|c: char| c.is_whitespace() || c == '|').filter(|w| !w.is_empty()).take(4) {
        // A word without `=` is a level only in capitals: one whose first letter or digit is not
        // one is passed at once (most words: an address, a date, a path).
        if !word.as_bytes().contains(&b'=') && !word.bytes().find(u8::is_ascii_alphanumeric).is_some_and(|b| b.is_ascii_uppercase()) {
            continue;
        }
        let w = word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '=');
        let w = w.rsplit_once('=').map_or(w, |(k, v)| if k.eq_ignore_ascii_case("level") || k.eq_ignore_ascii_case("severity") { v } else { "" });
        // Words in capitals, or a level=…'s value: "Error" in a sentence is not a level.
        if ((w.len() >= 4 && w.bytes().all(|b| b.is_ascii_uppercase())) || word.contains('='))
            && let Some(r) = kiln_obs::logs::record::severities(w)
        {
            return *r.start();
        }
    }
    9
}

/// A message no longer than a record holds, cut at a character.
pub fn message(mut m: String) -> String {
    let max = kiln_obs::logs::record::MAX_MESSAGE_BYTES;
    if m.len() > max {
        let mut c = max;
        while !m.is_char_boundary(c) {
            c -= 1;
        }
        m.truncate(c);
    }
    m
}

// --------------------------------------------------------------------------------------- store

/// The server's cloud integrations by name, kept in `<observe>/clouds.json` (their secrets sealed
/// under the master key, the file the owner's alone), each pulled by its own task.
pub struct Clouds {
    path: PathBuf,
    master: PathBuf,
    http: Arc<Client>,
    all: Mutex<BTreeMap<String, Cloud>>,
    running: Mutex<HashMap<String, Running>>,
    /// Each integration's streams' offsets, kept from one of its runs to the next.
    kept: Mutex<HashMap<String, Arc<eventhubs::Kept>>>,
    stop: crate::rt::Shutdown,
}

struct Running {
    stop: crate::rt::Shutdown,
    now: Arc<crate::rt::Notify>,
    status: Arc<Mutex<Status>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What binds a sealed secret to its integration and field.
fn aad(name: &str, field: &str) -> Vec<u8> {
    format!("kiln observe cloud\0{name}\0{field}").into_bytes()
}

impl Clouds {
    /// The integrations kept in `dir`, their secrets opened with the master key at `master`; none
    /// pulled until [`Clouds::start`]. A secret that does not open leaves its integration failing,
    /// said, until it is given again.
    pub fn open(dir: &Path, master: &Path, http: Arc<Client>, stop: crate::rt::Shutdown) -> Arc<Self> {
        let path = dir.join("clouds.json");
        let mut all: BTreeMap<String, Cloud> = BTreeMap::new();
        let mut broken: Vec<(String, String)> = Vec::new();
        if let Ok(b) = std::fs::read(&path) {
            match serde_json::from_slice::<BTreeMap<String, Cloud>>(&b) {
                Ok(read) => {
                    for (name, mut c) in read {
                        for (field, v) in c.secrets_mut() {
                            if let Some(s) = v.as_deref().and_then(|s| s.strip_prefix(SEALED)) {
                                match unseal(master, &name, field, s) {
                                    Ok(plain) => *v = Some(plain),
                                    Err(e) => {
                                        *v = None;
                                        broken.push((name.clone(), format!("its {field} does not open ({e:#}): give it again")));
                                    }
                                }
                            }
                        }
                        all.insert(name, c);
                    }
                }
                Err(e) => tracing::error!(path = %path.display(), error = %e, "observe's cloud integrations could not be read"),
            }
        }
        let clouds = Arc::new(Self { path, master: master.to_owned(), http, all: Mutex::new(all), running: Mutex::new(HashMap::new()), kept: Mutex::new(HashMap::new()), stop });
        for (name, why) in broken {
            let status = Status { state: "failing", error: Some(why), ..Status::default() };
            lock(&clouds.running).insert(name, Running { stop: crate::rt::Shutdown::new(), now: Arc::new(crate::rt::Notify::new()), status: Arc::new(Mutex::new(status)) });
        }
        clouds
    }

    /// Pulls every integration whose secrets opened.
    pub fn start(self: &Arc<Self>) {
        let all: Vec<(String, Cloud)> = lock(&self.all).iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        for (name, c) in all {
            if lock(&self.running).get(&name).is_some_and(|r| r.stop.is_cancelled() || lock(&r.status).state == "failing") {
                continue;
            }
            self.run(&name, c);
        }
    }

    pub fn get(&self, name: &str) -> Option<Cloud> {
        lock(&self.all).get(name).cloned()
    }

    /// Each integration as the API shows it: its settings (secrets hidden) and status.
    pub fn list(&self) -> Vec<Value> {
        let all = lock(&self.all);
        let running = lock(&self.running);
        all.iter()
            .map(|(name, c)| {
                let mut v = c.redacted();
                if let Value::Object(o) = &mut v {
                    o.insert("name".into(), Value::from(name.clone()));
                    let status = running.get(name).map(|r| lock(&r.status).clone()).unwrap_or_default();
                    o.insert("status".into(), serde_json::to_value(status).unwrap_or(Value::Null));
                }
                v
            })
            .collect()
    }

    /// Keeps `c` as `name` (secrets left out are the old ones) and pulls it from now.
    pub fn put(self: &Arc<Self>, name: &str, mut c: Cloud) -> Result<(), String> {
        if name.is_empty() || name.len() > 64 || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)) {
            return Err("an integration's name is 1 to 64 letters, digits, '-', '_' and '.'".into());
        }
        c.keep_secrets(self.get(name).as_ref());
        c.check()?;
        {
            let mut all = lock(&self.all);
            all.insert(name.to_owned(), c.clone());
            self.save(&all).map_err(|e| format!("{e:#}"))?;
        }
        self.run(name, c);
        Ok(())
    }

    pub fn delete(&self, name: &str) -> anyhow::Result<bool> {
        let had = {
            let mut all = lock(&self.all);
            let had = all.remove(name).is_some();
            self.save(&all)?;
            had
        };
        if let Some(r) = lock(&self.running).remove(name) {
            r.stop.cancel();
        }
        // Its streams' offsets go with it: one made again reads from its lookback.
        match lock(&self.kept).remove(name) {
            Some(k) => k.forget(),
            None => {
                let _ = std::fs::remove_file(self.offsets(name));
            }
        }
        Ok(had)
    }

    /// Where an integration's streams keep their offsets.
    fn offsets(&self, name: &str) -> PathBuf {
        self.path.with_file_name(format!("eventhubs-{name}.json"))
    }

    /// Pulls `name` now, not at its time.
    pub fn pull_now(&self, name: &str) -> bool {
        lock(&self.running).get(name).map(|r| r.now.notify()).is_some()
    }

    /// `c` (secrets left out are `name`'s) pulled once, nothing written: what it would read.
    pub async fn test(&self, name: &str, mut c: Cloud) -> Value {
        c.keep_secrets(self.get(name).as_ref());
        if let Err(e) = c.check() {
            return json!({ "ok": false, "error": e });
        }
        let api = Api::new(&self.http);
        let t0 = Instant::now();
        let r = Puller::new(&c).pull(&api, super::clock_ms()).await;
        let took = u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX);
        match r {
            Ok(p) => {
                let mut names: Vec<String> = p.series.iter().filter_map(|(k, _)| kiln_obs::batch::parse_key(&k.key).ok().map(|k| k.metric.to_owned())).collect();
                names.sort_unstable();
                names.dedup();
                // The log records the lookback holds (each once), and a few of them, the newest.
                let mut ids: Vec<u64> = p.logs.iter().map(|(id, _)| *id).collect();
                ids.sort_unstable();
                ids.dedup();
                let mut newest: Vec<&Record> = p.logs.iter().map(|(_, r)| r).collect();
                newest.sort_by_key(|r| std::cmp::Reverse(r.ts));
                let said: Vec<String> = newest.iter().take(5).map(|r| format!("{} {}: {}", r.source, r.service, r.message.chars().take(160).collect::<String>())).collect();
                json!({ "ok": true, "series": p.known, "over": p.over, "points": p.points(), "records": ids.len(), "records_sample": said, "calls": api.calls(), "took_ms": took, "metrics": names.len(), "sample": names.iter().take(50).collect::<Vec<_>>(), "warnings": p.warnings })
            }
            Err(e) => json!({ "ok": false, "error": format!("{e:#}"), "calls": api.calls(), "took_ms": took }),
        }
    }

    fn save(&self, all: &BTreeMap<String, Cloud>) -> anyhow::Result<()> {
        let mut sealed = all.clone();
        for (name, c) in &mut sealed {
            for (field, v) in c.secrets_mut() {
                if let Some(plain) = v.as_deref() {
                    let (ct, nonce) = crate::ci::secrets::seal_with(&self.master, plain.as_bytes(), &aad(name, field))?;
                    let b64 = base64::engine::general_purpose::STANDARD_NO_PAD;
                    *v = Some(format!("{SEALED}{}:{}", b64.encode(nonce), b64.encode(ct)));
                }
            }
        }
        crate::util::write_secret(&self.path, &serde_json::to_vec_pretty(&sealed)?, 0o600)?;
        Ok(())
    }

    /// Starts `name`'s task (the one before it stopped).
    fn run(self: &Arc<Self>, name: &str, c: Cloud) {
        let running = Running { stop: crate::rt::Shutdown::new(), now: Arc::new(crate::rt::Notify::new()), status: Arc::new(Mutex::new(Status { state: "starting", ..Status::default() })) };
        let (stop, now, status) = (running.stop.clone(), running.now.clone(), running.status.clone());
        if let Some(old) = lock(&self.running).insert(name.to_owned(), running) {
            old.stop.cancel();
        }
        let (http, name, server) = (self.http.clone(), name.to_owned(), self.stop.clone());
        // An Azure integration's event hubs: a stream each, beside its pulls.
        if let Cloud::Azure(a) = &c
            && let Some(hubs) = &a.event_hubs
            && !a.common.paused
        {
            let kept = lock(&self.kept).entry(name.clone()).or_insert_with(|| eventhubs::Kept::open(self.offsets(&name))).clone();
            for hub in &hubs.hubs {
                let reader = eventhubs::Reader { name: name.clone(), azure: a.clone(), hub: hub.clone(), kept: kept.clone(), http: http.clone(), status: status.clone() };
                let (stop, server) = (stop.clone(), server.clone());
                crate::rt::spawn(async move { eventhubs::run(reader, stop, server).await });
            }
        }
        crate::rt::spawn(async move { pull_loop(http, name, c, stop, server, now, status).await });
    }
}

fn unseal(master: &Path, name: &str, field: &str, s: &str) -> anyhow::Result<String> {
    let b64 = base64::engine::general_purpose::STANDARD_NO_PAD;
    let (nonce, ct) = s.split_once(':').ok_or_else(|| anyhow::anyhow!("not a sealed secret"))?;
    let plain = crate::ci::secrets::open_with(master, &b64.decode(ct)?, &b64.decode(nonce)?, &aad(name, field))?;
    Ok(String::from_utf8(plain)?)
}

/// An integration's task: a pull, then the next at its time (sooner when asked), until it is
/// changed, deleted or the server stops.
async fn pull_loop(http: Arc<Client>, name: String, c: Cloud, stop: crate::rt::Shutdown, server: crate::rt::Shutdown, now: Arc<crate::rt::Notify>, status: Arc<Mutex<Status>>) {
    let common = c.common().clone();
    let kind = c.kind();
    if common.paused {
        lock(&status).state = "paused";
        return;
    }
    let interval = Duration::from_secs(common.interval_s);
    let mut puller = Puller::new(&c);
    let mut seen = Seen::default();
    let mut failed = 0u32;
    let mut pulled_once = false;
    loop {
        if stop.is_cancelled() || server.is_cancelled() {
            return;
        }
        let t0 = Instant::now();
        let at = super::clock_ms();
        let api = Api::new(&http);
        let r = puller.pull(&api, at).await;
        if stop.is_cancelled() {
            return;
        }
        let took = t0.elapsed();
        let took_ms = u64::try_from(took.as_millis()).unwrap_or(u64::MAX);
        let calls = api.calls();
        let wait = match r {
            Ok(p) => {
                let (known, over, mut warnings) = (p.known, p.over, p.warnings);
                let (written, deferred) = write(p.series).await;
                let forgotten = seen.forgotten;
                let (records, records_deferred) = write_logs(p.logs, &mut seen).await;
                // What the next pull reads again is in its lookback: older ids are of no use.
                seen.prune(at.saturating_sub(common.lookback_ms()));
                if seen.forgotten > forgotten {
                    warnings.push(format!("{} log records' ids were forgotten past {SEEN_MAX} (a record of theirs read again is taken again): a shorter lookback_s keeps fewer", seen.forgotten - forgotten));
                }
                let was_failing = failed > 0;
                failed = 0;
                {
                    let mut s = lock(&status);
                    s.state = "ok";
                    s.error = None;
                    s.warnings = warnings;
                    s.last_pull = Some(at);
                    s.last_ok = Some(at);
                    s.series = known;
                    s.over = over;
                    s.points = written;
                    s.deferred = deferred;
                    s.calls = calls;
                    s.took_ms = took_ms;
                    s.pulls += 1;
                    s.records = records;
                    s.records_deferred = records_deferred;
                    s.records_total += records;
                }
                if !pulled_once || was_failing {
                    let what = if was_failing { "recovered" } else { "pulling" };
                    let over = if over > 0 { format!(" ({over} more past max_series, not asked for)") } else { String::new() };
                    let logs = if records > 0 { format!(" and {records} log records") } else { String::new() };
                    event(&name, kind, 9, what, format!("{kind} integration {name} {what}: {known} series{over}, {written} points{logs} in {took_ms} ms"));
                }
                pulled_once = true;
                record(&name, kind, written, records, calls, 0, known, took);
                interval.saturating_sub(took)
            }
            Err(e) => {
                let why = format!("{e:#}");
                failed += 1;
                {
                    let mut s = lock(&status);
                    s.state = "failing";
                    s.error = Some(why.clone());
                    s.last_pull = Some(at);
                    s.calls = calls;
                    s.took_ms = took_ms;
                    s.pulls += 1;
                    s.failures += 1;
                }
                if failed == 1 {
                    tracing::warn!(integration = %name, cloud = kind, error = %why, "a cloud integration's pull failed");
                    event(&name, kind, 17, "failing", format!("{kind} integration {name} is failing: {why}"));
                }
                record(&name, kind, 0, 0, calls, 1, 0, took);
                RETRY_MAX.saturating_mul(1 << (failed - 1).min(10)).min(interval)
            }
        };
        lock(&status).next_pull = Some(super::clock_ms().saturating_add(i64::try_from(wait.as_millis()).unwrap_or(i64::MAX)));
        let slept = pin!(stop.sleep(wait));
        let asked = pin!(now.notified());
        let ended = pin!(server.cancelled());
        match futures::future::select(futures::future::select(slept, asked), ended).await {
            futures::future::Either::Left((futures::future::Either::Left((false, _)), _)) | futures::future::Either::Right(_) => return,
            _ => {}
        }
    }
}

/// Writes a pull's points in parts, each when observe has room for it (waiting a while if it is
/// busy): the points written, and those left for the next pull to read again.
async fn write(series: Vec<(SeriesKey, Vec<(i64, f64)>)>) -> (u64, u64) {
    let Some(o) = super::api::get() else {
        return (0, series.iter().map(|(_, p)| p.len() as u64).sum());
    };
    let (mut written, mut deferred) = (0u64, 0u64);
    let mut part: Vec<(SeriesKey, Vec<(i64, f64)>)> = Vec::new();
    let mut n = 0usize;
    let mut rest = series.into_iter().peekable();
    while let Some(s) = rest.next() {
        n += s.1.len();
        part.push(s);
        if n >= WRITE_POINTS || rest.peek().is_none() {
            let t0 = Instant::now();
            while o.busy() && t0.elapsed() < BUSY_WAIT {
                crate::rt::sleep(Duration::from_millis(200)).await;
            }
            let points = n as u64;
            if o.offer(std::mem::take(&mut part)) {
                written += points;
            } else {
                deferred += points;
            }
            n = 0;
        }
    }
    (written, deferred)
}

/// Writes a pull's log records that no pull took before (one of each the pull read twice), in time
/// order and in parts, each when observe has room for it: the records written, their ids kept in
/// `seen`; and those left to the next pull to read again.
async fn write_logs(logs: Vec<(u64, Record)>, seen: &mut Seen) -> (u64, u64) {
    let mut fresh: Vec<(u64, Record)> = logs.into_iter().filter(|(id, _)| !seen.has(*id)).collect();
    fresh.sort_by_key(|(id, _)| *id);
    fresh.dedup_by_key(|(id, _)| *id);
    fresh.sort_by_key(|(_, r)| r.ts);
    let Some(o) = super::api::get() else {
        return (0, fresh.len() as u64);
    };
    let (mut written, mut deferred) = (0u64, 0u64);
    let mut rest = fresh.into_iter().peekable();
    while rest.peek().is_some() {
        let part: Vec<(u64, Record)> = rest.by_ref().take(WRITE_RECORDS).collect();
        let t0 = Instant::now();
        while o.busy() && t0.elapsed() < BUSY_WAIT {
            crate::rt::sleep(Duration::from_millis(200)).await;
        }
        let ids: Vec<(u64, i64)> = part.iter().map(|(id, r)| (*id, r.ts.div_euclid(1_000_000))).collect();
        let n = part.len() as u64;
        if o.offer_logs(part.into_iter().map(|(_, r)| r).collect()) {
            written += n;
            for (id, ms) in ids {
                seen.add(id, ms);
            }
        } else {
            deferred += n;
        }
    }
    (written, deferred)
}

/// kiln's own series of a pull.
#[allow(clippy::too_many_arguments)]
fn record(name: &str, kind: &str, points: u64, records: u64, calls: u64, errors: u64, series: usize, took: Duration) {
    let Some(o) = super::api::get() else { return };
    let ts = super::clock_ms();
    let tags = [("integration", name), ("cloud", kind)];
    let mut out = Vec::with_capacity(6);
    for (metric, k, v) in [
        ("kiln.cloud.points", Kind::Count, points as f64),
        ("kiln.cloud.records", Kind::Count, records as f64),
        ("kiln.cloud.calls", Kind::Count, calls as f64),
        ("kiln.cloud.errors", Kind::Count, errors as f64),
        ("kiln.cloud.series", Kind::Gauge, series as f64),
        ("kiln.cloud.pull.duration", Kind::Gauge, took.as_secs_f64()),
    ] {
        if let Ok(key) = SeriesKey::new(metric, tags, k) {
            out.push((key, vec![(ts, v)]));
        }
    }
    o.write(out);
}

fn event(name: &str, kind: &str, severity: u8, result: &str, message: String) {
    let Some(o) = super::api::get() else { return };
    o.log(vec![Record {
        ts: super::clock_ms().saturating_mul(1_000_000),
        severity,
        service: "kiln.cloud".into(),
        host: String::new(),
        source: "kiln.event".into(),
        message,
        attrs: vec![("event.type".into(), "cloud.pull".into()), ("integration".into(), name.into()), ("cloud".into(), kind.into()), ("result".into(), result.into())],
        trace_id: None,
        span_id: None,
    }]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_s_level_is_read_as_a_log_pipeline_reads_it() {
        for (m, want) in [
            (r#"{"level":"error","msg":"no"}"#, 17),
            (r#"{"severity":"WARNING"}"#, 13),
            (r#"{"levelname": "DEBUG", "x": 1}"#, 5),
            ("[ERROR]\t2026-10-18T12:00:00Z\tabc\tboom", 17),
            ("2026-10-18T12:00:00.000Z\t6f2b\tWARN\tslow", 13),
            ("time=2026 level=warn msg=late", 13),
            ("INFO starting", 9),
            ("FATAL: out of memory", 21),
            ("Error in a sentence is not a level", 9),
            ("NOTE: nothing to see", 9),
            ("", 9),
        ] {
            assert_eq!(severity_of(m), want, "{m:?}");
        }
    }

    /// A word passed at once (no `=`, its first letter or digit not a capital) is one the full
    /// reading passes too: random first words of levels, near misses and others read alike.
    #[test]
    fn a_line_s_words_read_as_they_did() {
        fn before(m: &str) -> u8 {
            let m = m.trim_start();
            for word in m.split(|c: char| c.is_whitespace() || c == '|').filter(|w| !w.is_empty()).take(4) {
                let w = word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '=');
                let w = w.rsplit_once('=').map_or(w, |(k, v)| if k.eq_ignore_ascii_case("level") || k.eq_ignore_ascii_case("severity") { v } else { "" });
                if ((w.len() >= 4 && w.bytes().all(|b| b.is_ascii_uppercase())) || word.contains('='))
                    && let Some(r) = kiln_obs::logs::record::severities(w)
                {
                    return *r.start();
                }
            }
            9
        }
        let words = [
            "ERROR",
            "error",
            "[WARN]",
            "WARN",
            "level=info",
            "Level=Error",
            "severity=warning",
            "\tERROR\t",
            "é",
            "ÉRROR",
            "1ERROR",
            "-ERROR-",
            "=ERROR",
            "ERR",
            "CRITICAL",
            "x=y",
            "|",
            " ",
            "\t",
            "10.0.0.1",
            "[18/Oct/2026:12:00:00",
            "-",
            "FATAL:",
            "DEBUG",
            "Debug",
            "INFO",
            "NOTICE",
            "a=b=ERROR",
            "level=",
            "=",
            "TRACE",
            "(WARNING)",
            "WARNING!",
            "😀ERROR",
        ];
        let mut rng = kiln_obs::testkit::Rng::new(31);
        for _ in 0..50_000 {
            let line: String = (0..rng.below(7)).map(|_| words.get(rng.below(words.len() as u64) as usize).copied().unwrap_or("")).collect::<Vec<_>>().join(if rng.below(2) == 0 { " " } else { "" });
            assert_eq!(severity_of_words(&line), before(&line), "{line:?}");
        }
    }

    #[test]
    fn seen_ids_are_kept_for_the_lookback_and_bounded() {
        let mut s = Seen::default();
        s.add(1, 1_000);
        s.add(2, 2_000);
        assert!(s.has(1) && s.has(2) && !s.has(3));
        s.prune(1_500);
        assert!(!s.has(1) && s.has(2), "older than the lookback, forgotten");
        assert_eq!(s.forgotten, 0, "forgotten in time is not counted");
        for i in 0..(SEEN_MAX as u64 + 10) {
            s.add(i + 100, 10_000 + i as i64);
        }
        s.prune(0);
        assert!(s.len() <= SEEN_MAX / 2 + 11 && s.forgotten > 0, "{} kept, {} forgotten", s.len(), s.forgotten);
        assert!(s.has(SEEN_MAX as u64 + 105), "the newest are kept");
        assert_eq!(id_of(&["a", "b"]), id_of(&["a", "b"]));
        assert_ne!(id_of(&["a", "b"]), id_of(&["ab", ""]), "parts are told apart");
    }

    #[test]
    fn names_break_into_words_as_datadog_s_do() {
        for (from, to) in [
            ("CPUUtilization", "cpuutilization"),
            ("NetworkIn", "network_in"),
            ("CPUCreditBalance", "cpucredit_balance"),
            ("EBSReadBytes", "ebsread_bytes"),
            ("HTTPCode_ELB_5XX", "httpcode_elb_5xx"),
            ("ApproximateNumberOfMessagesVisible", "approximate_number_of_messages_visible"),
            ("Percentage CPU", "percentage_cpu"),
            ("Disk Read Bytes/sec", "disk_read_bytes_sec"),
            ("UsedCapacity", "used_capacity"),
            ("Http5xx", "http5xx"),
            ("  odd -- name  ", "odd_name"),
        ] {
            assert_eq!(snake(from), to, "{from}");
        }
        assert_eq!(lower("ApplicationELB"), "applicationelb");
        assert_eq!(lower("My Company/App"), "my_company_app");
    }

    #[test]
    fn a_tag_said_twice_is_the_first_and_encoding_leaves_unreserved_characters() {
        let k = key("m", &[("region", "us-east-1"), ("region", "other"), ("", "x"), ("a", "1")], Kind::Gauge).map(|k| k.key).unwrap_or_default();
        assert_eq!(kiln_obs::batch::parse_key(&k).map(|k| k.tags).ok(), Some(vec![("a", "1"), ("region", "us-east-1")]));
        assert_eq!(form(&[("scope", "https://x/.default"), ("a b", "c&d=e")]), "scope=https%3A%2F%2Fx%2F.default&a%20b=c%26d%3De");
        assert_eq!(parse_time("2026-10-18T12:00:00Z").map(rfc3339), Some("2026-10-18T12:00:00Z".to_owned()));
        assert_eq!(parse_time("2026-10-18T12:00:00.250+00:00"), parse_time("2026-10-18T12:00:00Z").map(|t| t + 250));
    }

    fn aws(secret: Option<&str>) -> Cloud {
        Cloud::Aws(aws::Config { access_key_id: Some("AKID".into()), secret_access_key: secret.map(str::to_owned), regions: vec!["us-east-1".into()], ..aws::Config::default() })
    }

    #[test]
    fn secrets_are_sealed_at_rest_hidden_when_shown_and_kept_when_left_out() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let master = dir.path().join("secrets").join("master.key");
        let http = Arc::new(Client::public()?);
        let clouds = Clouds::open(dir.path(), &master, http.clone(), crate::rt::Shutdown::new());
        {
            let mut all = lock(&clouds.all);
            all.insert("prod".into(), aws(Some("s3cr3t-key")));
            clouds.save(&all)?;
        }
        let at_rest = std::fs::read_to_string(dir.path().join("clouds.json"))?;
        anyhow::ensure!(!at_rest.contains("s3cr3t-key") && at_rest.contains(SEALED), "{at_rest}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            anyhow::ensure!(std::fs::metadata(dir.path().join("clouds.json"))?.permissions().mode() & 0o077 == 0);
        }
        // Read again, the secret opens; shown, it is hidden; a change that leaves it out keeps it.
        let again = Clouds::open(dir.path(), &master, http.clone(), crate::rt::Shutdown::new());
        anyhow::ensure!(again.get("prod") == Some(aws(Some("s3cr3t-key"))));
        let shown = again.list();
        anyhow::ensure!(shown.first().and_then(|v| v.get("secret_access_key")).and_then(Value::as_str) == Some(HIDDEN), "{shown:?}");
        let mut changed = aws(Some(HIDDEN));
        changed.keep_secrets(again.get("prod").as_ref());
        anyhow::ensure!(changed == aws(Some("s3cr3t-key")));
        let mut removed = Cloud::Aws(aws::Config { access_key_id: None, secret_access_key: Some(String::new()), regions: vec!["us-east-1".into()], ..aws::Config::default() });
        removed.keep_secrets(again.get("prod").as_ref());
        anyhow::ensure!(matches!(&removed, Cloud::Aws(c) if c.secret_access_key.is_none()) && removed.check().is_ok(), "an empty secret removes the kept one");
        let mut other = Cloud::Gcp(gcp::Config::default());
        other.keep_secrets(again.get("prod").as_ref());
        anyhow::ensure!(matches!(other, Cloud::Gcp(gcp::Config { service_account: None, .. })), "another cloud's secrets are not taken");
        // Under another master key the secret does not open: said, not used.
        std::fs::write(&master, [7u8; 32])?;
        let other_key = Clouds::open(dir.path(), &master, http, crate::rt::Shutdown::new());
        anyhow::ensure!(other_key.get("prod") == Some(aws(None)));
        let s = other_key.list();
        let status = s.first().and_then(|v| v.get("status")).cloned().unwrap_or(Value::Null);
        anyhow::ensure!(status.get("state") == Some(&Value::from("failing")) && status.get("error").and_then(Value::as_str).is_some_and(|e| e.contains("secret_access_key")), "{status}");
        Ok(())
    }
}
