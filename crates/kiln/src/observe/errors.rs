//! Error Tracking (`docs/observe.md` §8.8): the issues the server keeps, as Datadog's Error
//! Tracking keeps them.
//!
//! An error that is an issue carries its `issue.id` (`kiln_obs::errors`: an entry span's error, a
//! log's, given where it is taken) and is counted on the server in the metric
//! `error_tracking.errors`, by issue, service, env and version (each window of ten seconds once its
//! records have come, as metrics from logs are). Every ten seconds the server reads that metric over
//! the last twenty minutes (a window is counted again for records late to it, so long): an issue it
//! has not seen is kept, its type, message and frame read from its first error in the span or the
//! log store; each one's first and last error, its versions and its envs follow its counts.
//!
//! Its status is Datadog's: FOR REVIEW as it comes; REVIEWED, RESOLVED or IGNORED as a user says.
//! An issue with no error for fourteen days is RESOLVED (Datadog's); a RESOLVED one that errs again
//! is a regression, FOR REVIEW again, when the error is of a version it had not been seen in when it
//! was resolved (any error, for a service that says no versions). An IGNORED one stays so.
//!
//! Kept in `<observe>/issues.json`: a user's change at once, what the metric says a minute after at
//! the most, everything as the server stops. [`MAX_ISSUES`] at the most: past them, the one longest
//! without an error among those RESOLVED or IGNORED goes, else the one longest without an error.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Issues kept at the most.
pub const MAX_ISSUES: usize = 10_000;
/// How long an issue goes without an error before it is resolved: Datadog's fourteen days.
pub const RESOLVE_AFTER_MS: i64 = 14 * 86_400_000;
/// How far back each read of the metric goes: as long as a window is counted again
/// (`super::logmetrics::HORIZON_MS`), so a count that comes late is read.
const HORIZON_MS: i64 = super::logmetrics::HORIZON_MS;
/// How far back the first read goes: what an issue may have done since it was last resolved.
const FIRST_MS: i64 = RESOLVE_AFTER_MS;
/// How often what the metric says is saved at the most.
const SAVE_EVERY: Duration = Duration::from_secs(60);
/// What an issue keeps of each: its versions and envs (the latest), its activity.
const VERSIONS: usize = 32;
const ENVS: usize = 16;
const ACTIVITY: usize = 50;
/// Text an issue keeps of its error at the most (bytes).
const TEXT_BYTES: usize = 512;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// An issue's status: Datadog's.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    #[default]
    ForReview,
    Reviewed,
    Resolved,
    Ignored,
}

impl Status {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().replace([' ', '-'], "_").as_str() {
            "for_review" => Self::ForReview,
            "reviewed" => Self::Reviewed,
            "resolved" => Self::Resolved,
            "ignored" => Self::Ignored,
            _ => return None,
        })
    }
}

/// A change of an issue's status: when, to what, by whom (`kiln` or `user`), why.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Activity {
    pub at: i64,
    pub status: Status,
    pub by: String,
    pub why: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Issue {
    pub id: String,
    /// Where its errors are said: `span` or `log` (empty until one is read).
    pub source: String,
    pub service: String,
    /// Its errors' type (Datadog's `error.type`, a log's `error.kind`).
    #[serde(rename = "type")]
    pub kind: String,
    /// Its first error's message.
    pub message: String,
    /// The frame its errors are grouped by: its file and its function.
    pub file: String,
    pub function: String,
    /// Its first and its last error's window (ms).
    pub first_seen: i64,
    pub last_seen: i64,
    /// The versions and envs its errors were of, in the order they came (the latest kept).
    pub versions: Vec<String>,
    pub envs: Vec<String>,
    pub status: Status,
    pub status_at: i64,
    /// Back for review because it erred again once resolved.
    pub regression: bool,
    /// The versions it had been seen in when it was resolved.
    pub resolved_in: Vec<String>,
    pub activity: Vec<Activity>,
}

impl Issue {
    fn set(&mut self, status: Status, at: i64, by: &str, why: &str) {
        if status == Status::Resolved {
            self.resolved_in = self.versions.clone();
        }
        if status != Status::ForReview || by == "user" {
            self.regression = false;
        }
        self.status = status;
        self.status_at = at;
        self.activity.push(Activity { at, status, by: by.to_owned(), why: why.to_owned() });
        let over = self.activity.len().saturating_sub(ACTIVITY);
        self.activity.drain(..over);
    }
}

/// What the first error of an issue says of it, read from the span or the log store.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sample {
    pub source: String,
    pub kind: String,
    pub message: String,
    pub file: String,
    pub function: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Saved {
    /// The time the metric was read to (ms).
    scanned_to: i64,
    issues: Vec<Issue>,
}

#[derive(Debug, Default)]
struct State {
    issues: HashMap<String, Issue>,
    scanned_to: i64,
    dirty: bool,
}

pub struct Issues {
    path: PathBuf,
    state: Mutex<State>,
}

impl Issues {
    pub fn open(dir: &Path) -> Arc<Self> {
        let path = dir.join("issues.json");
        let saved: Saved = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let issues = saved.issues.into_iter().map(|i| (i.id.clone(), i)).collect();
        Arc::new(Self { path, state: Mutex::new(State { issues, scanned_to: saved.scanned_to, dirty: false }) })
    }

    fn save(&self) {
        let bytes = {
            let mut st = lock(&self.state);
            let mut issues: Vec<&Issue> = st.issues.values().collect();
            issues.sort_by(|a, b| a.id.cmp(&b.id));
            let saved = serde_json::to_vec(&serde_json::json!({ "scanned_to": st.scanned_to, "issues": issues }));
            st.dirty = false;
            saved
        };
        match bytes {
            Ok(b) => {
                if let Err(e) = crate::sync::group::save_atomic(&self.path, &b) {
                    tracing::warn!(error = %e, "Error Tracking's issues were not saved");
                }
            }
            Err(e) => tracing::warn!(error = %e, "Error Tracking's issues were not saved"),
        }
    }

    /// Every issue kept.
    pub fn list(&self) -> Vec<Issue> {
        lock(&self.state).issues.values().cloned().collect()
    }

    pub fn get(&self, id: &str) -> Option<Issue> {
        lock(&self.state).issues.get(id).cloned()
    }

    /// A user's change of an issue's status, saved at once; the issue as it is now.
    pub fn set_status(&self, id: &str, status: Status, now_ms: i64) -> Option<Issue> {
        let issue = {
            let mut st = lock(&self.state);
            let i = st.issues.get_mut(id)?;
            if i.status != status {
                i.set(status, now_ms, "user", "");
            }
            i.clone()
        };
        self.save();
        Some(issue)
    }

    /// Reads the metric (`seen`: what [`kiln_obs::errors::seen`] says of `[from, to)`) as of
    /// `now_ms`, each issue not kept before given what its first error says (`sample`: its id, and
    /// when its first error was).
    pub fn scan(&self, now_ms: i64, seen: impl FnOnce(i64, i64) -> Vec<kiln_obs::errors::Seen>, sample: impl Fn(&str, i64, i64) -> Option<Sample>) {
        let scanned_to = lock(&self.state).scanned_to;
        let from = if scanned_to == 0 { now_ms.saturating_sub(FIRST_MS) } else { scanned_to.saturating_sub(HORIZON_MS) };
        let seen = seen(from, now_ms.saturating_add(1));
        // Each issue's series, together.
        let mut by_issue: BTreeMap<&str, Vec<&kiln_obs::errors::Seen>> = BTreeMap::new();
        for s in &seen {
            if s.issue != kiln_obs::logs::metrics::NONE {
                by_issue.entry(s.issue.as_str()).or_default().push(s);
            }
        }
        let new: Vec<(String, i64)> = {
            let st = lock(&self.state);
            by_issue.iter().filter(|(id, _)| !st.issues.contains_key(**id)).map(|(id, s)| ((*id).to_owned(), s.iter().map(|x| x.first).min().unwrap_or(now_ms))).collect()
        };
        // What a new issue's first error says, read outside the lock (a search of the stores).
        let samples: HashMap<String, Sample> = new.iter().filter_map(|(id, first)| sample(id, *first, now_ms).map(|s| (id.clone(), s))).collect();
        let mut st = lock(&self.state);
        for (id, series) in by_issue {
            let first = series.iter().map(|x| x.first).min().unwrap_or(now_ms);
            let last = series.iter().map(|x| x.last).max().unwrap_or(now_ms);
            let issue = st.issues.entry(id.to_owned()).or_insert_with(|| {
                let s = samples.get(id).cloned().unwrap_or_default();
                let mut i = Issue {
                    id: id.to_owned(),
                    source: s.source,
                    service: series.first().map(|x| x.service.clone()).unwrap_or_default(),
                    kind: cut(&s.kind),
                    message: cut(&s.message),
                    file: cut(&s.file),
                    function: cut(&s.function),
                    first_seen: first,
                    last_seen: last,
                    ..Issue::default()
                };
                i.set(Status::ForReview, now_ms, "kiln", "its first error");
                i
            });
            issue.first_seen = issue.first_seen.min(first);
            // A regression: an error after it was resolved, of a version it was not seen in then.
            if issue.status == Status::Resolved {
                let versioned = issue.resolved_in.iter().any(|v| v != kiln_obs::logs::metrics::NONE);
                let again = series.iter().find(|x| x.last > issue.status_at && (!versioned || !issue.resolved_in.contains(&x.version)));
                if let Some(x) = again {
                    let why = if versioned { format!("an error again, of version {}", x.version) } else { "an error again".to_owned() };
                    issue.set(Status::ForReview, now_ms, "kiln", &why);
                    issue.regression = true;
                }
            }
            issue.last_seen = issue.last_seen.max(last);
            for x in &series {
                remember(&mut issue.versions, &x.version, VERSIONS);
                remember(&mut issue.envs, &x.env, ENVS);
            }
        }
        // Fourteen days without an error: resolved.
        for issue in st.issues.values_mut() {
            if matches!(issue.status, Status::ForReview | Status::Reviewed) && now_ms.saturating_sub(issue.last_seen) > RESOLVE_AFTER_MS {
                issue.set(Status::Resolved, now_ms, "kiln", "no error in fourteen days");
            }
        }
        // Past the bound: the longest without an error among those resolved or ignored, else any.
        while st.issues.len() > MAX_ISSUES {
            let gone = st.issues.values().min_by_key(|i| (!matches!(i.status, Status::Resolved | Status::Ignored), i.last_seen)).map(|i| i.id.clone());
            match gone {
                Some(id) => {
                    st.issues.remove(&id);
                }
                None => break,
            }
        }
        st.scanned_to = now_ms;
        st.dirty = true;
    }

    /// Reads the metric every ten seconds on the server (its group's leader), until `shutdown`;
    /// saves what it says each minute at the most, and as it stops.
    pub async fn run(self: Arc<Self>, shutdown: crate::rt::Shutdown) {
        let mut saved = Instant::now();
        while shutdown.sleep(Duration::from_secs(10)).await {
            let Some(o) = super::api::get() else { continue };
            let v = o.view();
            if v.leader == 0 || v.leader != v.member {
                continue;
            }
            let Some(store) = o.store() else { continue };
            let me = self.clone();
            let _ = crate::rt::blocking(move || {
                let s = store.read().unwrap_or_else(PoisonError::into_inner);
                me.scan(
                    crate::util::now_ms(),
                    |from, to| {
                        kiln_obs::errors::seen(&s, &[], from, to).unwrap_or_else(|e| {
                            tracing::warn!(error = %e, "Error Tracking's metric was not read");
                            Vec::new()
                        })
                    },
                    |id, first, now| sample_of(&s, id, first, now),
                );
            })
            .await;
            if lock(&self.state).dirty && saved.elapsed() >= SAVE_EVERY {
                self.save();
                saved = Instant::now();
            }
        }
        self.save();
    }
}

/// `v` among `list` (its latest `max`), unless it is none.
fn remember(list: &mut Vec<String>, v: &str, max: usize) {
    if v.is_empty() || v == kiln_obs::logs::metrics::NONE || list.iter().any(|x| x == v) {
        return;
    }
    list.push(v.to_owned());
    let over = list.len().saturating_sub(max);
    list.drain(..over);
}

fn cut(s: &str) -> String {
    let mut end = s.len().min(TEXT_BYTES);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.get(..end).unwrap_or("").to_owned()
}

/// An issue's first error, from the span store, else the log store: the newest of the minute its
/// first window began, else of the range read.
pub fn sample_of(s: &kiln_obs::store::Store, id: &str, first_ms: i64, now_ms: i64) -> Option<Sample> {
    let ns = |ms: i64| ms.saturating_mul(1_000_000);
    let query = format!("@issue.id:{id}");
    let ranges = [(ns(first_ms), ns(first_ms.saturating_add(60_000))), (ns(first_ms.saturating_sub(HORIZON_MS)), ns(now_ms))];
    for (source, spans) in [("span", true), ("log", false)] {
        let search = if spans { kiln_obs::logs::search::parse_spans(&query) } else { kiln_obs::logs::search::parse(&query) };
        let Ok(search) = search else { continue };
        for (from, to) in ranges {
            let found = if spans { s.search_spans(&search, from, to, 1) } else { s.search_logs(&search, from, to, 1) };
            if let Some(r) = found.ok().and_then(|f| f.records.into_iter().next()) {
                return Some(sample_from(source, &r));
            }
        }
    }
    None
}

/// What an error's record says of its issue.
pub fn sample_from(source: &str, r: &kiln_obs::logs::record::Record) -> Sample {
    let e = kiln_obs::errors::Error::of(r.attrs.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    let frame = e.stack.and_then(kiln_obs::errors::frame);
    Sample {
        source: source.to_owned(),
        kind: e.kind.unwrap_or("").to_owned(),
        message: e.message.unwrap_or(if source == "log" { r.message.as_str() } else { "" }).to_owned(),
        file: frame.map(|f| f.file.to_owned()).unwrap_or_default(),
        function: frame.map(|f| f.function.to_owned()).unwrap_or_default(),
    }
}

// ---------------------------------------------------------------------------------- the API

/// An issue as the API says it, with its errors over the range asked (where it was read).
fn issue_json(i: &Issue, errors: Option<f64>) -> serde_json::Value {
    serde_json::json!({
        "id": i.id,
        "source": i.source,
        "service": i.service,
        "type": i.kind,
        "message": i.message,
        "file": i.file,
        "function": i.function,
        "first_seen": i.first_seen,
        "last_seen": i.last_seen,
        "versions": i.versions,
        "envs": i.envs,
        "status": i.status,
        "status_at": i.status_at,
        "regression": i.regression,
        "activity": i.activity,
        "errors": errors,
    })
}

/// A step of a timeline of `range` (ms) in some sixty buckets: the first of these that many fill.
fn step_of(range: i64) -> i64 {
    const STEPS: [i64; 11] = [10_000, 30_000, 60_000, 300_000, 900_000, 1_800_000, 3_600_000, 10_800_000, 21_600_000, 43_200_000, 86_400_000];
    STEPS.iter().copied().find(|s| range / s <= 60).unwrap_or(86_400_000)
}

/// The issues with errors in the range (a day unless asked), the most errors first: `service`,
/// `env`, `status` (a comma's list of them), `query` (words of its type, message, service, file or
/// function) narrow them; `all` adds those with none in the range, the latest first; `limit`
/// (100 unless asked, 1,000 at the most).
pub async fn list(q: &HashMap<String, String>) -> Result<crate::http::server::Response, super::api::Fail> {
    use http::StatusCode;
    let issues = super::api::issues()?.clone();
    let (from, to) = super::apm::range(q, 86_400_000)?;
    let service = q.get("service").map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
    let env = super::apm::env_filter(q);
    let statuses: Option<Vec<Status>> = q.get("status").map(|s| s.split(',').filter_map(Status::parse).collect());
    let query = q.get("query").map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty());
    let all = q.get("all").is_some_and(|v| v == "true" || v == "1");
    let limit = q.get("limit").and_then(|v| v.parse::<usize>().ok()).unwrap_or(100).min(1_000);
    super::api::reading(move |s| {
        let mut filter: Vec<(&str, &str)> = Vec::new();
        if let Some(sv) = service.as_deref() {
            filter.push(("service", sv));
        }
        if let Some(e) = env.as_deref() {
            filter.push(("env", e));
        }
        let seen = kiln_obs::errors::seen(s, &filter, from, to).map_err(|e| super::api::fail(StatusCode::INTERNAL_SERVER_ERROR, e))?;
        let mut counts: HashMap<&str, f64> = HashMap::new();
        for x in &seen {
            *counts.entry(x.issue.as_str()).or_default() += x.errors;
        }
        let words = |i: &Issue| {
            let Some(q) = query.as_deref() else { return true };
            let hay = format!("{} {} {} {} {}", i.kind, i.message, i.service, i.file, i.function).to_lowercase();
            q.split_whitespace().all(|w| hay.contains(w))
        };
        let mut out: Vec<(Option<f64>, Issue)> = issues
            .list()
            .into_iter()
            .filter(|i| statuses.as_ref().is_none_or(|st| st.contains(&i.status)))
            .filter(|i| service.as_deref().is_none_or(|sv| i.service == sv))
            .filter(words)
            .filter_map(|i| match counts.get(i.id.as_str()) {
                Some(&c) => Some((Some(c), i)),
                None if all && env.is_none() => Some((None, i)),
                None => None,
            })
            .collect();
        out.sort_by(|a, b| b.0.unwrap_or(-1.0).total_cmp(&a.0.unwrap_or(-1.0)).then(b.1.last_seen.cmp(&a.1.last_seen)).then(a.1.id.cmp(&b.1.id)));
        let total = out.len();
        out.truncate(limit);
        let list: Vec<serde_json::Value> = out.iter().map(|(c, i)| issue_json(i, *c)).collect();
        Ok(crate::http::server::json(StatusCode::OK, &serde_json::json!({ "from": from, "to": to, "total": total, "issues": list })))
    })
    .await
}

/// An issue: what it is, its errors over the range (a day unless asked) by bucket of some sixty,
/// by version and by env, its latest error in the range whole (its stack, its trace), its activity.
pub async fn detail(id: &str, q: &HashMap<String, String>) -> Result<crate::http::server::Response, super::api::Fail> {
    use http::StatusCode;
    let issue = super::api::issues()?.get(id).ok_or_else(|| super::api::fail(StatusCode::NOT_FOUND, "no such issue"))?;
    let (from, to) = super::apm::range(q, 86_400_000)?;
    let step = step_of(to.saturating_sub(from));
    let start = from - from.rem_euclid(step);
    super::api::reading(move |s| {
        let failed = |e: kiln_obs::store::StoreError| super::api::fail(StatusCode::INTERNAL_SERVER_ERROR, e);
        let timeline = kiln_obs::errors::timeline(s, &issue.id, start, to, step).map_err(failed)?;
        let seen = kiln_obs::errors::seen(s, &[("issue.id", issue.id.as_str())], from, to).map_err(failed)?;
        let mut versions: BTreeMap<&str, f64> = BTreeMap::new();
        let mut envs: BTreeMap<&str, f64> = BTreeMap::new();
        for x in &seen {
            *versions.entry(x.version.as_str()).or_default() += x.errors;
            *envs.entry(x.env.as_str()).or_default() += x.errors;
        }
        let total: f64 = seen.iter().map(|x| x.errors).sum();
        // Its latest error in the range, whole.
        let query = format!("@issue.id:{}", issue.id);
        let (a, b) = (from.saturating_mul(1_000_000), to.saturating_mul(1_000_000).saturating_add(999_999));
        let latest = match issue.source.as_str() {
            "log" => kiln_obs::logs::search::parse(&query).ok().and_then(|q| s.search_logs(&q, a, b, 1).ok()),
            _ => kiln_obs::logs::search::parse_spans(&query).ok().and_then(|q| s.search_spans(&q, a, b, 1).ok()),
        }
        .and_then(|f| f.records.into_iter().next());
        let by = |m: &BTreeMap<&str, f64>, key: &str| -> Vec<serde_json::Value> {
            let mut v: Vec<(&&str, &f64)> = m.iter().collect();
            v.sort_by(|x, y| y.1.total_cmp(x.1));
            v.into_iter().map(|(k, n)| serde_json::json!({ key: k, "errors": n })).collect()
        };
        Ok(crate::http::server::json(
            StatusCode::OK,
            &serde_json::json!({
                "issue": issue_json(&issue, Some(total)),
                "from": from,
                "to": to,
                "timeline": { "from": start, "step": step, "values": timeline.iter().map(|b| b.1).collect::<Vec<_>>() },
                "versions": by(&versions, "version"),
                "envs": by(&envs, "env"),
                "latest": latest.as_ref().map(super::api::RecordOut),
            }),
        ))
    })
    .await
}

/// A user's change of an issue's status (`{"status": "for_review" | "reviewed" | "resolved" |
/// "ignored"}`): the issue as it is now.
pub fn update(id: &str, body: &serde_json::Value) -> Result<crate::http::server::Response, super::api::Fail> {
    use http::StatusCode;
    let status =
        body.get("status").and_then(serde_json::Value::as_str).and_then(Status::parse).ok_or_else(|| super::api::fail(StatusCode::BAD_REQUEST, "status: for_review, reviewed, resolved or ignored"))?;
    let issue = super::api::issues()?.set_status(id, status, super::clock_ms()).ok_or_else(|| super::api::fail(StatusCode::NOT_FOUND, "no such issue"))?;
    Ok(crate::http::server::json(StatusCode::OK, &issue_json(&issue, None)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_obs::errors::Seen;

    fn seen(issue: &str, version: &str, first: i64, last: i64) -> Seen {
        Seen { issue: issue.into(), service: "shop".into(), env: "prod".into(), version: version.into(), errors: 1.0, first, last }
    }

    fn sample(id: &str, _: i64, _: i64) -> Option<Sample> {
        Some(Sample { source: "span".into(), kind: format!("{id}Error"), message: "bad item".into(), file: "/app/cart.py".into(), function: "total".into() })
    }

    fn issue(is: &Issues, id: &str) -> anyhow::Result<Issue> {
        is.get(id).ok_or_else(|| anyhow::anyhow!("no issue {id}"))
    }

    /// An issue comes for review with what its first error says; its first and last error, its
    /// versions and envs follow what the metric says, read again (a count that came late) or not.
    #[test]
    fn an_issue_comes_for_review_and_follows_its_counts() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let is = Issues::open(dir.path());
        let t = 1_800_000_000_000;
        is.scan(t, |_, _| vec![seen("a", "1.0", t - 30_000, t - 10_000)], sample);
        let a = issue(&is, "a")?;
        assert_eq!((a.status, a.kind.as_str(), a.function.as_str(), a.first_seen, a.last_seen), (Status::ForReview, "aError", "total", t - 30_000, t - 10_000));
        assert_eq!((a.versions.clone(), a.envs.clone(), a.activity.len()), (vec!["1.0".to_owned()], vec!["prod".to_owned()], 1));
        // Read again, the same windows and later ones of a new version: the same issue, followed.
        is.scan(t + 10_000, |_, _| vec![seen("a", "1.0", t - 30_000, t), seen("a", "1.1", t, t)], |_, _, _| None);
        let a = issue(&is, "a")?;
        assert_eq!((a.first_seen, a.last_seen, a.versions.clone(), a.activity.len()), (t - 30_000, t, vec!["1.0".to_owned(), "1.1".to_owned()], 1));
        assert_eq!(is.list().len(), 1);
        Ok(())
    }

    /// Resolved, an issue's error again in the version it was resolved in is not a regression (an
    /// old deploy still running); in a version it was not seen in, it is: for review again. A
    /// service of no versions: any error again. Ignored stays ignored.
    #[test]
    fn a_resolved_issue_erring_in_a_new_version_is_a_regression() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let is = Issues::open(dir.path());
        let t = 1_800_000_000_000;
        let none = kiln_obs::logs::metrics::NONE;
        is.scan(t, |_, _| vec![seen("a", "1.0", t - 30_000, t - 30_000), seen("b", none, t - 30_000, t - 30_000)], sample);
        for id in ["a", "b"] {
            assert_eq!(is.set_status(id, Status::Resolved, t + 1).map(|i| i.status), Some(Status::Resolved));
        }
        is.scan(t + 20_000, |_, _| vec![seen("a", "1.0", t + 10_000, t + 10_000)], |_, _, _| None);
        let a = issue(&is, "a")?;
        assert_eq!((a.status, a.regression), (Status::Resolved, false), "its old version still erring");
        is.scan(t + 30_000, |_, _| vec![seen("a", "1.1", t + 20_000, t + 20_000), seen("b", none, t + 20_000, t + 20_000)], |_, _, _| None);
        for id in ["a", "b"] {
            let i = issue(&is, id)?;
            assert_eq!((i.status, i.regression), (Status::ForReview, true), "{id}");
            assert!(i.activity.last().is_some_and(|a| a.by == "kiln" && a.why.starts_with("an error again")), "{id}: {:?}", i.activity);
        }
        is.set_status("a", Status::Ignored, t + 40_000);
        is.scan(t + 60_000, |_, _| vec![seen("a", "2.0", t + 50_000, t + 50_000)], |_, _, _| None);
        assert_eq!(issue(&is, "a")?.status, Status::Ignored);
        Ok(())
    }

    /// Fourteen days after its last error: resolved by kiln; kept, and read back as it was saved.
    #[test]
    fn fourteen_quiet_days_resolve_an_issue_and_what_is_kept_is_read_back() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let is = Issues::open(dir.path());
        let t = 1_800_000_000_000;
        let last = t - 10_000;
        is.scan(t, |_, _| vec![seen("a", "1.0", last, last)], sample);
        is.scan(last + RESOLVE_AFTER_MS, |_, _| Vec::new(), |_, _, _| None);
        assert_eq!(issue(&is, "a")?.status, Status::ForReview);
        is.scan(last + RESOLVE_AFTER_MS + 1, |_, _| Vec::new(), |_, _, _| None);
        let a = issue(&is, "a")?;
        assert_eq!((a.status, a.resolved_in.clone()), (Status::Resolved, vec!["1.0".to_owned()]));
        is.save();
        let again = Issues::open(dir.path());
        assert_eq!(again.get("a"), Some(a));
        assert_eq!(lock(&again.state).scanned_to, last + RESOLVE_AFTER_MS + 1);
        Ok(())
    }

    /// Past [`MAX_ISSUES`], the longest without an error among those resolved or ignored goes
    /// first.
    #[test]
    fn past_its_bound_the_quietest_resolved_issue_goes() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let is = Issues::open(dir.path());
        let t = 1_800_000_000_000;
        let many: Vec<Seen> = (0..MAX_ISSUES as i64).map(|i| seen(&format!("i{i}"), "1.0", t - 1_000_000 + i, t - 1_000_000 + i)).collect();
        is.scan(t, |_, _| many, |_, _, _| None);
        is.set_status("i5", Status::Resolved, t);
        is.scan(t + 10_000, |_, _| vec![seen("new", "1.0", t, t)], |_, _, _| None);
        assert_eq!(is.list().len(), MAX_ISSUES);
        assert!(is.get("i5").is_none() && is.get("new").is_some() && is.get("i0").is_some());
        Ok(())
    }
}
