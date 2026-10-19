//! Google Cloud Monitoring (`docs/observe.md` §10.1): each project's metric types of the services
//! asked (`metricDescriptors.list`, by prefix), each type's time series read aligned to the period
//! (`timeSeries.list`).
//!
//! Names: `gcp.<service>.<path>`, the type's domain's first word and its path's words by `.`
//! (`compute.googleapis.com/instance/cpu/utilization` → `gcp.compute.instance.cpu.utilization`,
//! `kubernetes.io/container/cpu/core_usage_time` → `gcp.kubernetes.container.cpu.core_usage_time`),
//! tagged with the metric's labels, the resource's (`project_id`, `zone`, `instance_id`, ...),
//! `resource_type`, then the metadata labels Google gives (an instance's `name`, user labels):
//! - GAUGE numbers: each period's mean (`ALIGN_MEAN`), a gauge; BOOL, the fraction true
//!   (`ALIGN_FRACTION_TRUE`).
//! - DELTA and CUMULATIVE numbers: each period's increase (`ALIGN_DELTA`, Google finding the
//!   resets), a count.
//! - DELTA and CUMULATIVE distributions: each period's (`ALIGN_DELTA`): `<name>.count` and
//!   `<name>.sum`, counts; `<name>.avg`, and `<name>.p50`, `.p95`, `.p99` read from its buckets,
//!   gauges: linear within a bucket as Prometheus's `histogram_quantile` reads one, log-linear
//!   within an exponential bucket (whose points are spread so).
//! - STRING, MONEY and GAUGE distributions are not read.
//!
//! A type without series is quiet: asked again hourly, not each pull, so a project's thousands of
//! built-in types cost one call each an hour, and those with data one each pull.
//!
//! Logs: each project's entries matching `logs` (a filter in Cloud Logging's query language,
//! `severity>=WARNING`; empty, every entry), read over the lookback oldest first (`entries.list`, a
//! thousand a page, 30 pages a project a pull: half of Logging's 60 calls a minute). A record:
//! `source` `gcp.<resource type>`, `service` the resource's (a Cloud Run service's, a function's, a
//! container's, an App Engine module's) else the log's name, `host` the instance or pod, the trace
//! and span from `trace` and `spanId`, `textPayload` or a JSON payload's `message` its message, the
//! payload's other top-level values attributes; its id the entry's `insertId` with its log and time.
//!
//! Credentials: the integration's service account key (its JSON), else the server's own as
//! Google's libraries find them: `GOOGLE_APPLICATION_CREDENTIALS` (a service account's key, or a
//! user's from `gcloud auth application-default login`), gcloud's file, the metadata server
//! (Compute Engine, GKE's Workload Identity, Cloud Run; `GCE_METADATA_HOST`).

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use base64::Engine as _;
use kiln_obs::batch::Kind;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use kiln_obs::logs::record::Record;

use super::{Answer, Api, Call, Common, DISCOVER_EVERY, METADATA_TIMEOUT, Pulled, REFRESH_MS, encode, env, home, number, parse_time, rfc3339};

/// What kiln asks of Google: reading monitoring data, and logs when it reads them; nothing more.
const SCOPE: &str = "https://www.googleapis.com/auth/monitoring.read";
const LOGGING_SCOPE: &str = "https://www.googleapis.com/auth/logging.read";
const LOGGING: &str = "https://logging.googleapis.com";
/// `entries.list`'s largest page, and the pages a project's pull reads at the most (Logging allows
/// 60 calls a minute a project).
const LOG_PAGE: u64 = 1_000;
const LOG_PAGES: usize = 30;
/// A record's attributes from its payload, at the most.
const PAYLOAD_ATTRS: usize = 32;
const TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
const MONITORING: &str = "https://monitoring.googleapis.com";
/// The metadata server, unless `GCE_METADATA_HOST` says another (its address, as Google's
/// libraries ask it: no name to resolve off Google's cloud).
const METADATA: &str = "169.254.169.254";
/// Types asked at once.
const CONCURRENCY: usize = 4;
/// How often a quiet type (no series last time) is asked again.
const QUIET: Duration = Duration::from_secs(3600);
/// Quantiles read from a distribution's buckets.
const QUANTILES: [(f64, &str); 3] = [(0.5, "p50"), (0.95, "p95"), (0.99, "p99")];

fn period() -> u64 {
    60
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// Projects read; none: the credentials' own.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub projects: Vec<String>,
    /// Metric type prefixes read (`compute.googleapis.com`, `cloudsql.googleapis.com/database`,
    /// `kubernetes.io`); none: every type.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<String>,
    /// Seconds a point covers: a multiple of 60.
    #[serde(default = "period")]
    pub period_s: u64,
    /// A service account's key, its JSON as Google gives it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_account: Option<String>,
    /// The Monitoring API's root instead of Google's: a test's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Log entries read, of each project: a filter in Cloud Logging's query language; empty, every
    /// entry; none, no logs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logs: Option<String>,
    /// Cloud Logging's root instead of Google's: a test's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logs_endpoint: Option<String>,
    #[serde(flatten)]
    pub common: Common,
}

impl Default for Config {
    fn default() -> Self {
        Self { projects: Vec::new(), services: Vec::new(), period_s: period(), service_account: None, endpoint: None, logs: None, logs_endpoint: None, common: Common::default() }
    }
}

impl Config {
    pub fn check(&self) -> Result<(), String> {
        if let Some(p) = self.projects.iter().find(|p| p.is_empty() || p.len() > 100 || !p.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-.:".contains(&b))) {
            return Err(format!("project {p:?}: a project's id (my-project-123)"));
        }
        if let Some(s) = self.services.iter().find(|s| s.is_empty() || s.len() > 200 || s.contains(['"', '\\'])) {
            return Err(format!("service {s:?}: a metric type's prefix (compute.googleapis.com)"));
        }
        if !self.period_s.is_multiple_of(60) || !(60..=86_400).contains(&self.period_s) {
            return Err(format!("period_s {}: a multiple of 60 up to 86400", self.period_s));
        }
        if let Some(k) = &self.service_account {
            Source::of_key(k).map_err(|e| format!("service_account: {e:#}"))?;
        }
        if self.endpoint.as_deref().is_some_and(|e| !(e.starts_with("https://") || e.starts_with("http://"))) {
            return Err("endpoint: a URL".into());
        }
        if self.logs_endpoint.as_deref().is_some_and(|e| !(e.starts_with("https://") || e.starts_with("http://"))) {
            return Err("logs_endpoint: a URL".into());
        }
        if self.logs.as_deref().is_some_and(|f| f.len() > 20_000) {
            return Err("logs: a filter of 20000 characters at the most (Cloud Logging's)".into());
        }
        Ok(())
    }

    fn logging(&self) -> &str {
        self.logs_endpoint.as_deref().unwrap_or(LOGGING).trim_end_matches('/')
    }

    /// The scopes its token asks for.
    fn scope(&self) -> String {
        if self.logs.is_some() { format!("{SCOPE} {LOGGING_SCOPE}") } else { SCOPE.to_owned() }
    }

    fn monitoring(&self) -> &str {
        self.endpoint.as_deref().unwrap_or(MONITORING).trim_end_matches('/')
    }
}

// -------------------------------------------------------------------------------- credentials

/// A credentials file as Google writes them: a service account's key, or a user's.
#[derive(Deserialize)]
struct KeyFile {
    #[serde(rename = "type")]
    kind: String,
    project_id: Option<String>,
    private_key_id: Option<String>,
    private_key: Option<String>,
    client_email: Option<String>,
    token_uri: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
    refresh_token: Option<String>,
    quota_project_id: Option<String>,
}

enum Source {
    Account { email: String, key: aws_lc_rs::rsa::KeyPair, key_id: Option<String>, token_uri: String, project: Option<String> },
    User { client_id: String, client_secret: String, refresh_token: String, token_uri: String, project: Option<String> },
    Metadata,
}

/// A PEM's first block, as DER.
fn pem(text: &str) -> anyhow::Result<Vec<u8>> {
    let body: String = text.lines().map(str::trim).skip_while(|l| !l.starts_with("-----BEGIN")).skip(1).take_while(|l| !l.starts_with("-----END")).collect();
    anyhow::ensure!(!body.is_empty(), "no PEM block");
    Ok(base64::engine::general_purpose::STANDARD.decode(body)?)
}

impl Source {
    fn of_key(text: &str) -> anyhow::Result<Self> {
        let f: KeyFile = serde_json::from_str(text).map_err(|e| anyhow::anyhow!("not a credentials file: {e}"))?;
        match f.kind.as_str() {
            "service_account" => {
                let email = f.client_email.ok_or_else(|| anyhow::anyhow!("a service account's key without its client_email"))?;
                let der = pem(f.private_key.as_deref().unwrap_or_default())?;
                let key = aws_lc_rs::rsa::KeyPair::from_pkcs8(&der).map_err(|e| anyhow::anyhow!("its private_key is not an RSA key ({e})"))?;
                Ok(Self::Account { email, key, key_id: f.private_key_id, token_uri: f.token_uri.unwrap_or_else(|| TOKEN_URI.into()), project: f.project_id })
            }
            "authorized_user" => Ok(Self::User {
                client_id: f.client_id.ok_or_else(|| anyhow::anyhow!("a user's credentials without client_id"))?,
                client_secret: f.client_secret.ok_or_else(|| anyhow::anyhow!("a user's credentials without client_secret"))?,
                refresh_token: f.refresh_token.ok_or_else(|| anyhow::anyhow!("a user's credentials without refresh_token"))?,
                token_uri: f.token_uri.unwrap_or_else(|| TOKEN_URI.into()),
                project: f.quota_project_id,
            }),
            other => anyhow::bail!("credentials of type {other:?}: a service account's key or a user's (external accounts are not read)"),
        }
    }

    /// The server's own, as Google's libraries find them.
    fn ambient() -> anyhow::Result<Self> {
        if let Some(path) = env("GOOGLE_APPLICATION_CREDENTIALS") {
            let text = std::fs::read_to_string(&path).map_err(|e| anyhow::anyhow!("GOOGLE_APPLICATION_CREDENTIALS {path}: {e}"))?;
            return Self::of_key(&text).map_err(|e| anyhow::anyhow!("GOOGLE_APPLICATION_CREDENTIALS {path}: {e:#}"));
        }
        let gcloud = env("CLOUDSDK_CONFIG").map(std::path::PathBuf::from).or_else(|| {
            if cfg!(windows) { env("APPDATA").map(|a| std::path::PathBuf::from(a).join("gcloud")) } else { home().map(|h| h.join(".config").join("gcloud")) }
        });
        if let Some(text) = gcloud.map(|d| d.join("application_default_credentials.json")).and_then(|f| std::fs::read_to_string(f).ok()) {
            return Self::of_key(&text);
        }
        Ok(Self::Metadata)
    }

    fn project(&self) -> Option<&str> {
        match self {
            Self::Account { project, .. } | Self::User { project, .. } => project.as_deref(),
            Self::Metadata => None,
        }
    }
}

fn metadata_url(path: &str) -> String {
    format!("http://{}{path}", env("GCE_METADATA_HOST").unwrap_or_else(|| METADATA.into()))
}

/// A signed JWT asking `token_uri` for an access token (RFC 7523, Google's service accounts).
fn assertion(email: &str, key: &aws_lc_rs::rsa::KeyPair, key_id: Option<&str>, token_uri: &str, scope: &str, now_s: i64) -> anyhow::Result<String> {
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let mut head = serde_json::Map::new();
    head.insert("alg".into(), "RS256".into());
    head.insert("typ".into(), "JWT".into());
    if let Some(k) = key_id {
        head.insert("kid".into(), k.into());
    }
    let claims = serde_json::json!({ "iss": email, "scope": scope, "aud": token_uri, "iat": now_s, "exp": now_s + 3600 });
    let signing = format!("{}.{}", b64.encode(serde_json::to_vec(&head)?), b64.encode(serde_json::to_vec(&claims)?));
    let mut sig = vec![0u8; key.public_modulus_len()];
    key.sign(&aws_lc_rs::signature::RSA_PKCS1_SHA256, &aws_lc_rs::rand::SystemRandom::new(), signing.as_bytes(), &mut sig).map_err(|_| anyhow::anyhow!("the service account's key did not sign"))?;
    Ok(format!("{signing}.{}", b64.encode(sig)))
}

/// An OAuth error's words (`{"error": ..., "error_description": ...}`), else Google's API's
/// (`{"error": {"message": ...}}`), else the body.
fn error(a: &Answer) -> String {
    let v: Value = serde_json::from_slice(&a.body).unwrap_or(Value::Null);
    let said = match v.get("error") {
        Some(Value::String(e)) => Some(v.get("error_description").and_then(Value::as_str).map_or_else(|| e.clone(), |d| format!("{e}: {d}"))),
        Some(Value::Object(o)) => o.get("message").and_then(Value::as_str).map(|m| o.get("status").and_then(Value::as_str).map_or_else(|| m.to_owned(), |s| format!("{s}: {m}"))),
        _ => None,
    };
    format!("{} {}", a.status, said.unwrap_or_else(|| a.text()))
}

/// Google's quota errors come as 429; a 403 of `rateLimitExceeded` is one too.
fn throttled(a: &Answer) -> bool {
    a.status == 403 && String::from_utf8_lossy(&a.body).contains("rateLimitExceeded")
}

struct Token {
    value: String,
    until: i64,
}

async fn token(api: &Api<'_>, src: &Source, scope: &str, now: i64) -> anyhow::Result<Token> {
    let (a, what) = match src {
        Source::Account { email, key, key_id, token_uri, .. } => {
            let jwt = assertion(email, key, key_id.as_deref(), token_uri, scope, crate::util::now_unix())?;
            let body = super::form(&[("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"), ("assertion", &jwt)]);
            (api.call(&Call::post(token_uri, "application/x-www-form-urlencoded", body), |_| false).await?, format!("the service account {email}'s token"))
        }
        Source::User { client_id, client_secret, refresh_token, token_uri, .. } => {
            let body = super::form(&[("grant_type", "refresh_token"), ("client_id", client_id), ("client_secret", client_secret), ("refresh_token", refresh_token)]);
            (api.call(&Call::post(token_uri, "application/x-www-form-urlencoded", body), |_| false).await?, "the user's token".to_owned())
        }
        Source::Metadata => {
            let url = metadata_url("/computeMetadata/v1/instance/service-accounts/default/token");
            let a = api.once(&Call::get(&url).header("metadata-flavor", "Google").timeout(METADATA_TIMEOUT)).await.map_err(|e| {
                anyhow::anyhow!("no Google credentials: none given, no GOOGLE_APPLICATION_CREDENTIALS, no gcloud application default credentials, and no metadata server ({e:#})")
            })?;
            (a, "the metadata server's token".to_owned())
        }
    };
    anyhow::ensure!(a.ok(), "{what}: {}", error(&a));
    let v: Value = a.json()?;
    let value = v.get("access_token").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("{what}: no access_token"))?.to_owned();
    let expires_in = v.get("expires_in").and_then(number).unwrap_or(3600.0);
    Ok(Token { value, until: now + (expires_in * 1000.0) as i64 })
}

// ----------------------------------------------------------------------------------- the API

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Descriptors {
    #[serde(default)]
    metric_descriptors: Vec<Descriptor>,
    next_page_token: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Descriptor {
    #[serde(rename = "type")]
    kind: String,
    metric_kind: Option<String>,
    value_type: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SeriesPage {
    #[serde(default)]
    time_series: Vec<Series>,
    next_page_token: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Series {
    #[serde(default)]
    metric: Labeled,
    #[serde(default)]
    resource: Labeled,
    metadata: Option<Metadata>,
    #[serde(default)]
    points: Vec<Point>,
}

#[derive(Debug, Default, Deserialize)]
struct Labeled {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    labels: BTreeMap<String, String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Metadata {
    #[serde(default)]
    system_labels: serde_json::Map<String, Value>,
    #[serde(default)]
    user_labels: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct Point {
    interval: Interval,
    value: Typed,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Interval {
    end_time: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Typed {
    double_value: Option<f64>,
    int64_value: Option<Value>,
    bool_value: Option<bool>,
    distribution_value: Option<Distribution>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Distribution {
    count: Option<Value>,
    mean: Option<f64>,
    bucket_options: Option<BucketOptions>,
    #[serde(default)]
    bucket_counts: Vec<Value>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BucketOptions {
    linear_buckets: Option<Linear>,
    exponential_buckets: Option<Exponential>,
    explicit_buckets: Option<Explicit>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Linear {
    num_finite_buckets: u32,
    width: f64,
    offset: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Exponential {
    num_finite_buckets: u32,
    growth_factor: f64,
    scale: f64,
}

#[derive(Debug, Clone, Deserialize)]
struct Explicit {
    #[serde(default)]
    bounds: Vec<f64>,
}

impl BucketOptions {
    /// Bucket `i`'s bounds (`[lower, upper)`): the first's lower is -∞, the last's upper +∞.
    fn bounds(&self, i: usize) -> (f64, f64) {
        let i = i as f64;
        if let Some(l) = &self.linear_buckets {
            let n = f64::from(l.num_finite_buckets);
            let at = |j: f64| l.offset + l.width * j;
            return (if i == 0.0 { f64::NEG_INFINITY } else { at(i - 1.0) }, if i > n { f64::INFINITY } else { at(i) });
        }
        if let Some(e) = &self.exponential_buckets {
            let n = f64::from(e.num_finite_buckets);
            let at = |j: f64| e.scale * e.growth_factor.powf(j);
            return (if i == 0.0 { f64::NEG_INFINITY } else { at(i - 1.0) }, if i > n { f64::INFINITY } else { at(i) });
        }
        let b = self.explicit_buckets.as_ref().map_or(&[][..], |e| e.bounds.as_slice());
        let j = i as usize;
        (if j == 0 { f64::NEG_INFINITY } else { b.get(j - 1).copied().unwrap_or(f64::INFINITY) }, b.get(j).copied().unwrap_or(f64::INFINITY))
    }
}

/// The `q` quantile of a distribution's buckets: linear within a bucket (Prometheus's
/// `histogram_quantile`), log-linear within an exponential one; the lowest bucket's from 0 when its
/// upper bound is above 0, else its upper bound; the overflow bucket's its lower bound.
fn quantile(opts: &BucketOptions, counts: &[f64], q: f64) -> Option<f64> {
    let total: f64 = counts.iter().sum();
    if total <= 0.0 {
        return None;
    }
    let rank = q * total;
    let mut below = 0.0;
    for (i, &c) in counts.iter().enumerate() {
        if c <= 0.0 || below + c < rank {
            below += c;
            continue;
        }
        let (lo, hi) = opts.bounds(i);
        let f = ((rank - below) / c).clamp(0.0, 1.0);
        return Some(if hi.is_infinite() {
            lo
        } else if lo.is_infinite() {
            if hi > 0.0 { hi * f } else { hi }
        } else if opts.exponential_buckets.is_some() && lo > 0.0 {
            lo * (hi / lo).powf(f)
        } else {
            lo + (hi - lo) * f
        });
    }
    None
}

/// What a type is read as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Read {
    Mean,
    Fraction,
    Delta,
    Distribution,
}

impl Read {
    fn of(metric_kind: &str, value_type: &str) -> Option<Self> {
        match (metric_kind, value_type) {
            ("GAUGE", "DOUBLE" | "INT64") => Some(Self::Mean),
            ("GAUGE", "BOOL") => Some(Self::Fraction),
            ("DELTA" | "CUMULATIVE", "DOUBLE" | "INT64") => Some(Self::Delta),
            ("DELTA" | "CUMULATIVE", "DISTRIBUTION") => Some(Self::Distribution),
            _ => None,
        }
    }

    fn aligner(self) -> &'static str {
        match self {
            Self::Mean => "ALIGN_MEAN",
            Self::Fraction => "ALIGN_FRACTION_TRUE",
            Self::Delta | Self::Distribution => "ALIGN_DELTA",
        }
    }
}

/// A type's name: `gcp.<service>.<path>`.
fn metric_name(t: &str) -> String {
    let (domain, path) = t.split_once('/').unwrap_or((t, ""));
    let mut out = format!("gcp.{}", super::lower(domain.split('.').next().unwrap_or(domain)));
    for w in path.split(['/', '.']).map(super::lower).filter(|w| !w.is_empty()) {
        out.push('.');
        out.push_str(&w);
    }
    out
}

/// A type found in a project, and when it was last asked and had series.
#[derive(Debug)]
struct Type {
    project: String,
    name: String,
    read: Read,
    asked: Option<Instant>,
    quiet: bool,
}

pub struct Puller {
    cfg: Config,
    tags: Vec<(String, String)>,
    source: Option<Source>,
    token: Option<Token>,
    projects: Vec<String>,
    types: Vec<Type>,
    discovered: Option<Instant>,
}

impl Puller {
    pub fn new(cfg: Config) -> Self {
        let tags = cfg.common.tag_pairs();
        Self { projects: cfg.projects.clone(), cfg, tags, source: None, token: None, types: Vec::new(), discovered: None }
    }

    async fn bearer(&mut self, api: &Api<'_>, now: i64) -> anyhow::Result<String> {
        if let Some(t) = &self.token
            && t.until.saturating_sub(now) > REFRESH_MS
        {
            return Ok(t.value.clone());
        }
        if self.source.is_none() {
            self.source = Some(match &self.cfg.service_account {
                Some(k) => Source::of_key(k)?,
                None => Source::ambient()?,
            });
        }
        let src = self.source.as_ref().ok_or_else(|| anyhow::anyhow!("no credentials"))?;
        let t = token(api, src, &self.cfg.scope(), now).await?;
        let v = t.value.clone();
        self.token = Some(t);
        Ok(v)
    }

    async fn get(&self, api: &Api<'_>, token: &str, url: &str) -> anyhow::Result<Answer> {
        let a = api.call(&Call::get(url).bearer(token), throttled).await?;
        anyhow::ensure!(a.ok(), "{}", error(&a));
        Ok(a)
    }

    pub async fn pull(&mut self, api: &Api<'_>, now: i64) -> anyhow::Result<Pulled> {
        let token = self.bearer(api, now).await?;
        if self.projects.is_empty() {
            self.projects = self.home_project(api).await?;
        }
        let mut warnings = Vec::new();
        if self.discovered.is_none_or(|t| t.elapsed() >= DISCOVER_EVERY) {
            match self.discover(api, &token, &mut warnings).await {
                Ok(()) => self.discovered = Some(Instant::now()),
                Err(e) if self.discovered.is_some() => warnings.push(format!("metric types were not listed again ({e:#}); those listed before are read")),
                Err(e) => return Err(e),
            }
        }
        let p = i64::try_from(self.cfg.period_s.saturating_mul(1000)).unwrap_or(60_000);
        let end = now.div_euclid(p) * p;
        let start = (end - self.cfg.common.lookback_ms()).div_euclid(p) * p;
        let asked: Vec<usize> = (0..self.types.len()).filter(|&i| self.types.get(i).is_some_and(|t| !t.quiet || t.asked.is_none_or(|a| a.elapsed() >= QUIET))).collect();
        let this = &*self;
        let token = token.as_str();
        let read: Vec<(usize, anyhow::Result<Vec<Series>>)> = crate::rt::each(asked.iter().copied(), CONCURRENCY, |i| async move {
            let Some(t) = this.types.get(i) else { return (i, Ok(Vec::new())) };
            (i, this.series(api, token, t, start, end).await)
        })
        .await;
        let mut out = Pulled::default();
        let mut budget = self.cfg.common.max_series;
        let (mut failed, mut first_error) = (0usize, None);
        let at = Instant::now();
        for (i, r) in read {
            let Some(t) = self.types.get_mut(i) else { continue };
            t.asked = Some(at);
            match r {
                Ok(series) => {
                    t.quiet = series.is_empty();
                    let (name, read) = (metric_name(&t.name), t.read);
                    for s in &series {
                        absorb(&name, read, s, &self.tags, &mut budget, &mut out);
                    }
                }
                Err(e) => {
                    failed += 1;
                    if warnings.len() < 20 {
                        warnings.push(format!("{} in {}: {e:#}", t.name, t.project));
                    }
                    first_error.get_or_insert(e);
                }
            }
        }
        if failed > 0 && failed == asked.len() {
            return Err(first_error.unwrap_or_else(|| anyhow::anyhow!("every type failed")));
        }
        out.known = out.series.len();
        if let Some(filter) = &self.cfg.logs {
            out.logs = self.read_logs(api, token, filter, now.saturating_sub(self.cfg.common.lookback_ms()), now, &mut warnings).await;
        }
        out.warnings = warnings;
        Ok(out)
    }

    /// Each project's entries matching `filter` in `[start, end]` (unix ms), oldest first, as
    /// records: a project that fails, a warning, the rest read.
    async fn read_logs(&self, api: &Api<'_>, token: &str, filter: &str, start: i64, end: i64, warnings: &mut Vec<String>) -> Vec<(u64, Record)> {
        let url = format!("{}/v2/entries:list", self.cfg.logging());
        let window = format!("timestamp>=\"{}\" AND timestamp<=\"{}\"", rfc3339(start), rfc3339(end));
        let filter = if filter.trim().is_empty() { window } else { format!("({filter}) AND {window}") };
        let mut out = Vec::new();
        for project in &self.projects {
            let mut next: Option<String> = None;
            for page_n in 0.. {
                if page_n == LOG_PAGES {
                    warnings.push(format!("{project}: {LOG_PAGES} pages of log entries read, the most a pull reads: the rest are the next pull's"));
                    break;
                }
                let mut body = serde_json::json!({ "resourceNames": [format!("projects/{project}")], "filter": filter, "orderBy": "timestamp asc", "pageSize": LOG_PAGE });
                if let (Some(t), Some(o)) = (&next, body.as_object_mut()) {
                    o.insert("pageToken".into(), t.as_str().into());
                }
                let answer = api.call(&Call::post(&url, "application/json", body.to_string()).bearer(token), throttled).await;
                let page: Value = match answer.and_then(|a| if a.ok() { a.json() } else { Err(anyhow::anyhow!("{}", error(&a))) }) {
                    Ok(v) => v,
                    Err(e) => {
                        warnings.push(format!("{project}: log entries: {e:#}"));
                        break;
                    }
                };
                if let Some(entries) = page.get("entries").and_then(Value::as_array) {
                    out.extend(entries.iter().filter_map(|e| record_of(project, e, &self.tags)));
                }
                match page.get("nextPageToken").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                    Some(t) if next.as_deref() != Some(t) => next = Some(t.to_owned()),
                    _ => break,
                }
            }
        }
        out
    }

    async fn home_project(&self, api: &Api<'_>) -> anyhow::Result<Vec<String>> {
        if let Some(p) = self.source.as_ref().and_then(Source::project).map(str::to_owned).or_else(|| env("GOOGLE_CLOUD_PROJECT")).or_else(|| env("CLOUDSDK_CORE_PROJECT")) {
            return Ok(vec![p]);
        }
        let url = metadata_url("/computeMetadata/v1/project/project-id");
        let a = api.once(&Call::get(&url).header("metadata-flavor", "Google").timeout(METADATA_TIMEOUT)).await.map_err(|e| anyhow::anyhow!("which projects? none said, none in the credentials or GOOGLE_CLOUD_PROJECT, and no metadata server ({e:#})"))?;
        anyhow::ensure!(a.ok(), "which projects? the metadata server: {}", a.text());
        Ok(vec![String::from_utf8_lossy(&a.body).trim().to_owned()])
    }

    /// Lists each project's types of the services asked, keeping what each was found as.
    async fn discover(&mut self, api: &Api<'_>, token: &str, warnings: &mut Vec<String>) -> anyhow::Result<()> {
        let mut found: Vec<Type> = Vec::new();
        let mut first_error = None;
        let mut listed = 0;
        for project in &self.projects {
            let prefixes: Vec<Option<&str>> = if self.cfg.services.is_empty() { vec![None] } else { self.cfg.services.iter().map(|s| Some(s.as_str())).collect() };
            for prefix in prefixes {
                match self.descriptors(api, token, project, prefix).await {
                    Ok(ds) => {
                        listed += 1;
                        for d in ds {
                            if let Some(read) = Read::of(d.metric_kind.as_deref().unwrap_or(""), d.value_type.as_deref().unwrap_or("")) {
                                found.push(Type { project: project.clone(), name: d.kind, read, asked: None, quiet: false });
                            }
                        }
                    }
                    Err(e) => {
                        warnings.push(format!("{project}: {e:#}"));
                        first_error.get_or_insert(e);
                    }
                }
            }
        }
        if listed == 0
            && let Some(e) = first_error
        {
            return Err(e);
        }
        found.sort_by(|a, b| (&a.project, &a.name).cmp(&(&b.project, &b.name)));
        found.dedup_by(|a, b| a.project == b.project && a.name == b.name);
        // What was known of a type stays known: when it was asked, and whether it was quiet.
        for t in &mut found {
            if let Some(old) = self.types.iter().find(|o| o.project == t.project && o.name == t.name) {
                t.asked = old.asked;
                t.quiet = old.quiet;
            }
        }
        self.types = found;
        Ok(())
    }

    async fn descriptors(&self, api: &Api<'_>, token: &str, project: &str, prefix: Option<&str>) -> anyhow::Result<Vec<Descriptor>> {
        let mut out = Vec::new();
        let mut next: Option<String> = None;
        loop {
            let mut url = format!("{}/v3/projects/{}/metricDescriptors?pageSize=1000", self.cfg.monitoring(), encode(project));
            if let Some(p) = prefix {
                url.push_str(&format!("&filter={}", encode(&format!("metric.type = starts_with(\"{p}\")"))));
            }
            if let Some(t) = &next {
                url.push_str(&format!("&pageToken={}", encode(t)));
            }
            let page: Descriptors = self.get(api, token, &url).await?.json()?;
            out.extend(page.metric_descriptors);
            match page.next_page_token.filter(|t| !t.is_empty()) {
                Some(t) if next.as_ref() != Some(&t) => next = Some(t),
                _ => break,
            }
        }
        Ok(out)
    }

    /// A type's series over `[start, end)`, aligned to the period.
    async fn series(&self, api: &Api<'_>, token: &str, t: &Type, start: i64, end: i64) -> anyhow::Result<Vec<Series>> {
        let mut out = Vec::new();
        let mut next: Option<String> = None;
        loop {
            let mut url = format!(
                "{}/v3/projects/{}/timeSeries?filter={}&interval.startTime={}&interval.endTime={}&aggregation.alignmentPeriod={}s&aggregation.perSeriesAligner={}&view=FULL",
                self.cfg.monitoring(),
                encode(&t.project),
                encode(&format!("metric.type = \"{}\"", t.name)),
                encode(&rfc3339(start)),
                encode(&rfc3339(end)),
                self.cfg.period_s,
                t.read.aligner(),
            );
            if let Some(n) = &next {
                url.push_str(&format!("&pageToken={}", encode(n)));
            }
            let page: SeriesPage = self.get(api, token, &url).await?.json()?;
            out.extend(page.time_series);
            match page.next_page_token.filter(|t| !t.is_empty()) {
                Some(t) if next.as_ref() != Some(&t) => next = Some(t),
                _ => break,
            }
        }
        Ok(out)
    }
}

/// A series' points as kiln's: a gauge or a count, or a distribution's six series; none past the
/// budget (counted as `over`).
fn absorb(name: &str, read: Read, s: &Series, tags: &[(String, String)], budget: &mut usize, out: &mut Pulled) {
    let system: Vec<(&str, String)> = s
        .metadata
        .iter()
        .flat_map(|m| m.system_labels.iter())
        .filter_map(|(k, v)| match v {
            Value::String(x) => Some((k.as_str(), x.clone())),
            Value::Bool(_) | Value::Number(_) => Some((k.as_str(), v.to_string())),
            _ => None,
        })
        .collect();
    let mut pairs: Vec<(&str, &str)> = s.metric.labels.iter().chain(&s.resource.labels).map(|(k, v)| (k.as_str(), v.as_str())).collect();
    if let Some(r) = &s.resource.kind {
        pairs.push(("resource_type", r.as_str()));
    }
    pairs.extend(system.iter().map(|(k, v)| (*k, v.as_str())));
    pairs.extend(s.metadata.iter().flat_map(|m| m.user_labels.iter()).map(|(k, v)| (k.as_str(), v.as_str())));
    pairs.extend(tags.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    let mut take = |metric: &str, kind: Kind, points: Vec<(i64, f64)>| {
        if points.is_empty() {
            return;
        }
        if *budget == 0 {
            out.over += 1;
            return;
        }
        if let Some(k) = super::key(metric, &pairs, kind) {
            *budget -= 1;
            out.series.push((k, points));
        }
    };
    let time = |p: &Point| parse_time(&p.interval.end_time);
    match read {
        Read::Mean | Read::Fraction | Read::Delta => {
            let mut points: Vec<(i64, f64)> = s.points.iter().filter_map(|p| Some((time(p)?, p.value.double_value.or_else(|| p.value.int64_value.as_ref().and_then(number)).or_else(|| p.value.bool_value.map(f64::from))?))).filter(|(_, v)| v.is_finite()).collect();
            points.sort_by_key(|p| p.0);
            take(name, if read == Read::Delta { Kind::Count } else { Kind::Gauge }, points);
        }
        Read::Distribution => {
            let mut count = Vec::new();
            let mut sum = Vec::new();
            let mut avg = Vec::new();
            let mut qs: [Vec<(i64, f64)>; 3] = Default::default();
            for p in &s.points {
                let (Some(ts), Some(d)) = (time(p), &p.value.distribution_value) else { continue };
                let n = d.count.as_ref().and_then(number).unwrap_or(0.0);
                let mean = d.mean.unwrap_or(0.0);
                count.push((ts, n));
                sum.push((ts, mean * n));
                if n > 0.0 {
                    avg.push((ts, mean));
                    let counts: Vec<f64> = d.bucket_counts.iter().map(|c| number(c).unwrap_or(0.0)).collect();
                    if let Some(opts) = &d.bucket_options {
                        for ((q, _), out) in QUANTILES.iter().zip(qs.iter_mut()) {
                            if let Some(v) = quantile(opts, &counts, *q).filter(|v| v.is_finite()) {
                                out.push((ts, v));
                            }
                        }
                    }
                }
            }
            take(&format!("{name}.count"), Kind::Count, count);
            take(&format!("{name}.sum"), Kind::Count, sum);
            take(&format!("{name}.avg"), Kind::Gauge, avg);
            for ((_, suffix), points) in QUANTILES.iter().zip(qs) {
                take(&format!("{name}.{suffix}"), Kind::Gauge, points);
            }
        }
    }
}

/// A log entry's record (the module's doc says how), its id; none without a time.
pub(super) fn record_of(project: &str, e: &Value, tags: &[(String, String)]) -> Option<(u64, Record)> {
    let at = e.get("timestamp").and_then(Value::as_str)?;
    let ts = super::parse_time_ns(at)?;
    let str_of = |v: Option<&Value>| v.and_then(Value::as_str).unwrap_or("").to_owned();
    let log_name = str_of(e.get("logName"));
    let log = log_name.split_once("/logs/").map_or(log_name.as_str(), |(_, l)| l).replace("%2F", "/");
    let id = super::id_of(&["gcp", project, &log_name, &str_of(e.get("insertId")), at]);
    let resource = e.get("resource");
    let rtype = str_of(resource.and_then(|r| r.get("type")));
    let rlabels = resource.and_then(|r| r.get("labels")).and_then(Value::as_object);
    let rlabel = |k: &str| rlabels.and_then(|l| l.get(k)).and_then(Value::as_str).filter(|v| !v.is_empty());
    let service = ["service_name", "function_name", "container_name", "module_id", "job_id"].iter().find_map(|k| rlabel(k)).unwrap_or(&log).to_owned();
    let host = ["instance_id", "pod_name", "node_name", "instance_name"].iter().find_map(|k| rlabel(k)).unwrap_or("").to_owned();
    let mut attrs: Vec<(String, String)> = vec![("gcp.log".into(), log.clone()), ("gcp.resource.type".into(), rtype.clone())];
    for (k, v) in rlabels.into_iter().flatten() {
        if let Some(v) = v.as_str() {
            attrs.push((format!("gcp.resource.labels.{k}"), v.to_owned()));
        }
    }
    for (k, v) in e.get("labels").and_then(Value::as_object).into_iter().flatten() {
        if let Some(v) = v.as_str() {
            attrs.push((format!("labels.{k}"), v.to_owned()));
        }
    }
    // The message: the text, a JSON payload's message (its other top-level values attributes), an
    // audit entry's method and resource, else the payload as it is.
    let scalar = |v: &Value| match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    };
    let message = if let Some(t) = e.get("textPayload").and_then(Value::as_str) {
        t.to_owned()
    } else if let Some(j) = e.get("jsonPayload").and_then(Value::as_object) {
        let said = ["message", "msg", "textPayload"].iter().find_map(|k| j.get(*k).and_then(Value::as_str)).map(str::to_owned);
        for (k, v) in j.iter().filter(|(k, _)| !matches!(k.as_str(), "message" | "msg" | "textPayload")).take(PAYLOAD_ATTRS) {
            if let Some(v) = scalar(v) {
                attrs.push((k.clone(), v));
            }
        }
        said.unwrap_or_else(|| Value::Object(j.clone()).to_string())
    } else if let Some(p) = e.get("protoPayload") {
        match (p.get("methodName").and_then(Value::as_str), p.get("resourceName").and_then(Value::as_str)) {
            (Some(m), Some(r)) => format!("{m} {r}"),
            (Some(m), None) => m.to_owned(),
            _ => p.to_string(),
        }
    } else {
        String::new()
    };
    for (k, v) in tags {
        if !attrs.iter().any(|(a, _)| a == k) {
            attrs.push((k.clone(), v.clone()));
        }
    }
    let severity = match e.get("severity").and_then(Value::as_str).and_then(kiln_obs::logs::record::severities) {
        Some(r) => *r.start(),
        None => super::severity_of(&message),
    };
    let hex = |s: &str| -> Option<Vec<u8>> { (0..s.len()).step_by(2).map(|i| s.get(i..i + 2).and_then(|b| u8::from_str_radix(b, 16).ok())).collect() };
    let trace_id = e.get("trace").and_then(Value::as_str).and_then(|t| t.rsplit('/').next()).and_then(hex).and_then(|b| <[u8; 16]>::try_from(b).ok());
    let span_id = e.get("spanId").and_then(Value::as_str).and_then(hex).and_then(|b| <[u8; 8]>::try_from(b).ok());
    Some((id, Record { ts, severity, service, host, source: format!("gcp.{rtype}"), message: super::message(message), attrs, trace_id, span_id }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_log_entry_is_a_record_with_its_resource_trace_and_payload() -> anyhow::Result<()> {
        let e: Value = serde_json::from_str(r#"{
            "logName": "projects/p/logs/run.googleapis.com%2Fstderr",
            "resource": {"type": "cloud_run_revision", "labels": {"service_name": "checkout", "project_id": "p"}},
            "timestamp": "2026-10-18T12:00:00.123456789Z", "insertId": "a1", "severity": "ERROR",
            "trace": "projects/p/traces/0123456789abcdef0123456789abcdef", "spanId": "000000000000004a",
            "jsonPayload": {"message": "card declined", "amount": 12, "retry": false, "nested": {"a": 1}},
            "labels": {"instanceId": "00a"}
        }"#)?;
        let (id, r) = record_of("p", &e, &[]).ok_or_else(|| anyhow::anyhow!("no record"))?;
        anyhow::ensure!(r.message == "card declined" && r.service == "checkout" && r.source == "gcp.cloud_run_revision" && r.severity == 17, "{r:?}");
        anyhow::ensure!(r.ts == 1_792_324_800_123_456_789, "{}", r.ts);
        anyhow::ensure!(r.trace_id.is_some_and(|t| t[0] == 0x01 && t[15] == 0xef) && r.span_id == Some([0, 0, 0, 0, 0, 0, 0, 0x4a]), "{:?} {:?}", r.trace_id, r.span_id);
        for want in [("gcp.log", "run.googleapis.com/stderr"), ("amount", "12"), ("retry", "false"), ("labels.instanceId", "00a"), ("gcp.resource.labels.service_name", "checkout")] {
            anyhow::ensure!(r.attrs.iter().any(|(k, v)| k == want.0 && v == want.1), "{want:?} in {:?}", r.attrs);
        }
        anyhow::ensure!(!r.attrs.iter().any(|(k, _)| k == "nested"), "only top-level values");
        anyhow::ensure!(record_of("p", &e, &[]).map(|x| x.0) == Some(id));
        let audit: Value = serde_json::from_str(r#"{"logName":"projects/p/logs/cloudaudit.googleapis.com%2Factivity","resource":{"type":"gce_instance","labels":{"instance_id":"77"}},"timestamp":"2026-10-18T12:00:01Z","insertId":"b","protoPayload":{"methodName":"v1.compute.instances.delete","resourceName":"projects/p/zones/z/instances/vm"}}"#)?;
        let (_, r) = record_of("p", &audit, &[]).ok_or_else(|| anyhow::anyhow!("no record"))?;
        anyhow::ensure!(r.message == "v1.compute.instances.delete projects/p/zones/z/instances/vm" && r.host == "77" && r.service == "cloudaudit.googleapis.com/activity" && r.severity == 9, "{r:?}");
        anyhow::ensure!(record_of("p", &serde_json::json!({"textPayload": "no time"}), &[]).is_none());
        Ok(())
    }

    #[test]
    fn types_are_named_as_their_service_and_path() {
        assert_eq!(metric_name("compute.googleapis.com/instance/cpu/utilization"), "gcp.compute.instance.cpu.utilization");
        assert_eq!(metric_name("kubernetes.io/container/cpu/core_usage_time"), "gcp.kubernetes.container.cpu.core_usage_time");
        assert_eq!(metric_name("workload.googleapis.com/http.server.duration"), "gcp.workload.http.server.duration");
        assert_eq!(metric_name("custom.googleapis.com/My Metric"), "gcp.custom.my_metric");
        assert_eq!(Read::of("CUMULATIVE", "INT64"), Some(Read::Delta));
        assert_eq!(Read::of("GAUGE", "DISTRIBUTION"), None);
        assert_eq!(Read::of("GAUGE", "STRING"), None);
    }

    #[test]
    fn quantiles_are_read_from_buckets_as_prometheus_reads_them() {
        // Explicit bounds 0 | 10 | 20 | 40, counts of the five buckets.
        let explicit = BucketOptions { explicit_buckets: Some(Explicit { bounds: vec![0.0, 10.0, 20.0, 40.0] }), ..BucketOptions::default() };
        let counts = [0.0, 10.0, 10.0, 0.0, 0.0];
        assert_eq!(quantile(&explicit, &counts, 0.5), Some(10.0));
        assert_eq!(quantile(&explicit, &counts, 0.75), Some(15.0));
        assert_eq!(quantile(&explicit, &[0.0, 0.0, 0.0, 0.0, 5.0], 0.99), Some(40.0), "the overflow bucket is its lower bound");
        assert_eq!(quantile(&explicit, &[0.0; 5], 0.5), None);
        // Linear: offset 0, width 5, 4 finite buckets.
        let linear = BucketOptions { linear_buckets: Some(Linear { num_finite_buckets: 4, width: 5.0, offset: 0.0 }), ..BucketOptions::default() };
        assert_eq!(quantile(&linear, &[0.0, 0.0, 4.0], 0.25), Some(6.25));
        // Exponential: scale 1, growth 10: bucket 2 is [10, 100); its middle is log-linear's 10^1.5.
        let exponential = BucketOptions { exponential_buckets: Some(Exponential { num_finite_buckets: 3, growth_factor: 10.0, scale: 1.0 }), ..BucketOptions::default() };
        let q = quantile(&exponential, &[0.0, 0.0, 2.0], 0.5).unwrap_or_default();
        assert!((q - 10f64.powf(1.5)).abs() < 1e-9, "{q}");
    }

    #[test]
    fn series_become_kiln_s_with_their_labels_and_distributions_their_six() -> anyhow::Result<()> {
        let page: SeriesPage = serde_json::from_str(
            r#"{"timeSeries":[
                {"metric":{"type":"compute.googleapis.com/instance/cpu/utilization","labels":{"instance_name":"vm-1"}},
                 "resource":{"type":"gce_instance","labels":{"project_id":"p","zone":"us-central1-a","instance_id":"123"}},
                 "metadata":{"systemLabels":{"name":"vm-1","preemptible":false,"network_tags":["a"]},"userLabels":{"env":"prod"}},
                 "metricKind":"GAUGE","valueType":"DOUBLE",
                 "points":[{"interval":{"startTime":"2026-10-18T12:01:00Z","endTime":"2026-10-18T12:01:00Z"},"value":{"doubleValue":0.25}},
                           {"interval":{"endTime":"2026-10-18T12:00:00Z"},"value":{"doubleValue":0.5}}]},
                {"metric":{"type":"loadbalancing.googleapis.com/https/total_latencies","labels":{"response_code":"200"}},
                 "resource":{"type":"https_lb_rule","labels":{"project_id":"p"}},
                 "metricKind":"DELTA","valueType":"DISTRIBUTION",
                 "points":[{"interval":{"endTime":"2026-10-18T12:00:00Z"},"value":{"distributionValue":{"count":"20","mean":15,
                   "bucketOptions":{"explicitBuckets":{"bounds":[0,10,20,40]}},"bucketCounts":["0","10","10"]}}}]}
            ]}"#,
        )?;
        let mut out = Pulled::default();
        let mut budget = 100;
        let tags = vec![("team".to_owned(), "infra".to_owned()), ("env".to_owned(), "never-this".to_owned())];
        let mut it = page.time_series.iter();
        if let Some(s) = it.next() {
            absorb("gcp.compute.instance.cpu.utilization", Read::Mean, s, &tags, &mut budget, &mut out);
        }
        if let Some(s) = it.next() {
            absorb("gcp.loadbalancing.https.total_latencies", Read::Distribution, s, &[], &mut budget, &mut out);
        }
        let t0 = parse_time("2026-10-18T12:00:00Z").unwrap_or_default();
        let got: Vec<(String, Vec<(String, String)>, Vec<(i64, f64)>)> = out
            .series
            .iter()
            .filter_map(|(k, p)| {
                let key = kiln_obs::batch::parse_key(&k.key).ok()?;
                Some((key.metric.to_owned(), key.tags.iter().map(|(a, b)| ((*a).to_owned(), (*b).to_owned())).collect(), p.clone()))
            })
            .collect();
        let first = got.first().ok_or_else(|| anyhow::anyhow!("a series"))?;
        let want_tags: Vec<(String, String)> = [("env", "prod"), ("instance_id", "123"), ("instance_name", "vm-1"), ("name", "vm-1"), ("preemptible", "false"), ("project_id", "p"), ("resource_type", "gce_instance"), ("team", "infra"), ("zone", "us-central1-a")].iter().map(|(a, b)| ((*a).to_owned(), (*b).to_owned())).collect();
        anyhow::ensure!(first.0 == "gcp.compute.instance.cpu.utilization" && first.1 == want_tags && first.2 == vec![(t0, 0.5), (t0 + 60_000, 0.25)], "{first:?}");
        let rest: Vec<(&str, &[(i64, f64)])> = got.iter().skip(1).map(|(m, _, p)| (m.as_str(), p.as_slice())).collect();
        anyhow::ensure!(
            rest == vec![
                ("gcp.loadbalancing.https.total_latencies.count", &[(t0, 20.0)][..]),
                ("gcp.loadbalancing.https.total_latencies.sum", &[(t0, 300.0)][..]),
                ("gcp.loadbalancing.https.total_latencies.avg", &[(t0, 15.0)][..]),
                ("gcp.loadbalancing.https.total_latencies.p50", &[(t0, 10.0)][..]),
                ("gcp.loadbalancing.https.total_latencies.p95", &[(t0, 19.0)][..]),
                ("gcp.loadbalancing.https.total_latencies.p99", &[(t0, 19.8)][..]),
            ],
            "{rest:?}"
        );
        // Past the budget, series are counted, not kept.
        let mut small = Pulled::default();
        let mut one = 1;
        if let Some(s) = page.time_series.get(1) {
            absorb("x", Read::Distribution, s, &[], &mut one, &mut small);
        }
        anyhow::ensure!(small.series.len() == 1 && small.over == 5);
        Ok(())
    }

    #[test]
    fn a_service_account_s_key_signs_a_token_request_google_would_verify() -> anyhow::Result<()> {
        use aws_lc_rs::encoding::AsDer as _;
        use aws_lc_rs::signature::KeyPair as _;
        let kp = aws_lc_rs::rsa::KeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048).map_err(|e| anyhow::anyhow!("{e}"))?;
        let der = kp.as_der().map_err(|e| anyhow::anyhow!("{e}"))?;
        let pem = format!("-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n", base64::engine::general_purpose::STANDARD.encode(der.as_ref()));
        let key = serde_json::json!({ "type": "service_account", "project_id": "p", "private_key_id": "k1", "private_key": pem, "client_email": "kiln@p.iam.gserviceaccount.com", "token_uri": "https://oauth2.googleapis.com/token" }).to_string();
        let Source::Account { email, key: pair, key_id, token_uri, project } = Source::of_key(&key)? else { anyhow::bail!("a service account") };
        anyhow::ensure!(project.as_deref() == Some("p"));
        let jwt = assertion(&email, &pair, key_id.as_deref(), &token_uri, SCOPE, 1_800_000_000)?;
        let parts: Vec<&str> = jwt.split('.').collect();
        let [head, claims, sig] = parts.as_slice() else { anyhow::bail!("three parts") };
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let public = aws_lc_rs::signature::UnparsedPublicKey::new(&aws_lc_rs::signature::RSA_PKCS1_2048_8192_SHA256, kp.public_key().as_ref());
        anyhow::ensure!(public.verify(format!("{head}.{claims}").as_bytes(), &b64.decode(sig)?).is_ok(), "the signature verifies");
        let c: Value = serde_json::from_slice(&b64.decode(claims)?)?;
        let claim = |k: &str| c.get(k).cloned().unwrap_or_default();
        anyhow::ensure!(claim("iss") == "kiln@p.iam.gserviceaccount.com" && claim("aud") == "https://oauth2.googleapis.com/token" && claim("scope") == SCOPE && claim("exp") == 1_800_003_600i64, "{c}");
        let h: Value = serde_json::from_slice(&b64.decode(head)?)?;
        anyhow::ensure!(h.get("alg") == Some(&Value::from("RS256")) && h.get("kid") == Some(&Value::from("k1")));
        anyhow::ensure!(Source::of_key(r#"{"type":"external_account"}"#).is_err());
        Ok(())
    }
}
