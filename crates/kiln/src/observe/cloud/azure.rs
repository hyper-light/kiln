//! Azure Monitor (`docs/observe.md` §10.1): each subscription's resources (Resource Manager's list,
//! of the types asked), each type's metrics (`metricDefinitions`, read once a type each discovery),
//! each resource's points a period at a time (`metrics`, 20 metrics a call).
//!
//! Names: `azure.<type>.<metric>`, the resource type without `Microsoft.` in lower case, its `/` and
//! `.` as `_`, the metric's words by `_` (`azure.compute_virtualmachines.percentage_cpu`,
//! `azure.web_sites.http5xx`, `azure.sql_servers_databases.cpu_percent`), tagged
//! `subscription_id`, `resource_group`, `name` and `region`, then the resource's own tags (keys in
//! lower case), then the integration's:
//! - a metric's primary aggregation (its definition's) is `<name>` itself: `Total` and `Count`
//!   counts (each period's), `Average`, `Minimum` and `Maximum` gauges;
//! - each aggregation asked besides (`aggregations`) is `<name>.<aggregation>` in lower case.
//!
//! A metric is read at the period, else at the finest time grain it has above it (a storage
//! account's capacity is hourly); metrics that need a dimension to be read are not.
//!
//! Logs: each Log Analytics workspace's rows (`workspaces`, their ids) of `log_query` (a table,
//! `AzureActivity`, or a query in KQL ending in rows; none, every table), read over the lookback
//! oldest first with the Log Analytics query API (its own token, for its own audience), 200,000
//! rows a workspace a pull at the most. A record: `source` `azure.<table>`, `service` the resource's
//! name (`_ResourceId`'s last) else the table, `host` `Computer`, the message the row's `Message`,
//! `RenderedDescription`, `Description`, `ResultDescription` or `OperationName` (else the row), its
//! level `Level`'s, `SeverityLevel`'s (Application Insights': 0 verbose to 4 critical) or the
//! message's, the row's other values attributes; its id `_ItemId`, else the row's values.
//!
//! Credentials: the integration's app (`tenant_id`, `client_id`, `client_secret`), else the
//! server's own as Azure's SDKs find them: the environment's app (`AZURE_TENANT_ID`,
//! `AZURE_CLIENT_ID`, `AZURE_CLIENT_SECRET`), a workload identity (AKS: `AZURE_FEDERATED_TOKEN_FILE`),
//! App Service's and Functions' managed identity (`IDENTITY_ENDPOINT`, `IDENTITY_HEADER`), the VM's
//! (its metadata service; a user-assigned identity by `client_id` or `AZURE_CLIENT_ID`).

use std::collections::BTreeMap;
use std::time::Instant;

use kiln_obs::batch::{Kind, SeriesKey};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use kiln_obs::logs::record::Record;

use super::{Answer, Api, Call, Common, DISCOVER_EVERY, METADATA_TIMEOUT, Pulled, REFRESH_MS, encode, env, number, parse_time};

const LOGIN: &str = "https://login.microsoftonline.com";
const ARM: &str = "https://management.azure.com";
/// Log Analytics' API, and its token's audience.
const LOGS: &str = "https://api.loganalytics.io";
/// Rows a workspace's pull takes at the most (the query API answers 500,000 at the most).
pub const LOG_ROWS: usize = 200_000;
/// A record's attributes from its row's other columns, at the most.
const ROW_ATTRS: usize = 32;
/// The VM's metadata service, unless `AZURE_POD_IDENTITY_AUTHORITY_HOST` says another (as Azure's
/// SDKs read it).
const IMDS: &str = "http://169.254.169.254";
/// Metrics a `metrics` call names at the most (Azure Monitor's limit).
const NAMES: usize = 20;
/// Resources read at once.
const CONCURRENCY: usize = 4;
/// Warnings a pull keeps.
const WARNINGS: usize = 20;
/// Azure Monitor's time grains, in seconds.
const GRAINS: [(u64, &str); 8] = [(60, "PT1M"), (300, "PT5M"), (900, "PT15M"), (1800, "PT30M"), (3600, "PT1H"), (21_600, "PT6H"), (43_200, "PT12H"), (86_400, "P1D")];

fn period() -> u64 {
    60
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    /// Subscriptions read; none: every one the credentials see.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subscriptions: Vec<String>,
    /// Resource types read (`Microsoft.Compute/virtualMachines`, ...); none: every type with metrics.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resource_types: Vec<String>,
    /// Aggregations read besides each metric's primary one: `Average`, `Total`, `Count`, `Minimum`,
    /// `Maximum`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aggregations: Vec<String>,
    /// Seconds a point covers: an Azure time grain (60, 300, 900, 1800, 3600, 21600, 43200, 86400).
    #[serde(default = "period")]
    pub period_s: u64,
    /// Microsoft Entra's and Resource Manager's roots instead of the public cloud's: a national
    /// cloud's (`https://login.microsoftonline.us`, `https://management.usgovcloudapi.net`), a test's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authority: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Log Analytics workspaces whose rows are read (their ids); none, no logs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspaces: Vec<String>,
    /// What is read of them: a table (`AzureActivity`) or a query in KQL ending in rows; none, every
    /// table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_query: Option<String>,
    /// Log Analytics' root instead of the public cloud's: a national cloud's
    /// (`https://api.loganalytics.us`), a test's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logs_endpoint: Option<String>,
    #[serde(flatten)]
    pub common: Common,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            tenant_id: None,
            client_id: None,
            client_secret: None,
            subscriptions: Vec::new(),
            resource_types: Vec::new(),
            aggregations: Vec::new(),
            period_s: period(),
            authority: None,
            endpoint: None,
            workspaces: Vec::new(),
            log_query: None,
            logs_endpoint: None,
            common: Common::default(),
        }
    }
}

impl Config {
    pub fn check(&self) -> Result<(), String> {
        if self.client_secret.is_some() && (self.tenant_id.is_none() || self.client_id.is_none()) {
            return Err("a client secret goes with its tenant_id and client_id".into());
        }
        if let Some(s) = self.subscriptions.iter().find(|s| s.is_empty() || s.len() > 64 || !s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')) {
            return Err(format!("subscription {s:?}: a subscription's id"));
        }
        if let Some(t) = self.resource_types.iter().find(|t| !t.contains('/') || t.contains(['\'', '"']) || t.len() > 200) {
            return Err(format!("resource type {t:?}: as Azure writes them (Microsoft.Compute/virtualMachines)"));
        }
        if let Some(a) = self.aggregations.iter().find(|a| Agg::of(a).is_none()) {
            return Err(format!("aggregation {a:?}: Average, Total, Count, Minimum or Maximum"));
        }
        if grain(self.period_s).is_none() {
            return Err(format!("period_s {}: an Azure time grain (60, 300, 900, 1800, 3600, 21600, 43200, 86400)", self.period_s));
        }
        for e in [&self.authority, &self.endpoint, &self.logs_endpoint].into_iter().flatten() {
            if !(e.starts_with("https://") || e.starts_with("http://")) {
                return Err(format!("{e:?}: a URL"));
            }
        }
        if let Some(w) = self.workspaces.iter().find(|w| w.len() != 36 || !w.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')) {
            return Err(format!("workspace {w:?}: a Log Analytics workspace's id (a GUID)"));
        }
        if self.log_query.as_deref().is_some_and(|q| q.trim().is_empty() || q.len() > 10_000) {
            return Err("log_query: a table or a query, 10000 characters at the most".into());
        }
        if self.log_query.is_some() && self.workspaces.is_empty() {
            return Err("a log query goes with the workspaces it reads".into());
        }
        Ok(())
    }

    fn logs(&self) -> &str {
        self.logs_endpoint.as_deref().unwrap_or(LOGS).trim_end_matches('/')
    }

    fn arm(&self) -> &str {
        self.endpoint.as_deref().unwrap_or(ARM).trim_end_matches('/')
    }

    fn login(&self) -> String {
        self.authority.clone().or_else(|| env("AZURE_AUTHORITY_HOST")).unwrap_or_else(|| LOGIN.into()).trim_end_matches('/').to_owned()
    }
}

fn grain(s: u64) -> Option<&'static str> {
    GRAINS.iter().find(|(g, _)| *g == s).map(|(_, n)| *n)
}

fn grain_seconds(name: &str) -> Option<u64> {
    GRAINS.iter().find(|(_, n)| *n == name).map(|(g, _)| *g)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Agg {
    Average,
    Total,
    Count,
    Minimum,
    Maximum,
}

impl Agg {
    fn of(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "average" => Some(Self::Average),
            "total" => Some(Self::Total),
            "count" => Some(Self::Count),
            "minimum" => Some(Self::Minimum),
            "maximum" => Some(Self::Maximum),
            _ => None,
        }
    }

    /// Its name in a `metrics` call, and its field in a point.
    fn name(self) -> &'static str {
        match self {
            Self::Average => "Average",
            Self::Total => "Total",
            Self::Count => "Count",
            Self::Minimum => "Minimum",
            Self::Maximum => "Maximum",
        }
    }

    fn field(self) -> &'static str {
        match self {
            Self::Average => "average",
            Self::Total => "total",
            Self::Count => "count",
            Self::Minimum => "minimum",
            Self::Maximum => "maximum",
        }
    }

    fn kind(self) -> Kind {
        if matches!(self, Self::Total | Self::Count) { Kind::Count } else { Kind::Gauge }
    }
}

/// A resource type's part of a name: `Microsoft.Compute/virtualMachines` → `compute_virtualmachines`.
fn type_name(t: &str) -> String {
    let t = t.to_ascii_lowercase();
    super::lower(t.strip_prefix("microsoft.").unwrap_or(&t))
}

/// A resource's tag's key: lower case, each run of characters other than letters, digits, `_`, `.`,
/// `-` and `/` one `_` (Datadog's).
fn tag_key(k: &str) -> String {
    let mut out = String::with_capacity(k.len());
    let mut gap = false;
    for c in k.chars() {
        if c.is_ascii_alphanumeric() || "_.-/".contains(c) {
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

/// A resource's id, each segment encoded for a URL's path.
fn path(id: &str) -> String {
    id.split('/').map(encode).collect::<Vec<_>>().join("/")
}

/// An id's segment after `name` (`resourceGroups`), in any case.
fn segment<'a>(id: &'a str, name: &str) -> Option<&'a str> {
    let mut it = id.split('/');
    while let Some(s) = it.next() {
        if s.eq_ignore_ascii_case(name) {
            return it.next();
        }
    }
    None
}

// -------------------------------------------------------------------------------- credentials

/// `{"error": {"code", "message"}}` (Resource Manager's) or Entra's `error_description`.
fn error(a: &Answer) -> String {
    let v: Value = serde_json::from_slice(&a.body).unwrap_or(Value::Null);
    let said = match v.get("error") {
        Some(Value::Object(o)) => o.get("message").and_then(Value::as_str).map(|m| o.get("code").and_then(Value::as_str).map_or_else(|| m.to_owned(), |c| format!("{c}: {m}"))),
        Some(Value::String(e)) => Some(v.get("error_description").and_then(Value::as_str).map_or_else(|| e.clone(), |d| format!("{e}: {}", d.lines().next().unwrap_or(d)))),
        _ => None,
    };
    format!("{} {}", a.status, said.unwrap_or_else(|| a.text()))
}

struct Token {
    value: String,
    until: i64,
}

/// A token's answer: `expires_in` (seconds from now) or `expires_on` (unix seconds), either a
/// number or its text.
fn token_of(a: &Answer, what: &str, now: i64) -> anyhow::Result<Token> {
    anyhow::ensure!(a.ok(), "{what}: {}", error(a));
    let v: Value = a.json()?;
    let value = v.get("access_token").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("{what}: no access_token"))?.to_owned();
    let until = match (v.get("expires_in").and_then(number), v.get("expires_on").and_then(number)) {
        (Some(s), _) => now + (s * 1000.0) as i64,
        (None, Some(on)) => (on * 1000.0) as i64,
        _ => now + 3_600_000,
    };
    Ok(Token { value, until })
}

/// A token for `audience` (Resource Manager's root, or Log Analytics').
async fn token(api: &Api<'_>, cfg: &Config, audience: &str, now: i64) -> anyhow::Result<Token> {
    let scope = format!("{audience}/.default");
    let resource = format!("{audience}/");
    let entra = |tenant: &str| format!("{}/{}/oauth2/v2.0/token", cfg.login(), encode(tenant));
    let form = "application/x-www-form-urlencoded";
    if let (Some(tenant), Some(id), Some(secret)) = (&cfg.tenant_id, &cfg.client_id, &cfg.client_secret) {
        let body = super::form(&[("grant_type", "client_credentials"), ("client_id", id), ("client_secret", secret), ("scope", &scope)]);
        let a = api.call(&Call::post(&entra(tenant), form, body), |_| false).await?;
        return token_of(&a, &format!("the app {id}'s token"), now);
    }
    if let (Some(tenant), Some(id), Some(secret)) = (env("AZURE_TENANT_ID"), env("AZURE_CLIENT_ID"), env("AZURE_CLIENT_SECRET")) {
        let body = super::form(&[("grant_type", "client_credentials"), ("client_id", &id), ("client_secret", &secret), ("scope", &scope)]);
        let a = api.call(&Call::post(&entra(&tenant), form, body), |_| false).await?;
        return token_of(&a, &format!("the app {id}'s token (AZURE_CLIENT_ID)"), now);
    }
    if let (Some(file), Some(tenant), Some(id)) = (env("AZURE_FEDERATED_TOKEN_FILE"), env("AZURE_TENANT_ID"), cfg.client_id.clone().or_else(|| env("AZURE_CLIENT_ID"))) {
        let assertion = std::fs::read_to_string(&file).map_err(|e| anyhow::anyhow!("the workload identity's token {file}: {e}"))?;
        let body = super::form(&[("grant_type", "client_credentials"), ("client_id", &id), ("client_assertion_type", "urn:ietf:params:oauth:client-assertion-type:jwt-bearer"), ("client_assertion", assertion.trim()), ("scope", &scope)]);
        let a = api.call(&Call::post(&entra(&tenant), form, body), |_| false).await?;
        return token_of(&a, "the workload identity's token", now);
    }
    let user_assigned = cfg.client_id.clone().or_else(|| env("AZURE_CLIENT_ID"));
    if let (Some(endpoint), Some(header)) = (env("IDENTITY_ENDPOINT"), env("IDENTITY_HEADER")) {
        let mut q = vec![("api-version", "2019-08-01"), ("resource", resource.as_str())];
        if let Some(id) = &user_assigned {
            q.push(("client_id", id));
        }
        let url = format!("{endpoint}?{}", super::form(&q));
        let a = api.call(&Call::get(&url).header("x-identity-header", &header), |_| false).await?;
        return token_of(&a, "the managed identity's token (IDENTITY_ENDPOINT)", now);
    }
    let base = env("AZURE_POD_IDENTITY_AUTHORITY_HOST").unwrap_or_else(|| IMDS.into());
    let mut q = vec![("api-version", "2018-02-01"), ("resource", resource.as_str())];
    if let Some(id) = &user_assigned {
        q.push(("client_id", id));
    }
    let url = format!("{}/metadata/identity/oauth2/token?{}", base.trim_end_matches('/'), super::form(&q));
    let a = api.once(&Call::get(&url).header("metadata", "true").timeout(METADATA_TIMEOUT)).await.map_err(|e| {
        anyhow::anyhow!("no Azure credentials: no app given, none in the environment (AZURE_CLIENT_SECRET, AZURE_FEDERATED_TOKEN_FILE, IDENTITY_ENDPOINT), and no VM metadata service ({e:#})")
    })?;
    token_of(&a, "the VM's managed identity's token", now)
}

// --------------------------------------------------------------------------------------- pulls

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Page<T> {
    #[serde(default = "Vec::new")]
    value: Vec<T>,
    next_link: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Subscription {
    subscription_id: String,
    state: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct Listed {
    id: String,
    name: String,
    #[serde(rename = "type")]
    kind: String,
    location: Option<String>,
    #[serde(default)]
    tags: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Definition {
    name: Name,
    primary_aggregation_type: Option<String>,
    #[serde(default)]
    metric_availabilities: Vec<Availability>,
    #[serde(default)]
    is_dimension_required: bool,
}

#[derive(Debug, Deserialize)]
struct Name {
    value: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Availability {
    time_grain: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Metrics {
    #[serde(default)]
    value: Vec<MetricOut>,
}

#[derive(Debug, Deserialize)]
struct MetricOut {
    name: Name,
    #[serde(default)]
    timeseries: Vec<Timeseries>,
}

#[derive(Debug, Deserialize)]
struct Timeseries {
    #[serde(default)]
    data: Vec<BTreeMap<String, Value>>,
}

/// A type's metric, as read: its name, time grain, and aggregations (the primary first).
#[derive(Debug, Clone)]
struct Def {
    name: String,
    grain: &'static str,
    aggs: Vec<Agg>,
}

/// A resource's metric, and its series by aggregation.
#[derive(Debug)]
struct Metric {
    name: String,
    grain: &'static str,
    series: Vec<(Agg, SeriesKey)>,
}

#[derive(Debug)]
struct Resource {
    id: String,
    metrics: Vec<Metric>,
}

pub struct Puller {
    cfg: Config,
    tags: Vec<(String, String)>,
    extras: Vec<Agg>,
    token: Option<Token>,
    logs_token: Option<Token>,
    subscriptions: Vec<String>,
    resources: Vec<Resource>,
    over: usize,
    discovered: Option<Instant>,
}

impl Puller {
    pub fn new(cfg: Config) -> Self {
        let tags = cfg.common.tag_pairs();
        let extras = cfg.aggregations.iter().filter_map(|a| Agg::of(a)).collect();
        Self { subscriptions: cfg.subscriptions.clone(), cfg, tags, extras, token: None, logs_token: None, resources: Vec::new(), over: 0, discovered: None }
    }

    async fn bearer(&mut self, api: &Api<'_>, now: i64) -> anyhow::Result<String> {
        if let Some(t) = &self.token
            && t.until.saturating_sub(now) > REFRESH_MS
        {
            return Ok(t.value.clone());
        }
        let t = token(api, &self.cfg, self.cfg.arm(), now).await?;
        let v = t.value.clone();
        self.token = Some(t);
        Ok(v)
    }

    async fn logs_bearer(&mut self, api: &Api<'_>, now: i64) -> anyhow::Result<String> {
        if let Some(t) = &self.logs_token
            && t.until.saturating_sub(now) > REFRESH_MS
        {
            return Ok(t.value.clone());
        }
        let t = token(api, &self.cfg, self.cfg.logs(), now).await?;
        let v = t.value.clone();
        self.logs_token = Some(t);
        Ok(v)
    }

    /// Each workspace's rows in `[start, end]` (unix ms), oldest first, as records: a workspace that
    /// fails, a warning, the rest read.
    async fn read_logs(&mut self, api: &Api<'_>, start: i64, end: i64, warnings: &mut Vec<String>) -> Vec<(u64, Record)> {
        let token = match self.logs_bearer(api, end).await {
            Ok(t) => t,
            Err(e) => {
                warnings.push(format!("logs: {e:#}"));
                return Vec::new();
            }
        };
        let base = self.cfg.log_query.clone().unwrap_or_else(|| "union withsource=_Table *".into());
        let (from, to) = (super::rfc3339(start), super::rfc3339(end));
        let query = format!("{base}\n| where TimeGenerated between (datetime({from}) .. datetime({to}))\n| order by TimeGenerated asc\n| take {LOG_ROWS}");
        let mut out = Vec::new();
        for ws in &self.cfg.workspaces {
            let url = format!("{}/v1/workspaces/{}/query", self.cfg.logs(), encode(ws));
            let body = serde_json::json!({ "query": query, "timespan": format!("{from}/{to}") }).to_string();
            let answer = api.call(&Call::post(&url, "application/json", body).bearer(&token), |a| a.status == 429).await;
            let v: Value = match answer.and_then(|a| if a.ok() { a.json() } else { Err(anyhow::anyhow!("{}", error(&a))) }) {
                Ok(v) => v,
                Err(e) => {
                    warnings.push(format!("workspace {ws}: {e:#}"));
                    continue;
                }
            };
            let mut rows = 0usize;
            for t in v.get("tables").and_then(Value::as_array).into_iter().flatten() {
                let columns: Vec<&str> = t.get("columns").and_then(Value::as_array).into_iter().flatten().map(|c| c.get("name").and_then(Value::as_str).unwrap_or("")).collect();
                for row in t.get("rows").and_then(Value::as_array).into_iter().flatten() {
                    rows += 1;
                    if let Some(r) = row.as_array().and_then(|cells| record_of(ws, &columns, cells, &self.tags)) {
                        out.push(r);
                    }
                }
            }
            if rows >= LOG_ROWS {
                warnings.push(format!("workspace {ws}: {LOG_ROWS} rows read, the most a pull takes: the rest are the next pull's"));
            }
        }
        out
    }

    async fn get(&self, api: &Api<'_>, token: &str, url: &str) -> anyhow::Result<Answer> {
        let a = api.call(&Call::get(url).bearer(token), |_| false).await?;
        anyhow::ensure!(a.ok(), "{}", error(&a));
        Ok(a)
    }

    /// Every page of a list, its next pages on Resource Manager's own host alone (the token is
    /// sent to no other).
    async fn pages<T: serde::de::DeserializeOwned>(&self, api: &Api<'_>, token: &str, first: String) -> anyhow::Result<Vec<T>> {
        let mut out = Vec::new();
        let mut url = first;
        let mut seen = 0;
        loop {
            let page: Page<T> = self.get(api, token, &url).await?.json()?;
            out.extend(page.value);
            seen += 1;
            match page.next_link.filter(|n| !n.is_empty()) {
                Some(n) if n.starts_with(&format!("{}/", self.cfg.arm())) && seen < 10_000 => url = n,
                Some(n) if !n.starts_with(&format!("{}/", self.cfg.arm())) => anyhow::bail!("a next page elsewhere than Resource Manager ({n}): not followed"),
                _ => return Ok(out),
            }
        }
    }

    pub async fn pull(&mut self, api: &Api<'_>, now: i64) -> anyhow::Result<Pulled> {
        let token = self.bearer(api, now).await?;
        let mut warnings = Vec::new();
        if self.discovered.is_none_or(|t| t.elapsed() >= DISCOVER_EVERY) {
            match self.discover(api, &token, &mut warnings).await {
                Ok(()) => self.discovered = Some(Instant::now()),
                Err(e) if self.discovered.is_some() => warnings.push(format!("resources were not listed again ({e:#}); those listed before are read")),
                Err(e) => return Err(e),
            }
        }
        let this = &*self;
        let token = token.as_str();
        // By place, not by reference: futures over borrowed items are not `Send` to rustc (their
        // higher-ranked lifetimes, rust-lang/rust#64552).
        let read: Vec<anyhow::Result<Vec<(SeriesKey, Vec<(i64, f64)>)>>> = crate::rt::each(0..this.resources.len(), CONCURRENCY, |i| async move {
            match this.resources.get(i) {
                Some(r) => this.resource(api, token, r, now).await,
                None => Ok(Vec::new()),
            }
        })
        .await;
        let mut out = Pulled { known: self.resources.iter().map(|r| r.metrics.iter().map(|m| m.series.len()).sum::<usize>()).sum(), over: self.over, ..Pulled::default() };
        let (mut failed, mut first_error) = (0usize, None);
        for (r, got) in self.resources.iter().zip(read) {
            match got {
                Ok(series) => out.series.extend(series),
                Err(e) => {
                    failed += 1;
                    if warnings.len() < WARNINGS {
                        warnings.push(format!("{}: {e:#}", r.id));
                    }
                    first_error.get_or_insert(e);
                }
            }
        }
        if failed > 0 && failed == self.resources.len() {
            return Err(first_error.unwrap_or_else(|| anyhow::anyhow!("every resource failed")));
        }
        if !self.cfg.workspaces.is_empty() {
            out.logs = self.read_logs(api, now.saturating_sub(self.cfg.common.lookback_ms()), now, &mut warnings).await;
        }
        out.warnings = warnings;
        Ok(out)
    }

    /// Lists each subscription's resources and each type's metrics, keeping the resources whose
    /// series fit under `max_series`.
    async fn discover(&mut self, api: &Api<'_>, token: &str, warnings: &mut Vec<String>) -> anyhow::Result<()> {
        if self.subscriptions.is_empty() {
            let subs: Vec<Subscription> = self.pages(api, token, format!("{}/subscriptions?api-version=2022-12-01", self.cfg.arm())).await?;
            self.subscriptions = subs.into_iter().filter(|s| s.state.as_deref().is_none_or(|st| st == "Enabled")).map(|s| s.subscription_id).collect();
            anyhow::ensure!(!self.subscriptions.is_empty(), "the credentials see no subscription");
        }
        let filter = (!self.cfg.resource_types.is_empty()).then(|| self.cfg.resource_types.iter().map(|t| format!("resourceType eq '{t}'")).collect::<Vec<_>>().join(" or "));
        let mut listed: Vec<(String, Listed)> = Vec::new();
        let mut first_error = None;
        let mut subs_read = 0;
        for sub in &self.subscriptions {
            let mut url = format!("{}/subscriptions/{}/resources?api-version=2021-04-01", self.cfg.arm(), encode(sub));
            if let Some(f) = &filter {
                url.push_str(&format!("&$filter={}", encode(f)));
            }
            match self.pages::<Listed>(api, token, url).await {
                Ok(rs) => {
                    subs_read += 1;
                    listed.extend(rs.into_iter().map(|r| (sub.clone(), r)));
                }
                Err(e) => {
                    warnings.push(format!("subscription {sub}: {e:#}"));
                    first_error.get_or_insert(e);
                }
            }
        }
        if subs_read == 0
            && let Some(e) = first_error
        {
            return Err(e);
        }
        listed.sort_by_cached_key(|(_, r)| r.id.to_ascii_lowercase());
        listed.dedup_by(|a, b| a.1.id.eq_ignore_ascii_case(&b.1.id));
        // Each type's metrics, read from its first resource.
        let mut defs: BTreeMap<String, Vec<Def>> = BTreeMap::new();
        for (_, r) in &listed {
            let t = r.kind.to_ascii_lowercase();
            if defs.contains_key(&t) {
                continue;
            }
            let url = format!("{}{}/providers/Microsoft.Insights/metricDefinitions?api-version=2018-01-01", self.cfg.arm(), path(&r.id));
            let a = api.call(&Call::get(&url).bearer(token), |_| false).await?;
            let found = if a.ok() {
                a.json::<Page<Definition>>().map(|p| self.defs(p.value)).unwrap_or_default()
            } else {
                // A type without metrics answers 400 or 404 (`ResourceTypeNotSupported`); another
                // error is said, and the type is asked again next discovery.
                if !matches!(a.status, 400 | 404) && warnings.len() < WARNINGS {
                    warnings.push(format!("{}'s metrics: {}", r.kind, error(&a)));
                }
                Vec::new()
            };
            defs.insert(t, found);
        }
        let mut budget = self.cfg.common.max_series;
        let mut over = 0;
        let mut resources = Vec::new();
        for (sub, r) in listed {
            let Some(ds) = defs.get(&r.kind.to_ascii_lowercase()).filter(|d| !d.is_empty()) else { continue };
            let each: usize = ds.iter().map(|d| d.aggs.len()).sum();
            if budget < each {
                over += each;
                continue;
            }
            budget -= each;
            resources.push(self.resource_of(&sub, &r, ds));
        }
        self.resources = resources;
        self.over = over;
        Ok(())
    }

    /// A type's definitions as read: by the period's grain, else the finest above it (else the
    /// coarsest); those that need a dimension, or whose names hold a comma, left out.
    fn defs(&self, ds: Vec<Definition>) -> Vec<Def> {
        ds.into_iter()
            .filter(|d| !d.is_dimension_required && !d.name.value.contains(',') && !d.name.value.is_empty())
            .map(|d| {
                let mut grains: Vec<u64> = d.metric_availabilities.iter().filter_map(|a| a.time_grain.as_deref().and_then(grain_seconds)).collect();
                grains.sort_unstable();
                let at = grains.iter().copied().find(|&g| g >= self.cfg.period_s).or_else(|| grains.last().copied()).unwrap_or(self.cfg.period_s);
                let primary = d.primary_aggregation_type.as_deref().and_then(Agg::of).unwrap_or(Agg::Average);
                let mut aggs = vec![primary];
                aggs.extend(self.extras.iter().copied().filter(|a| *a != primary));
                Def { name: d.name.value, grain: grain(at).unwrap_or("PT1M"), aggs }
            })
            .collect()
    }

    fn resource_of(&self, sub: &str, r: &Listed, ds: &[Def]) -> Resource {
        let tag_keys: Vec<(String, &str)> = r.tags.iter().flatten().map(|(k, v)| (tag_key(k), v.as_str())).filter(|(k, _)| !k.is_empty()).collect();
        let rg = segment(&r.id, "resourceGroups").unwrap_or_default().to_ascii_lowercase();
        let mut pairs: Vec<(&str, &str)> = vec![("subscription_id", sub), ("resource_group", rg.as_str()), ("name", r.name.as_str())];
        if let Some(l) = &r.location {
            pairs.push(("region", l.as_str()));
        }
        pairs.extend(tag_keys.iter().map(|(k, v)| (k.as_str(), *v)));
        pairs.extend(self.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        let base = format!("azure.{}", type_name(&r.kind));
        let metrics = ds
            .iter()
            .map(|d| {
                let name = format!("{base}.{}", super::snake(&d.name));
                let series = d
                    .aggs
                    .iter()
                    .enumerate()
                    .filter_map(|(i, &a)| {
                        let metric = if i == 0 { name.clone() } else { format!("{name}.{}", a.field()) };
                        Some((a, super::key(&metric, &pairs, a.kind())?))
                    })
                    .collect();
                Metric { name: d.name.clone(), grain: d.grain, series }
            })
            .collect();
        Resource { id: r.id.clone(), metrics }
    }

    /// One resource's points over the lookback before `now`: its metrics by grain, 20 a call.
    async fn resource(&self, api: &Api<'_>, token: &str, r: &Resource, now: i64) -> anyhow::Result<Vec<(SeriesKey, Vec<(i64, f64)>)>> {
        let mut by_grain: BTreeMap<&'static str, Vec<&Metric>> = BTreeMap::new();
        for m in &r.metrics {
            by_grain.entry(m.grain).or_default().push(m);
        }
        let mut out = Vec::new();
        for (g, ms) in by_grain {
            let step = i64::try_from(grain_seconds(g).unwrap_or(60) * 1000).unwrap_or(60_000);
            let end = now.div_euclid(step) * step;
            let start = (end - self.cfg.common.lookback_ms().max(step)).div_euclid(step) * step;
            let timespan = format!("{}/{}", super::rfc3339(start), super::rfc3339(end));
            for part in ms.chunks(NAMES) {
                let names = part.iter().map(|m| m.name.as_str()).collect::<Vec<_>>().join(",");
                let mut aggs: Vec<Agg> = part.iter().flat_map(|m| m.series.iter().map(|(a, _)| *a)).collect();
                aggs.sort_unstable();
                aggs.dedup();
                let aggs = aggs.iter().map(|a| a.name()).collect::<Vec<_>>().join(",");
                let url = format!(
                    "{}{}/providers/Microsoft.Insights/metrics?api-version=2018-01-01&metricnames={}&timespan={}&interval={g}&aggregation={}",
                    self.cfg.arm(),
                    path(&r.id),
                    encode(&names),
                    encode(&timespan),
                    encode(&aggs)
                );
                let got: Metrics = self.get(api, token, &url).await?.json()?;
                absorb(part, got, end, &mut out);
            }
        }
        Ok(out)
    }
}

/// A `metrics` answer's points into each metric's series; none at or after `end` (a period not
/// yet over).
fn absorb(asked: &[&Metric], got: Metrics, end: i64, out: &mut Vec<(SeriesKey, Vec<(i64, f64)>)>) {
    for m in got.value {
        let Some(mine) = asked.iter().find(|a| a.name.eq_ignore_ascii_case(&m.name.value)) else { continue };
        for (agg, key) in &mine.series {
            let mut points: Vec<(i64, f64)> = m
                .timeseries
                .iter()
                .flat_map(|t| &t.data)
                .filter_map(|d| {
                    let ts = parse_time(d.get("timeStamp")?.as_str()?)?;
                    let v = d.get(agg.field()).and_then(Value::as_f64)?;
                    (ts < end && v.is_finite()).then_some((ts, v))
                })
                .collect();
            points.sort_by_key(|p| p.0);
            points.dedup_by_key(|p| p.0);
            if !points.is_empty() {
                out.push((key.clone(), points));
            }
        }
    }
}

/// A Log Analytics row's record (the module's doc says how), its id; none without a time.
fn record_of(workspace: &str, columns: &[&str], cells: &[Value], tags: &[(String, String)]) -> Option<(u64, Record)> {
    let cell = |name: &str| columns.iter().position(|c| *c == name).and_then(|i| cells.get(i)).filter(|v| !v.is_null() && v.as_str() != Some(""));
    let text = |v: &Value| v.as_str().map_or_else(|| v.to_string(), str::to_owned);
    let ts = cell("TimeGenerated").and_then(Value::as_str).and_then(super::parse_time_ns)?;
    let table = cell("_Table").or_else(|| cell("Type")).map(text).unwrap_or_default();
    let resource = cell("_ResourceId").map(text).unwrap_or_default();
    let id = match cell("_ItemId") {
        Some(i) => super::id_of(&["azure", workspace, &text(i)]),
        None => super::id_of(&["azure", workspace, &Value::Array(cells.to_vec()).to_string()]),
    };
    let named = ["Message", "RenderedDescription", "Description", "ResultDescription", "OperationName"];
    let said = named.iter().find_map(|k| cell(k)).map(text);
    let message = said.unwrap_or_else(|| {
        let row: serde_json::Map<String, Value> = columns.iter().zip(cells).filter(|(_, v)| !v.is_null()).map(|(c, v)| ((*c).to_owned(), v.clone())).collect();
        Value::Object(row).to_string()
    });
    let severity = match (cell("SeverityLevel").and_then(Value::as_i64), cell("Level").or_else(|| cell("Severity")).and_then(Value::as_str)) {
        // Application Insights' levels: verbose, information, warning, error, critical.
        (Some(l), _) => [5u8, 9, 13, 17, 21].get(usize::try_from(l).unwrap_or(0)).copied().unwrap_or(9),
        (None, Some(l)) => kiln_obs::logs::record::severities(l).map_or_else(|| super::severity_of(&message), |r| *r.start()),
        (None, None) => super::severity_of(&message),
    };
    let service = resource.rsplit('/').find(|p| !p.is_empty()).map_or_else(|| table.clone(), str::to_owned);
    let host = cell("Computer").map(text).unwrap_or_default();
    let mut attrs: Vec<(String, String)> = vec![("azure.table".into(), table.clone()), ("azure.workspace".into(), workspace.to_owned())];
    if !resource.is_empty() {
        attrs.push(("azure.resource_id".into(), resource.clone()));
    }
    let skip = |c: &str| matches!(c, "TimeGenerated" | "_Table" | "Type" | "_ResourceId" | "_ItemId" | "Computer" | "SeverityLevel" | "Level" | "Severity" | "TenantId" | "SourceSystem" | "MG" | "ManagementGroupName") || named.contains(&c);
    for (c, v) in columns.iter().zip(cells).filter(|(c, v)| !skip(c) && !v.is_null() && !v.is_object() && !v.is_array() && v.as_str() != Some("")).take(ROW_ATTRS) {
        attrs.push(((*c).to_owned(), text(v)));
    }
    for (k, v) in tags {
        if !attrs.iter().any(|(a, _)| a == k) {
            attrs.push((k.clone(), v.clone()));
        }
    }
    Some((id, Record { ts, severity, service, host, source: format!("azure.{}", table.to_ascii_lowercase()), message: super::message(message), attrs, trace_id: None, span_id: None }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_log_analytics_row_is_a_record() -> anyhow::Result<()> {
        let ws = "00000000-0000-0000-0000-000000000001";
        let cols = ["TimeGenerated", "Message", "SeverityLevel", "_ResourceId", "AppRoleName", "_ItemId", "Type", "Properties"];
        let row: Vec<Value> = serde_json::from_str(r#"["2026-10-18T12:00:00.5Z", "payment failed", 3, "/subscriptions/s/resourceGroups/g/providers/Microsoft.Web/sites/checkout", "api", "item-9", "AppTraces", {"k": "v"}]"#)?;
        let (id, r) = record_of(ws, &cols, &row, &[]).ok_or_else(|| anyhow::anyhow!("no record"))?;
        anyhow::ensure!(r.message == "payment failed" && r.severity == 17 && r.service == "checkout" && r.source == "azure.apptraces" && r.ts == 1_792_324_800_500_000_000, "{r:?}");
        anyhow::ensure!(r.attrs.contains(&("AppRoleName".into(), "api".into())) && !r.attrs.iter().any(|(k, _)| k == "Properties"), "{:?}", r.attrs);
        let again: Vec<Value> = serde_json::from_str(r#"["2026-10-18T12:00:00.5Z", "other", 1, "", "", "item-9", "AppTraces", null]"#)?;
        anyhow::ensure!(record_of(ws, &cols, &again, &[]).map(|x| x.0) == Some(id), "the row's _ItemId is its id");
        let cols = ["TimeGenerated", "OperationName", "Level", "Type", "Computer"];
        let row: Vec<Value> = serde_json::from_str(r#"["2026-10-18T12:00:01Z", "Delete virtual machine", "Warning", "AzureActivity", "vm-1"]"#)?;
        let (_, r) = record_of(ws, &cols, &row, &[]).ok_or_else(|| anyhow::anyhow!("no record"))?;
        anyhow::ensure!(r.message == "Delete virtual machine" && r.severity == 13 && r.host == "vm-1" && r.service == "AzureActivity", "{r:?}");
        let cols = ["TimeGenerated", "Count", "Type"];
        let row: Vec<Value> = serde_json::from_str(r#"["2026-10-18T12:00:02Z", 4, "Usage"]"#)?;
        let (_, r) = record_of(ws, &cols, &row, &[]).ok_or_else(|| anyhow::anyhow!("no record"))?;
        anyhow::ensure!(r.message.contains("\"Count\":4"), "a row without a message is its values: {}", r.message);
        Ok(())
    }

    #[test]
    fn names_tags_and_grains_read_as_datadog_s() {
        assert_eq!(type_name("Microsoft.Compute/virtualMachines"), "compute_virtualmachines");
        assert_eq!(type_name("Microsoft.Sql/servers/databases"), "sql_servers_databases");
        assert_eq!(type_name("Microsoft.Web/sites"), "web_sites");
        assert_eq!(tag_key("Cost Center"), "cost_center");
        assert_eq!(tag_key("app.kubernetes.io/name"), "app.kubernetes.io/name");
        assert_eq!(segment("/subscriptions/s/resourceGroups/RG-1/providers/Microsoft.Compute/virtualMachines/vm", "resourcegroups"), Some("RG-1"));
        assert_eq!(path("/subscriptions/s/resourceGroups/my rg/x"), "/subscriptions/s/resourceGroups/my%20rg/x");
        assert_eq!(grain(300), Some("PT5M"));
        assert_eq!(grain(120), None);
    }

    #[test]
    fn definitions_resources_and_points_become_series() -> anyhow::Result<()> {
        let cfg = Config { aggregations: vec!["Maximum".into(), "Total".into()], common: Common { tags: vec!["team:infra".into()], ..Common::default() }, ..Config::default() };
        let p = Puller::new(cfg);
        let defs: Page<Definition> = serde_json::from_str(
            r#"{"value":[
                {"name":{"value":"Percentage CPU","localizedValue":"Percentage CPU"},"primaryAggregationType":"Average","metricAvailabilities":[{"timeGrain":"PT1M","retention":"P93D"},{"timeGrain":"PT1H"}]},
                {"name":{"value":"Network In Total"},"primaryAggregationType":"Total","metricAvailabilities":[{"timeGrain":"PT1M"}]},
                {"name":{"value":"UsedCapacity"},"primaryAggregationType":"Average","metricAvailabilities":[{"timeGrain":"PT1H"}]},
                {"name":{"value":"Needs A Dimension"},"isDimensionRequired":true},
                {"name":{"value":"a,b"}}
            ]}"#,
        )?;
        let ds = p.defs(defs.value);
        let summary: Vec<(&str, &str, Vec<Agg>)> = ds.iter().map(|d| (d.name.as_str(), d.grain, d.aggs.clone())).collect();
        anyhow::ensure!(
            summary == vec![("Percentage CPU", "PT1M", vec![Agg::Average, Agg::Maximum, Agg::Total]), ("Network In Total", "PT1M", vec![Agg::Total, Agg::Maximum]), ("UsedCapacity", "PT1H", vec![Agg::Average, Agg::Maximum, Agg::Total])],
            "{summary:?}"
        );
        let listed: Listed = serde_json::from_str(r#"{"id":"/subscriptions/sub-1/resourceGroups/Prod-RG/providers/Microsoft.Compute/virtualMachines/vm-1","name":"vm-1","type":"Microsoft.Compute/virtualMachines","location":"eastus","tags":{"Cost Center":"42","team":"never-this"}}"#)?;
        let r = p.resource_of("sub-1", &listed, &ds);
        let cpu = r.metrics.first().ok_or_else(|| anyhow::anyhow!("a metric"))?;
        let names: Vec<(String, Vec<(String, String)>)> = cpu
            .series
            .iter()
            .filter_map(|(_, k)| {
                let key = kiln_obs::batch::parse_key(&k.key).ok()?;
                Some((key.metric.to_owned(), key.tags.iter().map(|(a, b)| ((*a).to_owned(), (*b).to_owned())).collect()))
            })
            .collect();
        let tags: Vec<(String, String)> = [("cost_center", "42"), ("name", "vm-1"), ("region", "eastus"), ("resource_group", "prod-rg"), ("subscription_id", "sub-1"), ("team", "never-this")].iter().map(|(a, b)| ((*a).to_owned(), (*b).to_owned())).collect();
        anyhow::ensure!(
            names == vec![("azure.compute_virtualmachines.percentage_cpu".to_owned(), tags.clone()), ("azure.compute_virtualmachines.percentage_cpu.maximum".to_owned(), tags.clone()), ("azure.compute_virtualmachines.percentage_cpu.total".to_owned(), tags)],
            "{names:?}"
        );
        anyhow::ensure!(r.metrics.get(1).and_then(|m| m.series.first()).is_some_and(|(a, k)| *a == Agg::Total && k.kind == Kind::Count));
        // Points: the period not yet over is not taken; a point without the aggregation is none.
        let got: Metrics = serde_json::from_str(
            r#"{"cost":0,"timespan":"x","interval":"PT1M","value":[{"id":"x","type":"Microsoft.Insights/metrics","name":{"value":"Percentage CPU"},"unit":"Percent","timeseries":[{"metadatavalues":[],"data":[
                {"timeStamp":"2026-10-18T12:01:00Z","average":2.5,"maximum":9},
                {"timeStamp":"2026-10-18T12:00:00Z","average":1.5,"maximum":3},
                {"timeStamp":"2026-10-18T12:02:00Z"},
                {"timeStamp":"2026-10-18T12:03:00Z","average":7}
            ]}],"errorCode":"Success"}]}"#,
        )?;
        let end = parse_time("2026-10-18T12:03:00Z").unwrap_or_default();
        let mut out = Vec::new();
        absorb(&[cpu], got, end, &mut out);
        let t0 = parse_time("2026-10-18T12:00:00Z").unwrap_or_default();
        let points: Vec<&[(i64, f64)]> = out.iter().map(|(_, p)| p.as_slice()).collect();
        anyhow::ensure!(points == vec![&[(t0, 1.5), (t0 + 60_000, 2.5)][..], &[(t0, 3.0), (t0 + 60_000, 9.0)][..]], "{points:?}");
        Ok(())
    }

    #[test]
    fn settings_are_checked() {
        assert_eq!(Config::default().check(), Ok(()));
        for bad in [
            Config { client_secret: Some("s".into()), ..Config::default() },
            Config { period_s: 120, ..Config::default() },
            Config { aggregations: vec!["Median".into()], ..Config::default() },
            Config { resource_types: vec!["virtualMachines".into()], ..Config::default() },
            Config { subscriptions: vec!["a b".into()], ..Config::default() },
        ] {
            assert!(bad.check().is_err(), "{bad:?}");
        }
    }
}
