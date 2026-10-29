//! Amazon CloudWatch (`docs/observe.md` §10.1): each region's metrics of the namespaces asked,
//! those with points in the last 3 hours (`ListMetrics`, `RecentlyActive`), their statistics read a
//! period at a time (`GetMetricData`, 500 queries a call), over CloudWatch's JSON 1.0 protocol,
//! each call signed with Signature Version 4.
//!
//! Names are Datadog's: `aws.<namespace>.<metric>`, the namespace without `AWS/` in lower case and
//! the metric's words joined by `_` (`aws.ec2.cpuutilization`, `aws.ec2.network_in`,
//! `aws.sqs.approximate_number_of_messages_visible`), tagged `region` and each dimension by its
//! name in lower case (`instanceid`, `functionname`, `queuename`), then the integration's tags:
//! - `Sum` and `SampleCount`: `<name>.sum` and `<name>.samplecount`, counts (each period's);
//! - with both asked (the default) the average, `sum / samplecount`, is `<name>` itself, a gauge:
//!   CloudWatch keeps each period's count, sum, minimum and maximum, and its `Average` is that
//!   quotient, so it costs no query of its own;
//! - any other statistic (`Maximum`, `Minimum`, `p99`, `tm99`, ...) is `<name>.<statistic>` in
//!   lower case (`.maximum`, `.p99_9`), a gauge.
//!
//! Logs: the events of the log groups asked (`log_groups`: names, or prefixes ending in `*`, found
//! with `DescribeLogGroups`), each region's, matching `log_filter` (CloudWatch Logs' filter pattern)
//! when said, read over the lookback with `FilterLogEvents` (JSON 1.1, signed for `logs`). A record
//! is Datadog's forwarder's: `source` the AWS service the group is of (`/aws/lambda/…` `lambda`,
//! `/aws/rds/…` `rds`; else `cloudwatch`), `service` the group's last name (a function's), the group
//! and stream `aws.awslogs.logGroup` and `aws.awslogs.logStream`, and `region`; its id the event's.
//!
//! Credentials: the integration's keys, else the server's own as AWS's SDKs find them: the
//! environment (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`), the shared
//! credentials file (`AWS_SHARED_CREDENTIALS_FILE`, `AWS_PROFILE`), a web identity (EKS's IRSA:
//! `AWS_WEB_IDENTITY_TOKEN_FILE`, `AWS_ROLE_ARN`), the container's (ECS, EKS Pod Identity:
//! `AWS_CONTAINER_CREDENTIALS_RELATIVE_URI` or `_FULL_URI`), the instance's (EC2's IMDSv2). Either
//! may assume a role (`role_arn`, `external_id`), as Datadog's integration reads other accounts.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Instant;

use kiln_obs::batch::{Kind, SeriesKey};
use serde::{Deserialize, Deserializer, Serialize};
use time::OffsetDateTime;

use kiln_obs::logs::record::Record;

use super::sigv4::{self, Keys};
use super::{Answer, Api, Call, Common, DISCOVER_EVERY, METADATA_TIMEOUT, Pulled, REFRESH_MS, env, home, parse_time};

/// Queries a `GetMetricData` call holds at the most (CloudWatch's limit).
const QUERIES: usize = 500;
/// Events a `FilterLogEvents` call returns at the most (CloudWatch Logs' limit).
const EVENTS: u64 = 10_000;
/// Log groups read a region at the most: `FilterLogEvents` is held to 5 calls a second an account
/// and region (CloudWatch Logs' quota), so a hundred groups' pull takes some 20 s at the least.
pub const LOG_GROUPS: usize = 100;
/// Log records a pull takes at the most: the rest, the next pull's (and the status says so).
pub const PULL_RECORDS: usize = 200_000;
/// `ListMetrics`' `RecentlyActive`: metrics with points in the last 3 hours (the one value it
/// takes).
const RECENTLY: &str = "PT3H";
/// The session an assumed role or a web identity opens, as CloudTrail shows it.
const SESSION: &str = "kiln-observe";
const STS_VERSION: &str = "2011-06-15";
/// The instance metadata service, unless `AWS_EC2_METADATA_SERVICE_ENDPOINT` says another.
const IMDS: &str = "http://169.254.169.254";
/// The container credentials endpoint of ECS (`AWS_CONTAINER_CREDENTIALS_RELATIVE_URI`'s host).
const ECS: &str = "http://169.254.170.2";
/// Keys read from the environment or a file are read again this often (they may be rotated).
const REREAD_MS: i64 = 15 * 60_000;
/// botocore's throttling errors (its standard retry mode's).
const THROTTLING: [&str; 14] = [
    "Throttling",
    "ThrottlingException",
    "ThrottledException",
    "RequestThrottledException",
    "TooManyRequestsException",
    "ProvisionedThroughputExceededException",
    "TransactionInProgressException",
    "RequestLimitExceeded",
    "BandwidthLimitExceeded",
    "LimitExceededException",
    "RequestThrottled",
    "SlowDown",
    "PriorRequestNotComplete",
    "EC2ThrottledException",
];

fn stats() -> Vec<String> {
    vec!["Sum".into(), "SampleCount".into()]
}

fn period() -> u64 {
    60
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// Regions read; none: the server's own (`AWS_REGION`, `AWS_DEFAULT_REGION`, its instance's).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub regions: Vec<String>,
    /// Namespaces read (`AWS/EC2`, `AWS/Lambda`, `CWAgent`, ...); none: every one with metrics.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub namespaces: Vec<String>,
    /// Statistics asked of each metric.
    #[serde(default = "stats")]
    pub stats: Vec<String>,
    /// Seconds a point covers: 1, 5, 10, 30 or a multiple of 60.
    #[serde(default = "period")]
    pub period_s: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_key_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_access_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_token: Option<String>,
    /// A role assumed with those credentials (another account's).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_arn: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
    /// CloudWatch's and STS's endpoints, `{region}` replaced: another partition's, or a test's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sts_endpoint: Option<String>,
    /// Log groups whose events are read, in each region: names, or prefixes ending in `*`; none,
    /// no logs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub log_groups: Vec<String>,
    /// A filter pattern the events read match (CloudWatch Logs' syntax); none, every event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_filter: Option<String>,
    /// CloudWatch Logs' endpoint, `{region}` replaced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logs_endpoint: Option<String>,
    #[serde(flatten)]
    pub common: Common,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            regions: Vec::new(),
            namespaces: Vec::new(),
            stats: stats(),
            period_s: period(),
            access_key_id: None,
            secret_access_key: None,
            session_token: None,
            role_arn: None,
            external_id: None,
            endpoint: None,
            sts_endpoint: None,
            log_groups: Vec::new(),
            log_filter: None,
            logs_endpoint: None,
            common: Common::default(),
        }
    }
}

impl Config {
    pub fn check(&self) -> Result<(), String> {
        if let Some(r) = self.regions.iter().find(|r| r.is_empty() || r.len() > 32 || !r.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')) {
            return Err(format!("region {r:?}: as AWS writes them (us-east-1)"));
        }
        if let Some(n) = self.namespaces.iter().find(|n| n.is_empty() || n.len() > 255) {
            return Err(format!("namespace {n:?}: 1 to 255 characters"));
        }
        if self.stats.is_empty() {
            return Err("stats: at least one (Sum, SampleCount, Average, Minimum, Maximum, p99, ...)".into());
        }
        for (i, s) in self.stats.iter().enumerate() {
            if s.is_empty() || s.len() > 40 || !s.bytes().all(|b| b.is_ascii_alphanumeric() || b".%:()".contains(&b)) {
                return Err(format!("statistic {s:?}: as CloudWatch writes them (Sum, Maximum, p99, tm99, TM(10%:90%))"));
            }
            if self.stats.iter().take(i).any(|o| super::lower(o) == super::lower(s)) {
                return Err(format!("statistic {s:?} is asked twice"));
            }
        }
        if !(matches!(self.period_s, 1 | 5 | 10 | 30) || (self.period_s.is_multiple_of(60) && (60..=86_400).contains(&self.period_s))) {
            return Err(format!("period_s {}: 1, 5, 10, 30 or a multiple of 60 up to 86400", self.period_s));
        }
        if self.access_key_id.is_some() != self.secret_access_key.is_some() {
            return Err("an access key and its secret go together".into());
        }
        if self.session_token.is_some() && self.access_key_id.is_none() {
            return Err("a session token goes with an access key and its secret".into());
        }
        if self.role_arn.as_deref().is_some_and(|a| !a.starts_with("arn:")) {
            return Err("role_arn: a role's ARN (arn:aws:iam::123456789012:role/name)".into());
        }
        if self.external_id.is_some() && self.role_arn.is_none() {
            return Err("an external id goes with a role to assume".into());
        }
        for e in [&self.endpoint, &self.sts_endpoint, &self.logs_endpoint].into_iter().flatten() {
            if !(e.starts_with("https://") || e.starts_with("http://")) {
                return Err(format!("endpoint {e:?}: a URL"));
            }
        }
        // A log group's name: 1 to 512 of these (CloudWatch Logs'), a prefix ending in `*`.
        let named = |g: &str| {
            let n = g.strip_suffix('*').unwrap_or(g);
            (g.ends_with('*') || !n.is_empty()) && n.len() <= 512 && n.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-/.#".contains(&b))
        };
        if let Some(g) = self.log_groups.iter().find(|g| !named(g)) {
            return Err(format!("log group {g:?}: a name (/aws/lambda/checkout) or a prefix ending in * (/aws/lambda/*)"));
        }
        if self.log_groups.len() > LOG_GROUPS {
            return Err(format!("log_groups: {LOG_GROUPS} at the most"));
        }
        if self.log_filter.as_deref().is_some_and(|f| f.len() > 1024) {
            return Err("log_filter: 1024 characters at the most (CloudWatch Logs')".into());
        }
        if self.log_filter.is_some() && self.log_groups.is_empty() {
            return Err("a log filter goes with the log groups it filters".into());
        }
        Ok(())
    }

    fn logs(&self, region: &str) -> String {
        self.logs_endpoint.as_deref().map_or_else(|| format!("https://logs.{region}.{}/", domain(region)), |e| e.replace("{region}", region))
    }

    fn monitoring(&self, region: &str) -> String {
        self.endpoint.as_deref().map_or_else(|| format!("https://monitoring.{region}.{}/", domain(region)), |e| e.replace("{region}", region))
    }

    fn sts(&self, region: &str) -> String {
        self.sts_endpoint.as_deref().map_or_else(|| format!("https://sts.{region}.{}/", domain(region)), |e| e.replace("{region}", region))
    }
}

/// The partition's domain: China's regions are under `amazonaws.com.cn`.
fn domain(region: &str) -> &'static str {
    if region.starts_with("cn-") { "amazonaws.com.cn" } else { "amazonaws.com" }
}

/// A statistic's suffix: `Maximum` → `maximum`, `p99.9` → `p99_9`, `TM(10%:90%)` → `tm_10_90`.
pub(super) fn suffix(stat: &str) -> String {
    super::lower(stat)
}

fn kind_of(stat: &str) -> Kind {
    if stat == "Sum" || stat == "SampleCount" { Kind::Count } else { Kind::Gauge }
}

/// A metric's name: `aws.<namespace>.<metric>`.
pub(super) fn metric_name(namespace: &str, metric: &str) -> String {
    format!("aws.{}.{}", super::lower(namespace.strip_prefix("AWS/").unwrap_or(namespace)), super::snake(metric))
}

// ------------------------------------------------------------------------------------ the API

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Dimension {
    name: String,
    value: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Listed {
    namespace: Option<String>,
    metric_name: Option<String>,
    #[serde(default)]
    dimensions: Vec<Dimension>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ListOut {
    #[serde(default)]
    metrics: Vec<Listed>,
    next_token: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DataOut {
    #[serde(default)]
    metric_data_results: Vec<DataResult>,
    next_token: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DataResult {
    id: String,
    #[serde(default, deserialize_with = "times")]
    timestamps: Vec<i64>,
    #[serde(default)]
    values: Vec<f64>,
    status_code: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GroupsOut {
    #[serde(default)]
    log_groups: Vec<Group>,
    next_token: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Group {
    log_group_name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EventsOut {
    #[serde(default)]
    events: Vec<Event>,
    next_token: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Event {
    #[serde(default)]
    pub(super) event_id: String,
    #[serde(default)]
    pub(super) log_stream_name: String,
    pub(super) timestamp: i64,
    #[serde(default)]
    pub(super) message: String,
}

/// An event's record (the module's doc says how), its id.
pub(super) fn record_of(region: &str, group: &str, e: Event, tags: &[(String, String)]) -> (u64, Record) {
    let id = super::id_of(&["aws", region, group, &e.event_id]);
    let mut parts = group.split('/').filter(|p| !p.is_empty());
    let source = match (group.starts_with("/aws/"), parts.nth(1)) {
        (true, Some(svc)) => svc.to_ascii_lowercase(),
        _ => "cloudwatch".to_owned(),
    };
    let service = group.rsplit('/').find(|p| !p.is_empty()).unwrap_or(group).to_owned();
    let mut attrs: Vec<(String, String)> = Vec::with_capacity(3 + tags.len());
    attrs.push(("aws.awslogs.logGroup".into(), group.to_owned()));
    attrs.push(("aws.awslogs.logStream".into(), e.log_stream_name));
    attrs.push(("region".into(), region.to_owned()));
    attrs.extend(tags.iter().filter(|(k, _)| !attrs.iter().any(|(a, _)| a == k)).cloned().collect::<Vec<_>>());
    let severity = super::severity_of(&e.message);
    (id, Record { ts: e.timestamp.saturating_mul(1_000_000), severity, service, host: String::new(), source, message: super::message(e.message), attrs, trace_id: None, span_id: None })
}

/// Timestamps as JSON 1.0 gives them (epoch seconds, fractions and all) or as text (RFC 3339), in
/// unix ms.
fn times<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Vec<i64>, D::Error> {
    let raw: Vec<serde_json::Value> = Deserialize::deserialize(d)?;
    Ok(raw
        .iter()
        .filter_map(|v| match v {
            serde_json::Value::Number(n) => n.as_f64().map(|s| (s * 1000.0).round() as i64),
            serde_json::Value::String(s) => parse_time(s),
            _ => None,
        })
        .collect())
}

/// An error's code: what `x-amzn-query-error` says (`Throttling;Sender`), else `x-amzn-errortype`,
/// the JSON's `__type` (after its `#`), the XML's `<Code>`.
fn error_code(a: &Answer) -> String {
    if let Some(t) = &a.error_type {
        return t.split([';', ':']).next().unwrap_or_default().to_owned();
    }
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&a.body)
        && let Some(t) = v.get("__type").or_else(|| v.get("code")).and_then(serde_json::Value::as_str)
    {
        return t.rsplit('#').next().unwrap_or(t).to_owned();
    }
    xml(&String::from_utf8_lossy(&a.body), "Code").unwrap_or_default()
}

fn error(a: &Answer) -> String {
    let message = serde_json::from_slice::<serde_json::Value>(&a.body)
        .ok()
        .and_then(|v| v.get("message").or_else(|| v.get("Message")).and_then(serde_json::Value::as_str).map(str::to_owned))
        .or_else(|| xml(&String::from_utf8_lossy(&a.body), "Message"))
        .unwrap_or_else(|| a.text());
    let code = error_code(a);
    if code.is_empty() { format!("{} {message}", a.status) } else { format!("{} {code}: {message}", a.status) }
}

fn throttled(a: &Answer) -> bool {
    THROTTLING.contains(&error_code(a).as_str())
}

/// The text of the first `<tag>…</tag>` in `doc`, its entities read: what STS's answers hold.
fn xml(doc: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let start = doc.find(&open)? + open.len();
    let len = doc.get(start..)?.find(&format!("</{tag}>"))?;
    let raw = doc.get(start..start + len)?;
    Some(raw.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&"))
}

/// A call of CloudWatch Logs' `op` in `region`, signed.
async fn cloudwatch_logs(api: &Api<'_>, cfg: &Config, keys: &Keys, region: &str, op: &str, body: String) -> anyhow::Result<Answer> {
    let url = cfg.logs(region);
    let (host, path) = super::authority_and_path(&url)?;
    let target = format!("Logs_20140328.{op}");
    let ct = "application/x-amz-json-1.1";
    let headers = [("host", host.as_str()), ("content-type", ct), ("x-amz-target", target.as_str())];
    let signed = sigv4::sign(keys, region, "logs", &sigv4::Request { method: "POST", path: &path, query: &[], headers: &headers, body: body.as_bytes() }, OffsetDateTime::now_utc());
    let mut call = Call::post(&url, ct, body).header("x-amz-target", &target);
    for (k, v) in signed {
        call = call.header(&k, &v);
    }
    let a = api.call(&call, throttled).await?;
    if !a.ok() {
        anyhow::bail!("CloudWatch Logs {op} in {region}: {}", error(&a));
    }
    Ok(a)
}

/// A call of CloudWatch's `op` in `region`, signed.
async fn cloudwatch(api: &Api<'_>, cfg: &Config, keys: &Keys, region: &str, op: &str, body: String) -> anyhow::Result<Answer> {
    let url = cfg.monitoring(region);
    let (host, path) = super::authority_and_path(&url)?;
    let target = format!("GraniteServiceVersion20100801.{op}");
    let ct = "application/x-amz-json-1.0";
    let headers = [("host", host.as_str()), ("content-type", ct), ("x-amz-target", target.as_str())];
    let signed = sigv4::sign(keys, region, "monitoring", &sigv4::Request { method: "POST", path: &path, query: &[], headers: &headers, body: body.as_bytes() }, OffsetDateTime::now_utc());
    let mut call = Call::post(&url, ct, body).header("x-amz-target", &target);
    for (k, v) in signed {
        call = call.header(&k, &v);
    }
    let a = api.call(&call, throttled).await?;
    if !a.ok() {
        anyhow::bail!("CloudWatch {op} in {region}: {}", error(&a));
    }
    Ok(a)
}

// -------------------------------------------------------------------------------- credentials

/// Credentials as issued (the container's, the instance's).
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Issued {
    access_key_id: String,
    secret_access_key: String,
    token: Option<String>,
    expiration: Option<String>,
}

impl Issued {
    fn keys(self, now: i64) -> (Keys, i64) {
        let until = self.expiration.as_deref().and_then(parse_time).unwrap_or(now + REREAD_MS);
        (Keys { access_key: self.access_key_id, secret: self.secret_access_key, token: self.token.filter(|t| !t.is_empty()) }, until)
    }
}

/// The keys of `profile` in a shared credentials file.
fn profile_keys(text: &str, profile: &str) -> Option<Keys> {
    let (mut inside, mut id, mut secret, mut token) = (false, None, None, None);
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') || l.starts_with(';') {
            continue;
        }
        if let Some(name) = l.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
            inside = name.trim() == profile;
            continue;
        }
        if let (true, Some((k, v))) = (inside, l.split_once('=')) {
            let v = Some(v.trim().to_owned());
            match k.trim() {
                "aws_access_key_id" => id = v,
                "aws_secret_access_key" => secret = v,
                "aws_session_token" => token = v,
                _ => {}
            }
        }
    }
    Some(Keys { access_key: id?, secret: secret?, token })
}

/// The `<Credentials>` of an STS answer.
fn sts_keys(a: &Answer, now: i64) -> anyhow::Result<(Keys, i64)> {
    let doc = String::from_utf8_lossy(&a.body);
    let creds = xml(&doc, "Credentials").ok_or_else(|| anyhow::anyhow!("STS answered without credentials"))?;
    let field = |t: &str| xml(&creds, t).ok_or_else(|| anyhow::anyhow!("STS's credentials hold no {t}"));
    let until = xml(&creds, "Expiration").as_deref().and_then(parse_time).unwrap_or(now + REREAD_MS);
    Ok((Keys { access_key: field("AccessKeyId")?, secret: field("SecretAccessKey")?, token: xml(&creds, "SessionToken") }, until))
}

async fn sts(api: &Api<'_>, cfg: &Config, region: &str, params: &[(&str, &str)], signer: Option<&Keys>, now: i64) -> anyhow::Result<(Keys, i64)> {
    let url = cfg.sts(region);
    let body = super::form(params);
    let ct = "application/x-www-form-urlencoded; charset=utf-8";
    let mut call = Call::post(&url, ct, body.clone().into_bytes());
    if let Some(keys) = signer {
        let (host, path) = super::authority_and_path(&url)?;
        let headers = [("host", host.as_str()), ("content-type", ct)];
        for (k, v) in sigv4::sign(keys, region, "sts", &sigv4::Request { method: "POST", path: &path, query: &[], headers: &headers, body: body.as_bytes() }, OffsetDateTime::now_utc()) {
            call = call.header(&k, &v);
        }
    }
    let a = api.call(&call, throttled).await?;
    if !a.ok() {
        let action = params.iter().find(|(k, _)| *k == "Action").map_or("", |(_, v)| *v);
        anyhow::bail!("STS {action}: {}", error(&a));
    }
    sts_keys(&a, now)
}

async fn assume_role(api: &Api<'_>, cfg: &Config, base: &Keys, arn: &str, region: &str, now: i64) -> anyhow::Result<(Keys, i64)> {
    let mut params = vec![("Action", "AssumeRole"), ("Version", STS_VERSION), ("RoleArn", arn), ("RoleSessionName", SESSION), ("DurationSeconds", "3600")];
    if let Some(x) = &cfg.external_id {
        params.push(("ExternalId", x));
    }
    sts(api, cfg, region, &params, Some(base), now).await
}

fn imds_base() -> String {
    env("AWS_EC2_METADATA_SERVICE_ENDPOINT").map_or_else(|| IMDS.to_owned(), |e| e.trim_end_matches('/').to_owned())
}

/// A session token of the instance metadata service (IMDSv2), and what `path` holds.
async fn imds(api: &Api<'_>, paths: &[&str]) -> anyhow::Result<Vec<String>> {
    let base = imds_base();
    let url = format!("{base}/latest/api/token");
    let call = Call { method: crate::http::client::Method::Put, url: &url, headers: vec![("x-aws-ec2-metadata-token-ttl-seconds".into(), "21600".into())], body: bytes::Bytes::new(), timeout: METADATA_TIMEOUT };
    let a = api.once(&call).await?;
    anyhow::ensure!(a.ok(), "the instance metadata service: {} {}", a.status, a.text());
    let token = String::from_utf8_lossy(&a.body).trim().to_owned();
    let mut out = Vec::with_capacity(paths.len());
    let mut role = String::new();
    for p in paths {
        let path = p.replace("{role}", &role);
        let url = format!("{base}{path}");
        let a = api.once(&Call::get(&url).header("x-aws-ec2-metadata-token", &token).timeout(METADATA_TIMEOUT)).await?;
        anyhow::ensure!(a.ok(), "the instance metadata service, {path}: {} {}", a.status, a.text());
        let text = String::from_utf8_lossy(&a.body).trim().to_owned();
        if p.ends_with("security-credentials/") {
            role = text.lines().next().unwrap_or_default().trim().to_owned();
            anyhow::ensure!(!role.is_empty(), "the instance has no role");
        }
        out.push(text);
    }
    Ok(out)
}

/// The server's own credentials, as AWS's SDKs find them.
async fn ambient(api: &Api<'_>, cfg: &Config, region: &str, now: i64) -> anyhow::Result<(Keys, i64)> {
    if let (Some(id), Some(secret)) = (env("AWS_ACCESS_KEY_ID"), env("AWS_SECRET_ACCESS_KEY")) {
        return Ok((Keys { access_key: id, secret, token: env("AWS_SESSION_TOKEN") }, now + REREAD_MS));
    }
    let file = env("AWS_SHARED_CREDENTIALS_FILE").map(PathBuf::from).or_else(|| home().map(|h| h.join(".aws").join("credentials")));
    let profile = env("AWS_PROFILE").unwrap_or_else(|| "default".into());
    if let Some(k) = file.and_then(|f| std::fs::read_to_string(f).ok()).and_then(|t| profile_keys(&t, &profile)) {
        return Ok((k, now + REREAD_MS));
    }
    if let (Some(file), Some(arn)) = (env("AWS_WEB_IDENTITY_TOKEN_FILE"), env("AWS_ROLE_ARN")) {
        let token = std::fs::read_to_string(&file).map_err(|e| anyhow::anyhow!("the web identity token {file}: {e}"))?;
        let session = env("AWS_ROLE_SESSION_NAME").unwrap_or_else(|| SESSION.into());
        let params = [("Action", "AssumeRoleWithWebIdentity"), ("Version", STS_VERSION), ("RoleArn", arn.as_str()), ("RoleSessionName", session.as_str()), ("WebIdentityToken", token.trim()), ("DurationSeconds", "3600")];
        return sts(api, cfg, region, &params, None, now).await;
    }
    let container = env("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI").map(|r| format!("{ECS}{r}")).or_else(|| env("AWS_CONTAINER_CREDENTIALS_FULL_URI"));
    if let Some(url) = container {
        let auth = match env("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE") {
            Some(f) => Some(std::fs::read_to_string(&f).map_err(|e| anyhow::anyhow!("the container's authorization token {f}: {e}"))?.trim().to_owned()),
            None => env("AWS_CONTAINER_AUTHORIZATION_TOKEN"),
        };
        let mut call = Call::get(&url).timeout(METADATA_TIMEOUT);
        if let Some(t) = &auth {
            call = call.header("authorization", t);
        }
        let a = api.call(&call, |_| false).await?;
        anyhow::ensure!(a.ok(), "the container's credentials ({url}): {} {}", a.status, a.text());
        return Ok(a.json::<Issued>()?.keys(now));
    }
    if env("AWS_EC2_METADATA_DISABLED").is_some_and(|v| v.eq_ignore_ascii_case("true")) {
        anyhow::bail!("no AWS credentials: none given, none in the environment, the shared credentials file, a web identity or the container's, and the instance's are off (AWS_EC2_METADATA_DISABLED)");
    }
    let got = imds(api, &["/latest/meta-data/iam/security-credentials/", "/latest/meta-data/iam/security-credentials/{role}"]).await.map_err(|e| anyhow::anyhow!("no AWS credentials: none given, none in the environment, the shared credentials file, a web identity or the container's, nor the instance's ({e:#})"))?;
    let creds = got.get(1).ok_or_else(|| anyhow::anyhow!("the instance metadata service gave no credentials"))?;
    Ok(serde_json::from_str::<Issued>(creds).map_err(|e| anyhow::anyhow!("the instance's credentials: {e}"))?.keys(now))
}

/// A cloud's keys (its own, else the server's: [`ambient`]; a role assumed with them, when it
/// names one), kept until five minutes before they end: what its pulls sign with, and an archive's
/// objects (`crate::observe::archives`).
#[derive(Default)]
pub(crate) struct Creds {
    /// The keys signing calls, and until when (unix ms).
    held: Option<(Keys, i64)>,
}

impl Creds {
    pub(crate) async fn get(&mut self, api: &Api<'_>, cfg: &Config, region: &str, now: i64) -> anyhow::Result<Keys> {
        if let Some((k, until)) = &self.held
            && until.saturating_sub(now) > REFRESH_MS
        {
            return Ok(k.clone());
        }
        let (base, until) = match (&cfg.access_key_id, &cfg.secret_access_key) {
            (Some(id), Some(secret)) => (Keys { access_key: id.clone(), secret: secret.clone(), token: cfg.session_token.clone() }, i64::MAX),
            _ => ambient(api, cfg, region, now).await?,
        };
        let held = match &cfg.role_arn {
            Some(arn) => assume_role(api, cfg, &base, arn, region, now).await?,
            None => (base, until),
        };
        self.held = Some(held.clone());
        Ok(held.0)
    }
}

// --------------------------------------------------------------------------------------- pulls

/// A metric found, and the series it is pulled into.
#[derive(Debug)]
struct Found {
    /// The query's `Metric`, as JSON: its namespace, name and dimensions.
    metric: String,
    /// Each statistic's series, in the order `stats` says (none where it could not be made).
    keys: Vec<Option<SeriesKey>>,
    avg: Option<SeriesKey>,
}

#[derive(Debug)]
struct Region {
    name: String,
    found: Vec<Found>,
}

pub struct Puller {
    cfg: Config,
    tags: Vec<(String, String)>,
    creds: Creds,
    regions: Vec<String>,
    found: Vec<Region>,
    over: usize,
    discovered: Option<Instant>,
    /// The log groups read: (region, name).
    groups: Vec<(String, String)>,
}

impl Puller {
    pub fn new(cfg: Config) -> Self {
        let tags = cfg.common.tag_pairs();
        Self { regions: cfg.regions.clone(), cfg, tags, creds: Creds::default(), found: Vec::new(), over: 0, discovered: None, groups: Vec::new() }
    }

    /// The statistics' places of `Sum` and `SampleCount`, when both are asked: the average's.
    fn averaged(&self) -> Option<(usize, usize)> {
        Some((self.cfg.stats.iter().position(|s| s == "Sum")?, self.cfg.stats.iter().position(|s| s == "SampleCount")?))
    }

    pub async fn pull(&mut self, api: &Api<'_>, now: i64) -> anyhow::Result<Pulled> {
        if self.regions.is_empty() {
            self.regions = match env("AWS_REGION").or_else(|| env("AWS_DEFAULT_REGION")) {
                Some(r) => vec![r],
                None => imds(api, &["/latest/meta-data/placement/region"]).await.map_err(|e| anyhow::anyhow!("which regions? none said, no AWS_REGION, and the instance's metadata service did not say ({e:#})"))?,
            };
        }
        let home_region = self.regions.first().cloned().unwrap_or_else(|| "us-east-1".into());
        let keys = self.creds.get(api, &self.cfg, &home_region, now).await?;
        let mut warnings = Vec::new();
        if self.discovered.is_none_or(|t| t.elapsed() >= DISCOVER_EVERY) {
            match self.discover(api, &keys, &mut warnings).await {
                Ok(()) => self.discovered = Some(Instant::now()),
                Err(e) if self.discovered.is_some() => warnings.push(format!("metrics were not listed again ({e:#}); those listed before are read")),
                Err(e) => return Err(e),
            }
            if !self.cfg.log_groups.is_empty() {
                self.discover_logs(api, &keys, &mut warnings).await;
            }
        }
        let p = i64::try_from(self.cfg.period_s.saturating_mul(1000)).unwrap_or(60_000);
        let end = now.div_euclid(p) * p;
        let start = (end - self.cfg.common.lookback_ms()).div_euclid(p) * p;
        let mut out = Pulled { known: self.known(), over: self.over, ..Pulled::default() };
        let mut failed = 0;
        let mut first_error = None;
        for r in &self.found {
            match self.region(api, &keys, r, start, end).await {
                Ok(series) => out.series.extend(series),
                Err(e) => {
                    failed += 1;
                    warnings.push(format!("{}: {e:#}", r.name));
                    first_error.get_or_insert(e);
                }
            }
        }
        if failed > 0 && failed == self.found.len() {
            return Err(first_error.unwrap_or_else(|| anyhow::anyhow!("every region failed")));
        }
        if !self.cfg.log_groups.is_empty() {
            out.logs = self.read_logs(api, &keys, now.saturating_sub(self.cfg.common.lookback_ms()), now, &mut warnings).await;
        }
        out.warnings = warnings;
        Ok(out)
    }

    /// Each log group's events in `[start, end]` (unix ms), as records: a group that fails, a
    /// warning, the rest read.
    async fn read_logs(&self, api: &Api<'_>, keys: &Keys, start: i64, end: i64, warnings: &mut Vec<String>) -> Vec<(u64, Record)> {
        let mut out = Vec::new();
        for (region, group) in &self.groups {
            let mut next: Option<String> = None;
            loop {
                if out.len() >= PULL_RECORDS {
                    warnings.push(format!("{PULL_RECORDS} log records read, the most a pull takes: the rest are the next pull's"));
                    return out;
                }
                let mut body = serde_json::Map::new();
                body.insert("logGroupName".into(), group.as_str().into());
                body.insert("startTime".into(), start.into());
                body.insert("endTime".into(), end.into());
                body.insert("limit".into(), EVENTS.into());
                if let Some(f) = &self.cfg.log_filter {
                    body.insert("filterPattern".into(), f.as_str().into());
                }
                if let Some(t) = &next {
                    body.insert("nextToken".into(), t.as_str().into());
                }
                let page: EventsOut = match cloudwatch_logs(api, &self.cfg, keys, region, "FilterLogEvents", serde_json::Value::Object(body).to_string()).await.and_then(|a| a.json()) {
                    Ok(p) => p,
                    Err(e) => {
                        warnings.push(format!("{region} {group}: {e:#}"));
                        break;
                    }
                };
                out.extend(page.events.into_iter().map(|e| record_of(region, group, e, &self.tags)));
                // A page may hold nothing and still go on: CloudWatch Logs searches in parts.
                match page.next_token.filter(|t| !t.is_empty()) {
                    Some(t) if next.as_ref() != Some(&t) => next = Some(t),
                    _ => break,
                }
            }
        }
        out
    }

    /// The log groups the settings name, in each region: a prefix's from `DescribeLogGroups`.
    async fn discover_logs(&mut self, api: &Api<'_>, keys: &Keys, warnings: &mut Vec<String>) {
        let mut groups = Vec::new();
        for region in &self.regions {
            let mut here: Vec<String> = Vec::new();
            for g in &self.cfg.log_groups {
                let Some(prefix) = g.strip_suffix('*') else {
                    here.push(g.clone());
                    continue;
                };
                let mut next: Option<String> = None;
                loop {
                    let mut body = serde_json::Map::new();
                    if !prefix.is_empty() {
                        body.insert("logGroupNamePrefix".into(), prefix.into());
                    }
                    if let Some(t) = &next {
                        body.insert("nextToken".into(), t.as_str().into());
                    }
                    let page: GroupsOut = match cloudwatch_logs(api, &self.cfg, keys, region, "DescribeLogGroups", serde_json::Value::Object(body).to_string()).await.and_then(|a| a.json()) {
                        Ok(p) => p,
                        Err(e) => {
                            warnings.push(format!("{region}: log groups {g:?} were not listed ({e:#})"));
                            break;
                        }
                    };
                    here.extend(page.log_groups.into_iter().map(|l| l.log_group_name));
                    match page.next_token.filter(|t| !t.is_empty()) {
                        Some(t) if next.as_ref() != Some(&t) => next = Some(t),
                        _ => break,
                    }
                }
            }
            here.sort();
            here.dedup();
            if here.len() > LOG_GROUPS {
                warnings.push(format!("{region}: {} log groups, {LOG_GROUPS} read (CloudWatch Logs' quota of 5 searches a second)", here.len()));
                here.truncate(LOG_GROUPS);
            }
            groups.extend(here.into_iter().map(|g| (region.clone(), g)));
        }
        self.groups = groups;
    }

    fn known(&self) -> usize {
        self.found.iter().flat_map(|r| &r.found).map(|f| f.keys.iter().flatten().count() + usize::from(f.avg.is_some())).sum()
    }

    /// Lists each region's metrics, keeping those that fit under `max_series`.
    async fn discover(&mut self, api: &Api<'_>, keys: &Keys, warnings: &mut Vec<String>) -> anyhow::Result<()> {
        let mut listed: Vec<(String, Vec<Listed>)> = Vec::with_capacity(self.regions.len());
        let mut first_error = None;
        for region in &self.regions {
            match self.list(api, keys, region).await {
                Ok(l) => listed.push((region.clone(), l)),
                Err(e) => {
                    warnings.push(format!("{region}: {e:#}"));
                    first_error.get_or_insert(e);
                }
            }
        }
        if listed.is_empty()
            && let Some(e) = first_error
        {
            return Err(e);
        }
        let (found, over) = found(listed, &self.cfg, &self.tags, self.averaged());
        self.found = found;
        self.over = over;
        Ok(())
    }

    async fn list(&self, api: &Api<'_>, keys: &Keys, region: &str) -> anyhow::Result<Vec<Listed>> {
        let namespaces: Vec<Option<&str>> = if self.cfg.namespaces.is_empty() { vec![None] } else { self.cfg.namespaces.iter().map(|n| Some(n.as_str())).collect() };
        let mut out = Vec::new();
        for ns in namespaces {
            let mut next: Option<String> = None;
            loop {
                let mut body = serde_json::Map::new();
                body.insert("RecentlyActive".into(), RECENTLY.into());
                if let Some(n) = ns {
                    body.insert("Namespace".into(), n.into());
                }
                if let Some(t) = &next {
                    body.insert("NextToken".into(), t.clone().into());
                }
                let a = cloudwatch(api, &self.cfg, keys, region, "ListMetrics", serde_json::Value::Object(body).to_string()).await?;
                let page: ListOut = a.json()?;
                out.extend(page.metrics);
                match page.next_token.filter(|t| !t.is_empty()) {
                    Some(t) if next.as_ref() != Some(&t) => next = Some(t),
                    _ => break,
                }
            }
        }
        Ok(out)
    }

    /// One region's statistics over `[start, end)`.
    async fn region(&self, api: &Api<'_>, keys: &Keys, r: &Region, start: i64, end: i64) -> anyhow::Result<Vec<(SeriesKey, Vec<(i64, f64)>)>> {
        let stats = self.cfg.stats.len();
        let queries: Vec<usize> = (0..r.found.len() * stats).filter(|&q| r.found.get(q / stats).is_some_and(|f| f.keys.get(q % stats).is_some_and(Option::is_some))).collect();
        let mut points: Vec<Vec<(i64, f64)>> = vec![Vec::new(); r.found.len() * stats];
        for part in queries.chunks(QUERIES) {
            let body = data_request(r, &self.cfg, part, start, end);
            let mut next: Option<String> = None;
            loop {
                let mut b = body.clone();
                if let Some(t) = &next {
                    b.pop();
                    let _ = write!(b, ",\"NextToken\":{}}}", serde_json::Value::from(t.as_str()));
                }
                let a = cloudwatch(api, &self.cfg, keys, &r.name, "GetMetricData", b).await?;
                let page: DataOut = a.json()?;
                absorb(&page, &mut points);
                match page.next_token.filter(|t| !t.is_empty()) {
                    Some(t) if next.as_ref() != Some(&t) => next = Some(t),
                    _ => break,
                }
            }
        }
        Ok(assemble(r, points, stats, self.averaged()))
    }
}

/// The metrics listed, by region, as what is pulled: sorted, the same metric once, as many as fit
/// under `max_series` (and how many series did not fit).
fn found(listed: Vec<(String, Vec<Listed>)>, cfg: &Config, tags: &[(String, String)], averaged: Option<(usize, usize)>) -> (Vec<Region>, usize) {
    let each = cfg.stats.len() + usize::from(averaged.is_some());
    let mut budget = cfg.common.max_series;
    let mut over = 0;
    let mut regions = Vec::with_capacity(listed.len());
    for (region, mut metrics) in listed {
        metrics.retain(|m| m.namespace.as_deref().is_some_and(|n| !n.is_empty()) && m.metric_name.as_deref().is_some_and(|n| !n.is_empty()));
        for m in &mut metrics {
            m.dimensions.sort_by(|a, b| a.name.cmp(&b.name));
        }
        metrics.sort_by(|a, b| (&a.namespace, &a.metric_name, &a.dimensions).cmp(&(&b.namespace, &b.metric_name, &b.dimensions)));
        metrics.dedup_by(|a, b| a.namespace == b.namespace && a.metric_name == b.metric_name && a.dimensions == b.dimensions);
        let mut found = Vec::with_capacity(metrics.len().min(budget / each.max(1)));
        for m in metrics {
            if budget < each {
                over += each;
                continue;
            }
            budget -= each;
            let (ns, name) = (m.namespace.unwrap_or_default(), m.metric_name.unwrap_or_default());
            let base = metric_name(&ns, &name);
            let dims: Vec<(String, &str)> = m.dimensions.iter().map(|d| (d.name.to_ascii_lowercase(), d.value.as_str())).collect();
            let mut pairs: Vec<(&str, &str)> = dims.iter().map(|(k, v)| (k.as_str(), *v)).collect();
            pairs.push(("region", region.as_str()));
            pairs.extend(tags.iter().map(|(k, v)| (k.as_str(), v.as_str())));
            let keys = cfg.stats.iter().map(|s| super::key(&format!("{base}.{}", suffix(s)), &pairs, kind_of(s))).collect();
            let avg = averaged.and_then(|_| super::key(&base, &pairs, Kind::Gauge));
            let metric = serde_json::json!({ "Namespace": ns, "MetricName": name, "Dimensions": &m.dimensions }).to_string();
            found.push(Found { metric, keys, avg });
        }
        regions.push(Region { name: region, found });
    }
    (regions, over)
}

/// A `GetMetricData` request of `queries` (each `metric × stats + stat`), each query's id `q<n>`.
fn data_request(r: &Region, cfg: &Config, queries: &[usize], start: i64, end: i64) -> String {
    let stats = cfg.stats.len();
    let mut b = String::with_capacity(queries.len() * 220 + 120);
    b.push_str("{\"MetricDataQueries\":[");
    let mut first = true;
    for &q in queries {
        let (Some(f), Some(stat)) = (r.found.get(q / stats), cfg.stats.get(q % stats)) else { continue };
        if !first {
            b.push(',');
        }
        first = false;
        let _ = write!(b, "{{\"Id\":\"q{q}\",\"MetricStat\":{{\"Metric\":{},\"Period\":{},\"Stat\":{}}},\"ReturnData\":true}}", f.metric, cfg.period_s, serde_json::Value::from(stat.as_str()));
    }
    let _ = write!(b, "],\"StartTime\":{},\"EndTime\":{},\"ScanBy\":\"TimestampAscending\"}}", start / 1000, end / 1000);
    b
}

/// A page's results into each query's points.
fn absorb(page: &DataOut, points: &mut [Vec<(i64, f64)>]) {
    for r in &page.metric_data_results {
        if r.status_code.as_deref().is_some_and(|s| s != "Complete" && s != "PartialData") {
            continue;
        }
        let Some(q) = r.id.strip_prefix('q').and_then(|n| n.parse::<usize>().ok()) else { continue };
        if let Some(p) = points.get_mut(q) {
            p.extend(r.timestamps.iter().copied().zip(r.values.iter().copied()).filter(|(_, v)| v.is_finite()));
        }
    }
}

/// Each query's points as its series', and the averages of those with both a sum and a count.
fn assemble(r: &Region, mut points: Vec<Vec<(i64, f64)>>, stats: usize, averaged: Option<(usize, usize)>) -> Vec<(SeriesKey, Vec<(i64, f64)>)> {
    let mut out = Vec::with_capacity(r.found.len() * (stats + 1));
    for p in &mut points {
        p.sort_by_key(|s| s.0);
        p.dedup_by_key(|s| s.0);
    }
    for (m, f) in r.found.iter().enumerate() {
        if let (Some(avg), Some((sum, count))) = (&f.avg, averaged) {
            let (sums, counts) = (points.get(m * stats + sum), points.get(m * stats + count));
            if let (Some(sums), Some(counts)) = (sums, counts) {
                let mut a = Vec::with_capacity(sums.len().min(counts.len()));
                let (mut i, mut j) = (0, 0);
                while let (Some(&(ts, s)), Some(&(tc, c))) = (sums.get(i), counts.get(j)) {
                    match ts.cmp(&tc) {
                        std::cmp::Ordering::Less => i += 1,
                        std::cmp::Ordering::Greater => j += 1,
                        std::cmp::Ordering::Equal => {
                            if c > 0.0 {
                                a.push((ts, s / c));
                            }
                            i += 1;
                            j += 1;
                        }
                    }
                }
                if !a.is_empty() {
                    out.push((avg.clone(), a));
                }
            }
        }
        for (s, key) in f.keys.iter().enumerate() {
            if let (Some(key), Some(p)) = (key, points.get_mut(m * stats + s))
                && !p.is_empty()
            {
                out.push((key.clone(), std::mem::take(p)));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_log_event_is_a_record_as_datadog_s_forwarder_makes_it() -> anyhow::Result<()> {
        let tags = vec![("team".to_owned(), "pay".to_owned())];
        let e = Event { event_id: "3721".into(), log_stream_name: "2026/10/18/[$LATEST]ab".into(), timestamp: 1_800_000_000_123, message: "2026-10-18T12:00:00Z\tid\tERROR\tcard declined".into() };
        let (id, r) = record_of("eu-west-1", "/aws/lambda/checkout", e, &tags);
        anyhow::ensure!(r.source == "lambda" && r.service == "checkout" && r.severity == 17 && r.ts == 1_800_000_000_123_000_000, "{r:?}");
        anyhow::ensure!(r.attrs.contains(&("aws.awslogs.logGroup".into(), "/aws/lambda/checkout".into())) && r.attrs.contains(&("region".into(), "eu-west-1".into())) && r.attrs.contains(&("team".into(), "pay".into())), "{:?}", r.attrs);
        let again = Event { event_id: "3721".into(), log_stream_name: String::new(), timestamp: 0, message: String::new() };
        anyhow::ensure!(record_of("eu-west-1", "/aws/lambda/checkout", again, &tags).0 == id, "an event's id is its own, whatever else is read");
        let other = Event { event_id: "1".into(), log_stream_name: "i-0abc".into(), timestamp: 1, message: "plain".into() };
        let (_, r) = record_of("us-east-1", "app/web", other, &[]);
        anyhow::ensure!(r.source == "cloudwatch" && r.service == "web" && r.severity == 9, "{r:?}");
        let page: EventsOut = serde_json::from_str(r#"{"events":[{"eventId":"9","logStreamName":"s","timestamp":5,"message":"m","ingestionTime":6}],"nextToken":"t","searchedLogStreams":[]}"#)?;
        anyhow::ensure!(page.events.len() == 1 && page.next_token.as_deref() == Some("t"));
        Ok(())
    }

    #[test]
    fn log_settings_are_checked() {
        let with = |groups: &[&str], filter: Option<&str>| Config { log_groups: groups.iter().map(|g| (*g).to_owned()).collect(), log_filter: filter.map(str::to_owned), ..Config::default() }.check();
        assert!(with(&["/aws/lambda/checkout", "/aws/rds/*", "*"], Some("ERROR")).is_ok());
        assert!(with(&["bad name"], None).is_err());
        assert!(with(&[""], None).is_err());
        assert!(with(&[], Some("ERROR")).is_err(), "a filter without groups");
        assert!(with(&vec!["g"; LOG_GROUPS + 1], None).is_err());
    }

    fn listed(ns: &str, name: &str, dims: &[(&str, &str)]) -> Listed {
        Listed { namespace: Some(ns.into()), metric_name: Some(name.into()), dimensions: dims.iter().map(|(n, v)| Dimension { name: (*n).into(), value: (*v).into() }).collect() }
    }

    fn names(series: &[(SeriesKey, Vec<(i64, f64)>)]) -> Vec<(String, Vec<(String, String)>, Vec<(i64, f64)>)> {
        let mut v: Vec<_> = series
            .iter()
            .filter_map(|(k, p)| {
                let key = kiln_obs::batch::parse_key(&k.key).ok()?;
                Some((key.metric.to_owned(), key.tags.iter().map(|(a, b)| ((*a).to_owned(), (*b).to_owned())).collect(), p.clone()))
            })
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    #[test]
    fn statistics_become_datadog_s_series_and_the_average_is_the_sum_over_the_count() -> anyhow::Result<()> {
        let cfg = Config { stats: vec!["Sum".into(), "SampleCount".into(), "Maximum".into(), "p99.9".into()], common: Common { tags: vec!["env:prod".into()], ..Common::default() }, ..Config::default() };
        let tags = cfg.common.tag_pairs();
        let averaged = Some((0, 1));
        let (regions, over) = found(vec![("us-east-1".into(), vec![listed("AWS/EC2", "CPUUtilization", &[("InstanceId", "i-1")]), listed("AWS/EC2", "CPUUtilization", &[("InstanceId", "i-1")])])], &cfg, &tags, averaged);
        anyhow::ensure!(over == 0 && regions.first().map(|r| r.found.len()) == Some(1), "the same metric listed twice is pulled once");
        let r = regions.first().ok_or_else(|| anyhow::anyhow!("a region"))?;
        let body: serde_json::Value = serde_json::from_str(&data_request(r, &cfg, &[0, 1, 2, 3], 1_800_000_000_000, 1_800_000_900_000))?;
        let at = |p: &str| body.pointer(p).cloned().unwrap_or_default();
        anyhow::ensure!(at("/MetricDataQueries/2/Id") == "q2" && at("/MetricDataQueries/2/MetricStat/Stat") == "Maximum" && at("/MetricDataQueries/0/MetricStat/Metric/Dimensions/0/Name") == "InstanceId", "{body}");
        anyhow::ensure!(at("/StartTime") == 1_800_000_000i64 && at("/EndTime") == 1_800_000_900i64 && at("/ScanBy") == "TimestampAscending");
        // Results as CloudWatch's JSON gives them: epoch seconds; a period without a count is no average.
        let page: DataOut = serde_json::from_str(
            r#"{"MetricDataResults":[
                {"Id":"q0","Label":"Sum","Timestamps":[1800000060.0,1800000000],"Values":[30.0,12.0],"StatusCode":"Complete"},
                {"Id":"q1","Timestamps":[1800000000,1800000060],"Values":[4,0],"StatusCode":"Complete"},
                {"Id":"q2","Timestamps":["2027-01-15T08:00:00Z"],"Values":[9.5],"StatusCode":"PartialData"},
                {"Id":"q3","Timestamps":[1800000000],"Values":[7],"StatusCode":"Forbidden"},
                {"Id":"zz","Timestamps":[1800000000],"Values":[1]}
            ],"Messages":[]}"#,
        )?;
        let mut points = vec![Vec::new(); 4];
        absorb(&page, &mut points);
        let got = names(&assemble(r, points, 4, averaged));
        let tags = |extra: &[(&str, &str)]| {
            let mut t: Vec<(String, String)> = [("env", "prod"), ("instanceid", "i-1"), ("region", "us-east-1")].iter().chain(extra).map(|(a, b)| ((*a).to_owned(), (*b).to_owned())).collect();
            t.sort();
            t
        };
        let want = vec![
            ("aws.ec2.cpuutilization".to_owned(), tags(&[]), vec![(1_800_000_000_000, 3.0)]),
            ("aws.ec2.cpuutilization.maximum".to_owned(), tags(&[]), vec![(parse_time("2027-01-15T08:00:00Z").unwrap_or_default(), 9.5)]),
            ("aws.ec2.cpuutilization.samplecount".to_owned(), tags(&[]), vec![(1_800_000_000_000, 4.0), (1_800_000_060_000, 0.0)]),
            ("aws.ec2.cpuutilization.sum".to_owned(), tags(&[]), vec![(1_800_000_000_000, 12.0), (1_800_000_060_000, 30.0)]),
        ];
        anyhow::ensure!(got == want, "{got:#?}");
        Ok(())
    }

    #[test]
    fn past_max_series_metrics_are_left_out_whole_and_counted() {
        let cfg = Config { common: Common { max_series: 7, ..Common::default() }, ..Config::default() };
        let many: Vec<Listed> = (0..5).map(|i| listed("AWS/Lambda", "Invocations", &[("FunctionName", &format!("f{i}"))])).collect();
        let (regions, over) = found(vec![("eu-west-1".into(), many)], &cfg, &[], Some((0, 1)));
        // Three series a metric (sum, count, average): two metrics fit in seven, three left out.
        assert_eq!(regions.first().map(|r| r.found.len()), Some(2));
        assert_eq!(over, 9);
    }

    #[test]
    fn names_tags_and_statistics_read_as_datadog_s() {
        assert_eq!(metric_name("AWS/ApplicationELB", "HTTPCode_Target_5XX_Count"), "aws.applicationelb.httpcode_target_5xx_count");
        assert_eq!(metric_name("CWAgent", "mem_used_percent"), "aws.cwagent.mem_used_percent");
        assert_eq!(metric_name("AWS/SQS", "ApproximateNumberOfMessagesVisible"), "aws.sqs.approximate_number_of_messages_visible");
        assert_eq!(suffix("p99.9"), "p99_9");
        assert_eq!(suffix("TM(10%:90%)"), "tm_10_90");
        assert_eq!(kind_of("Sum"), Kind::Count);
        assert_eq!(kind_of("Maximum"), Kind::Gauge);
    }

    #[test]
    fn credentials_files_sts_answers_and_errors_are_read() -> anyhow::Result<()> {
        let file = "[default]\naws_access_key_id = AKIDDEFAULT\naws_secret_access_key=s1\n\n# comment\n[ops]\naws_access_key_id=AKIDOPS\naws_secret_access_key = s2\naws_session_token = t2\n";
        anyhow::ensure!(profile_keys(file, "default") == Some(Keys { access_key: "AKIDDEFAULT".into(), secret: "s1".into(), token: None }));
        anyhow::ensure!(profile_keys(file, "ops") == Some(Keys { access_key: "AKIDOPS".into(), secret: "s2".into(), token: Some("t2".into()) }));
        anyhow::ensure!(profile_keys(file, "none").is_none());
        // STS's AssumeRole answer, as its documentation shows one.
        let body = r#"<AssumeRoleResponse xmlns="https://sts.amazonaws.com/doc/2011-06-15/">
  <AssumeRoleResult>
    <AssumedRoleUser><Arn>arn:aws:sts::123456789012:assumed-role/demo/kiln-observe</Arn><AssumedRoleId>ARO123EXAMPLE123:kiln-observe</AssumedRoleId></AssumedRoleUser>
    <Credentials>
      <AccessKeyId>ASIAIOSFODNN7EXAMPLE</AccessKeyId>
      <SecretAccessKey>wJalrXUtnFEMI/K7MDENG/bPxRfiCYzEXAMPLEKEY</SecretAccessKey>
      <SessionToken>AQoDYXdzEPT//////////wEXAMPLEtc764&amp;bNrC</SessionToken>
      <Expiration>2019-11-09T13:34:41Z</Expiration>
    </Credentials>
  </AssumeRoleResult>
</AssumeRoleResponse>"#;
        let a = Answer { status: 200, body: body.into(), retry_after: None, error_type: None };
        let (k, until) = sts_keys(&a, 0)?;
        anyhow::ensure!(k.access_key == "ASIAIOSFODNN7EXAMPLE" && k.token.as_deref() == Some("AQoDYXdzEPT//////////wEXAMPLEtc764&bNrC") && until == parse_time("2019-11-09T13:34:41Z").unwrap_or_default());
        let denied = Answer { status: 403, body: "<ErrorResponse><Error><Type>Sender</Type><Code>AccessDenied</Code><Message>not authorized to perform sts:AssumeRole</Message></Error></ErrorResponse>".into(), retry_after: None, error_type: None };
        anyhow::ensure!(error(&denied) == "403 AccessDenied: not authorized to perform sts:AssumeRole" && !throttled(&denied), "{}", error(&denied));
        let slow = Answer { status: 400, body: r#"{"__type":"com.amazonaws.cloudwatch#Throttling","message":"Rate exceeded"}"#.into(), retry_after: None, error_type: Some("Throttling;Sender".into()) };
        anyhow::ensure!(throttled(&slow) && error(&slow) == "400 Throttling: Rate exceeded");
        let json_only = Answer { status: 400, body: r#"{"__type":"com.amazonaws.cloudwatch#InvalidParameterValueException","message":"bad"}"#.into(), retry_after: None, error_type: None };
        anyhow::ensure!(error(&json_only) == "400 InvalidParameterValueException: bad" && !throttled(&json_only));
        Ok(())
    }

    #[test]
    fn settings_are_checked() {
        let ok = Config { regions: vec!["us-east-1".into()], ..Config::default() };
        assert_eq!(ok.check(), Ok(()));
        for bad in [
            Config { regions: vec!["US East".into()], ..Config::default() },
            Config { stats: vec![], ..Config::default() },
            Config { stats: vec!["Sum".into(), "sum".into()], ..Config::default() },
            Config { period_s: 90, ..Config::default() },
            Config { access_key_id: Some("A".into()), ..Config::default() },
            Config { role_arn: Some("role".into()), ..Config::default() },
            Config { external_id: Some("x".into()), ..Config::default() },
            Config { endpoint: Some("monitoring".into()), ..Config::default() },
        ] {
            assert!(bad.check().is_err(), "{bad:?}");
        }
        assert_eq!(ok.monitoring("cn-north-1"), "https://monitoring.cn-north-1.amazonaws.com.cn/");
        assert_eq!(Config { endpoint: Some("http://127.0.0.1:9/{region}".into()), ..Config::default() }.monitoring("eu-west-1"), "http://127.0.0.1:9/eu-west-1");
    }
}
