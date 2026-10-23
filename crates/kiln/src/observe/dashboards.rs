//! Dashboards (`docs/observe.md` §11): what the server keeps of them, and the ones kiln brings.
//!
//! A dashboard is Datadog's, near enough: a title, template variables (`$host` in a widget's query
//! stands for `host:<value>`, or `*`; `$host.value` for the value alone), and widgets on a
//! 12-column grid, each a kind with its queries (`timeseries`, `query_value`, `toplist`, `table`,
//! `heatmap`, `distribution`, `pie`), a log search (`logs`), an event search (`events`), monitors'
//! states (`monitors`), text (`note`), or widgets of its own (`group`, Datadog's: a titled set of
//! widgets on a grid of their own, no group among them). The UI draws them; the API keeps them
//! (`<observe>/dashboards.json`), and takes and gives Datadog's dashboard JSON (`super::ddash`).
//! kiln's own dashboards (`builtin:…`: its hosts, CI, CD, repositories' sync, blobs, agents and
//! their live feed) are always there and read-only: a copy is the way to change one.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Widgets a dashboard holds at the most, its groups' among them: the largest of Datadog's 678
/// integration dashboards holds 126 (`super::ddash`'s corpus). And bytes it may be.
pub const WIDGETS: usize = 200;
pub const BYTES: usize = 1 << 20;
pub const KINDS: &[&str] = &["timeseries", "query_value", "toplist", "table", "heatmap", "distribution", "pie", "logs", "events", "monitors", "check", "note", "group"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Dashboard {
    #[serde(default)]
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub template_variables: Vec<TemplateVariable>,
    #[serde(default)]
    pub widgets: Vec<Widget>,
    #[serde(default)]
    pub builtin: bool,
    #[serde(default)]
    pub created: i64,
    #[serde(default)]
    pub modified: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TemplateVariable {
    /// `$name` in queries.
    pub name: String,
    /// The tag it filters by (`host`).
    pub tag: String,
    #[serde(default = "star")]
    pub default: String,
}

fn star() -> String {
    "*".into()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Widget {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: String,
    pub kind: String,
    /// Metric queries (`timeseries`, `query_value`, `toplist`, `table`).
    #[serde(default)]
    pub queries: Vec<String>,
    /// A log or event search (`logs`, `events`).
    #[serde(default)]
    pub search: String,
    /// Markdown (`note`).
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub layout: Layout,
    /// What the kind takes besides: a unit, a display (`line`, `bars`, `area`), how a query value, a
    /// toplist or a pie sums a series over the range (`aggregator`: `avg`, `sum`, `min`, `max`,
    /// `last`), how many a toplist shows (`limit`) and in which `order` (`desc`, `asc`).
    #[serde(default)]
    pub options: Value,
    /// A group's widgets, on its own grid.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub widgets: Vec<Widget>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Layout {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Default for Layout {
    fn default() -> Self {
        Self { x: 0, y: 0, w: 6, h: 4 }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

pub struct Dashboards {
    path: PathBuf,
    all: Mutex<BTreeMap<String, Dashboard>>,
}

impl Dashboards {
    pub fn open(dir: &Path) -> Self {
        let path = dir.join("dashboards.json");
        let all = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Self { path, all: Mutex::new(all) }
    }

    /// Every dashboard: kiln's own first, then the kept ones by title.
    pub fn list(&self) -> Vec<Dashboard> {
        let mut out = builtins();
        let mut kept: Vec<Dashboard> = lock(&self.all).values().cloned().collect();
        kept.sort_by_cached_key(|d| d.title.to_lowercase());
        out.extend(kept);
        out
    }

    pub fn get(&self, id: &str) -> Option<Dashboard> {
        if id.starts_with("builtin:") {
            return builtins().into_iter().find(|d| d.id == id);
        }
        lock(&self.all).get(id).cloned()
    }

    /// Creates or replaces a dashboard, checked: widgets of known kinds on the grid (a group's on
    /// its own, no group in a group), at most [`WIDGETS`] with its groups', each with an id of its
    /// own.
    pub fn put(&self, mut d: Dashboard) -> Result<Dashboard, String> {
        if d.id.starts_with("builtin:") {
            return Err("kiln's own dashboards are read-only: copy one to change it".into());
        }
        if d.title.trim().is_empty() {
            return Err("a dashboard's title".into());
        }
        let n = d.widgets.iter().map(|w| 1 + w.widgets.len()).sum::<usize>();
        if n > WIDGETS {
            return Err(format!("{n} widgets: at most {WIDGETS}"));
        }
        let every = || d.widgets.iter().flat_map(|w| std::iter::once(w).chain(&w.widgets));
        let mut ids: std::collections::HashSet<String> = every().map(|w| w.id.clone()).filter(|id| !id.is_empty()).collect();
        if ids.len() != every().filter(|w| !w.id.is_empty()).count() {
            return Err("two widgets of one id".into());
        }
        let vars = d.template_variables.clone();
        for w in &mut d.widgets {
            check(w, &vars, false, &mut ids)?;
            for c in &mut w.widgets {
                check(c, &vars, true, &mut ids)?;
            }
        }
        if serde_json::to_vec(&d).map_or(0, |b| b.len()) > BYTES {
            return Err("a dashboard of more than 1 MiB".into());
        }
        let now = crate::util::now_ms();
        let mut all = lock(&self.all);
        match all.get(&d.id) {
            Some(old) => d.created = old.created,
            None => {
                if d.id.is_empty() {
                    d.id = super::fresh_id("d", |id| all.contains_key(id));
                }
                d.created = now;
            }
        }
        d.modified = now;
        d.builtin = false;
        all.insert(d.id.clone(), d.clone());
        let bytes = serde_json::to_vec_pretty(&*all).map_err(|e| e.to_string())?;
        drop(all);
        crate::sync::group::save_atomic(&self.path, &bytes).map_err(|e| e.to_string())?;
        Ok(d)
    }

    pub fn delete(&self, id: &str) -> Result<bool, String> {
        let mut all = lock(&self.all);
        let had = all.remove(id).is_some();
        let bytes = serde_json::to_vec_pretty(&*all).map_err(|e| e.to_string())?;
        drop(all);
        crate::sync::group::save_atomic(&self.path, &bytes).map_err(|e| e.to_string())?;
        Ok(had)
    }
}

/// A widget checked (`in_group`: a group's): of a known kind, on its grid's 12 columns, its queries
/// parsed (template variables filled with `*`), a group's widgets a group's alone; given an id
/// when it has none.
fn check(w: &mut Widget, vars: &[TemplateVariable], in_group: bool, ids: &mut std::collections::HashSet<String>) -> Result<(), String> {
    if !KINDS.contains(&w.kind.as_str()) {
        return Err(format!("a widget of kind {:?}: one of {}", w.kind, KINDS.join(", ")));
    }
    if w.layout.w == 0 || w.layout.h == 0 || w.layout.x + w.layout.w > 12 {
        return Err("a widget's layout: on the grid's 12 columns, at least 1×1".into());
    }
    if w.kind == "group" && in_group {
        return Err("a group in a group: a group's widgets are of other kinds".into());
    }
    if w.kind != "group" && !w.widgets.is_empty() {
        return Err(format!("a {} widget with widgets of its own: a group's alone", w.kind));
    }
    for q in &w.queries {
        let filled = filled_with_any(q, vars);
        kiln_obs::query::parse::parse(&filled).map_err(|e| format!("a widget's query {q:?}: {e}"))?;
    }
    // A service check's status (Datadog's check status widget): its check, how it groups, and the
    // tags of its runs and groups.
    if w.kind == "check" {
        let o = &w.options;
        if o.get("check").and_then(Value::as_str).is_none_or(|c| c.trim().is_empty()) {
            return Err("a check widget's check: a service check's name".into());
        }
        if !matches!(o.get("grouping").and_then(Value::as_str), None | Some("cluster" | "check")) {
            return Err("a check widget's grouping: cluster (its groups') or check (one group's)".into());
        }
        for key in ["group_by", "tags"] {
            match o.get(key) {
                None | Some(Value::Null) => {}
                Some(Value::Array(a)) if a.len() <= 32 && a.iter().all(Value::is_string) => {}
                Some(_) => return Err(format!("a check widget's {key}: a list of tags, 32 at the most")),
            }
        }
    }
    if w.id.is_empty() {
        w.id = super::fresh_id("w", |id| ids.contains(id));
        ids.insert(w.id.clone());
    }
    Ok(())
}

/// A query with its template variables filled with `*` (`$host.value` and `$host` alike), to be
/// checked.
pub fn filled_with_any(q: &str, vars: &[TemplateVariable]) -> String {
    let mut by_length: Vec<&TemplateVariable> = vars.iter().collect();
    by_length.sort_by_key(|v| std::cmp::Reverse(v.name.len()));
    by_length.iter().fold(q.to_owned(), |q, v| q.replace(&format!("${}.value", v.name), "*").replace(&format!("${}", v.name), "*"))
}

fn widget(title: &str, kind: &str, queries: &[&str], (x, y, w, h): (u32, u32, u32, u32), options: Value) -> Widget {
    Widget {
        id: format!("{}-{x}-{y}", kind),
        title: title.into(),
        kind: kind.into(),
        queries: queries.iter().map(|q| (*q).to_owned()).collect(),
        search: String::new(),
        text: String::new(),
        layout: Layout { x, y, w, h },
        options,
        widgets: Vec::new(),
    }
}

fn search_widget(title: &str, kind: &str, search: &str, (x, y, w, h): (u32, u32, u32, u32)) -> Widget {
    Widget { id: format!("{}-{x}-{y}", kind), title: title.into(), kind: kind.into(), queries: Vec::new(), search: search.into(), text: String::new(), layout: Layout { x, y, w, h }, options: Value::Null, widgets: Vec::new() }
}

/// kiln's own dashboards: its hosts, its CI, its CD, its repositories' sync, the store it keeps its
/// bytes in, its agents, and their sessions' live feed.
pub fn builtins() -> Vec<Dashboard> {
    let host = vec![TemplateVariable { name: "host".into(), tag: "host".into(), default: "*".into() }];
    vec![
        Dashboard {
            id: "builtin:hosts".into(),
            title: "Hosts".into(),
            description: "Every node kiln runs on: CPU, memory, load, disks, network, and the containers they run in.".into(),
            template_variables: host.clone(),
            widgets: vec![
                widget("CPU user %", "timeseries", &["avg:system.cpu.user{$host} by {host}"], (0, 0, 6, 4), json!({ "unit": "%" })),
                widget("CPU system %", "timeseries", &["avg:system.cpu.system{$host} by {host}"], (6, 0, 6, 4), json!({ "unit": "%" })),
                widget("Memory usable", "timeseries", &["avg:system.mem.usable{$host} by {host}"], (0, 4, 6, 4), json!({ "unit": "MiB" })),
                widget("Load per core (1 min)", "timeseries", &["avg:system.load.norm.1{$host} by {host}"], (6, 4, 6, 4), json!({})),
                widget("Disk in use", "toplist", &["max:system.disk.in_use{$host} by {host,mount}"], (0, 8, 6, 4), json!({ "unit": "fraction" })),
                widget("Network in (bytes/s)", "timeseries", &["sum:system.net.bytes_rcvd{$host} by {host}"], (6, 8, 6, 4), json!({ "unit": "B/s" })),
                widget("Container memory (bytes)", "timeseries", &["avg:container.memory.usage{$host} by {host}"], (0, 12, 6, 4), json!({ "unit": "B" })),
                widget("kiln processes' memory (bytes)", "timeseries", &["avg:kiln.process.rss_bytes{$host} by {host,role}"], (6, 12, 6, 4), json!({ "unit": "B" })),
            ],
            builtin: true,
            created: 0,
            modified: 0,
        },
        Dashboard {
            id: "builtin:ci".into(),
            title: "kiln CI".into(),
            description: "Jobs and runs as they finish: how many, how long, how many failed, and where they waited.".into(),
            template_variables: vec![TemplateVariable { name: "repo".into(), tag: "repo".into(), default: "*".into() }],
            widgets: vec![
                widget("Jobs finished", "timeseries", &["sum:kiln.ci.jobs.finished{$repo} by {status}.as_count()"], (0, 0, 6, 4), json!({ "display": "bars" })),
                widget("Failed jobs", "query_value", &["sum:kiln.ci.jobs.finished{$repo,status:failed}.as_count()"], (6, 0, 3, 4), json!({ "aggregator": "sum" })),
                widget("Longest job", "query_value", &["max:kiln.ci.job.duration{$repo}"], (9, 0, 3, 4), json!({ "unit": "s", "aggregator": "max" })),
                widget("Job duration by job (s)", "timeseries", &["avg:kiln.ci.job.duration{$repo} by {job}"], (0, 4, 6, 4), json!({ "unit": "s" })),
                widget("Queue time by runner (s)", "timeseries", &["avg:kiln.ci.job.queue_time{$repo} by {runner}"], (6, 4, 6, 4), json!({ "unit": "s" })),
                widget("Slowest jobs", "toplist", &["max:kiln.ci.job.duration{$repo} by {repo,job}"], (0, 8, 6, 4), json!({ "unit": "s" })),
                search_widget("CI events", "events", "service:kiln.ci", (6, 8, 6, 4)),
            ],
            builtin: true,
            created: 0,
            modified: 0,
        },
        Dashboard {
            id: "builtin:cd".into(),
            title: "kiln CD".into(),
            description: "Deployments: what was deployed, how it went, and every app's health.".into(),
            template_variables: vec![TemplateVariable { name: "app".into(), tag: "app".into(), default: "*".into() }],
            widgets: vec![
                widget("Deploys", "timeseries", &["sum:kiln.cd.deploys{$app} by {result}.as_count()"], (0, 0, 6, 4), json!({ "display": "bars" })),
                widget("Apps by health", "timeseries", &["sum:kiln.cd.apps{*} by {health}"], (6, 0, 6, 4), json!({})),
                search_widget("Deploys and status changes", "events", "service:kiln.cd", (0, 4, 12, 5)),
            ],
            builtin: true,
            created: 0,
            modified: 0,
        },
        Dashboard {
            id: "builtin:sync".into(),
            title: "Repo sync".into(),
            description: "Repositories kept in step across machines: what is committed, how far each copy is behind and how it is, what was dropped or conflicted, and agents' moves.".into(),
            template_variables: vec![TemplateVariable { name: "repo".into(), tag: "repo".into(), default: "*".into() }],
            widgets: vec![
                widget("Steps committed", "timeseries", &["sum:kiln_sync_steps_committed_total{$repo} by {repo}.as_count()"], (0, 0, 4, 4), json!({ "display": "bars" })),
                widget("Changes", "timeseries", &["sum:kiln_sync_changes_total{$repo} by {kind}.as_count()"], (4, 0, 4, 4), json!({ "display": "bars" })),
                widget("Commit time, p95 (s)", "timeseries", &["p95:kiln_sync_commit_seconds{$repo} by {repo}"], (8, 0, 4, 4), json!({ "unit": "s" })),
                widget("Copies behind (steps)", "timeseries", &["max:kiln_sync_copy_lag_steps{$repo} by {repo,machine}"], (0, 4, 4, 4), json!({})),
                widget("Copies in error or offline", "toplist", &["max:kiln_sync_copy_state{$repo,state:error} by {repo,machine}", "max:kiln_sync_copy_state{$repo,state:offline} by {repo,machine}"], (4, 4, 4, 4), json!({})),
                widget("Since a copy was heard (s)", "timeseries", &["max:kiln_sync_copy_heard_age_seconds{$repo} by {machine}"], (8, 4, 4, 4), json!({ "unit": "s" })),
                widget("Dropped, by reason", "timeseries", &["sum:kiln_sync_dropped_total{$repo} by {reason}.as_count()"], (0, 8, 4, 4), json!({ "display": "bars" })),
                widget("Conflicts", "timeseries", &["sum:kiln_sync_conflicts_total{$repo} by {repo}.as_count()", "max:kiln_sync_conflict_files{$repo} by {repo}"], (4, 8, 4, 4), json!({})),
                widget("Errors", "timeseries", &["sum:kiln_sync_errors_total{$repo} by {kind}.as_count()"], (8, 8, 4, 4), json!({ "display": "bars" })),
                widget("Applied after a change, p95 (s)", "timeseries", &["p95:kiln_sync_apply_lag_seconds{$repo} by {repo}"], (0, 12, 4, 4), json!({ "unit": "s" })),
                widget("Transfers", "timeseries", &["sum:kiln_sync_transfers_total{$repo} by {result}.as_count()"], (4, 12, 4, 4), json!({ "display": "bars" })),
                widget("Contents moved (bytes/s)", "timeseries", &["sum:kiln_sync_blob_bytes_total{*} by {direction}"], (8, 12, 4, 4), json!({ "unit": "B/s" })),
                widget("Moves", "timeseries", &["sum:kiln_moves_total{*} by {result}.as_count()"], (0, 16, 4, 4), json!({ "display": "bars" })),
                widget("A move's barrier, p95 (s)", "timeseries", &["p95:kiln_move_barrier_seconds{*}", "p95:kiln_move_files_wait_seconds{*}"], (4, 16, 4, 4), json!({ "unit": "s" })),
                search_widget("Sync's events", "events", "service:kiln.sync", (8, 16, 4, 4)),
            ],
            builtin: true,
            created: 0,
            modified: 0,
        },
        Dashboard {
            id: "builtin:blobs".into(),
            title: "Blobs".into(),
            description: "The store kiln keeps its bytes in: its requests by operation and how they went, how long they took, the bytes moved, and what was let go.".into(),
            template_variables: vec![TemplateVariable { name: "store".into(), tag: "store".into(), default: "*".into() }],
            widgets: vec![
                widget("Requests, by operation", "timeseries", &["sum:kiln_blobs_requests_total{$store} by {op}.as_count()"], (0, 0, 4, 4), json!({ "display": "bars" })),
                widget(
                    "Refused and unavailable",
                    "timeseries",
                    &["sum:kiln_blobs_requests_total{$store,outcome:refused} by {op}.as_count()", "sum:kiln_blobs_requests_total{$store,outcome:unavailable} by {op}.as_count()"],
                    (4, 0, 4, 4),
                    json!({ "display": "bars" }),
                ),
                widget("A request's time, p50 and p99 (s)", "timeseries", &["p50:kiln_blobs_request_seconds{$store} by {op}", "p99:kiln_blobs_request_seconds{$store} by {op}"], (8, 0, 4, 4), json!({ "unit": "s" })),
                widget("Bytes moved (bytes/s)", "timeseries", &["sum:kiln_blobs_bytes_total{$store} by {direction}"], (0, 4, 4, 4), json!({ "unit": "B/s" })),
                widget("Let go, by what kept them", "timeseries", &["sum:kiln_blobs_collected_total{*} by {kind}.as_count()"], (4, 4, 4, 4), json!({ "display": "bars" })),
                widget("Archives given up", "query_value", &["sum:kiln_blobs_archives_given_up_total{*}.as_count()"], (8, 4, 4, 4), json!({ "aggregator": "sum" })),
            ],
            builtin: true,
            created: 0,
            modified: 0,
        },
        Dashboard {
            id: "builtin:agents".into(),
            title: "kiln agents".into(),
            description: "Agents' goals and sessions: goals sent and how they ended, each harness host's looks, its records and its journal, and messages sent to sessions: each taken once, a repeat answered as the first was and never delivered again, those refused and why, those not answered in time.".into(),
            template_variables: host.clone(),
            widgets: vec![
                // The goal driver is the server's: its counts are the server's alone.
                widget("Goals sent", "timeseries", &["sum:kiln_goals_sent_total{*} by {kind}.as_count()"], (0, 0, 4, 4), json!({ "display": "bars" })),
                widget("Goals ended", "timeseries", &["sum:kiln_goals_ended_total{*} by {status}.as_count()"], (4, 0, 4, 4), json!({ "display": "bars" })),
                widget("Harness looks, by host", "timeseries", &["sum:kiln_harness_polls_total{$host} by {host}.as_count()"], (8, 0, 4, 4), json!({ "display": "bars" })),
                widget("Records written, by host", "timeseries", &["sum:kiln_harness_records_written_total{$host} by {host}.as_count()"], (0, 4, 4, 4), json!({ "display": "bars" })),
                widget(
                    "Journal commits, and those that could not be written",
                    "timeseries",
                    &["sum:kiln_harness_journal_commits_total{$host} by {outcome}.as_count()", "sum:kiln_harness_journal_rehanded_total{$host}.as_count()"],
                    (4, 4, 4, 4),
                    json!({ "display": "bars" }),
                ),
                widget("A journal commit, p50 and p99 (s)", "timeseries", &["p50:kiln_harness_journal_commit_seconds{$host} by {host}", "p99:kiln_harness_journal_commit_seconds{$host} by {host}"], (8, 4, 4, 4), json!({ "unit": "s" })),
                widget("Messages taken", "timeseries", &["sum:kiln_sends_accepted_total{*}.as_count()"], (0, 8, 4, 4), json!({ "display": "bars" })),
                widget("Repeats, answered and not delivered again", "timeseries", &["sum:kiln_sends_repeated_total{*}.as_count()"], (4, 8, 4, 4), json!({ "display": "bars" })),
                widget(
                    "Not answered within 10 s, and handed on unchecked",
                    "timeseries",
                    &["sum:kiln_sends_unanswered_total{*}.as_count()", "sum:kiln_sends_unchecked_total{*}.as_count()"],
                    (8, 8, 4, 4),
                    json!({ "display": "bars" }),
                ),
                widget("Messages refused, by reason", "timeseries", &["sum:kiln_sends_refused_total{*} by {reason}.as_count()"], (0, 12, 6, 4), json!({ "display": "bars" })),
                widget("An inbox's take, p50 and p99 (s)", "timeseries", &["p50:kiln_sends_inbox_seconds{$host} by {host}", "p99:kiln_sends_inbox_seconds{$host} by {host}"], (6, 12, 6, 4), json!({ "unit": "s" })),
            ],
            builtin: true,
            created: 0,
            modified: 0,
        },
        Dashboard {
            id: "builtin:live-feed".into(),
            title: "Live feed".into(),
            description: "Agent sessions' live feeds on kiln's bus, server to every browser watching: frames published, how long one took to reach a viewer, viewers that fell behind or were refused and started over, the bus's topics and viewers, and its memory against its limit.".into(),
            template_variables: host.clone(),
            widgets: vec![
                widget("Frames published", "timeseries", &["sum:kiln_bus_published_total{$host}.as_count()"], (0, 0, 4, 4), json!({ "display": "bars" })),
                widget("A frame's delivery, p50 and p99 (s)", "timeseries", &["p50:kiln_bus_delivery_seconds{$host}", "p99:kiln_bus_delivery_seconds{$host}"], (4, 0, 4, 4), json!({ "unit": "s" })),
                widget(
                    "Viewers behind and resynced, and refused by operation",
                    "timeseries",
                    &["sum:kiln_bus_lag_notices_total{$host}.as_count()", "sum:kiln_bus_refused_total{$host} by {op}.as_count()"],
                    (8, 0, 4, 4),
                    json!({ "display": "bars" }),
                ),
                widget("Viewers and topics", "timeseries", &["max:kiln_bus_viewers{$host}", "max:kiln_bus_topics{$host}"], (0, 4, 6, 4), json!({})),
                widget("Memory against its limit (bytes)", "timeseries", &["max:kiln_bus_memory_bytes{$host}", "max:kiln_bus_memory_limit_bytes{$host}"], (6, 4, 6, 4), json!({ "unit": "B" })),
            ],
            builtin: true,
            created: 0,
            modified: 0,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kiln_s_own_dashboards_are_valid_and_read_only() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let d = Dashboards::open(dir.path());
        for b in builtins() {
            // Each would be taken as a dashboard of its own.
            let copy = Dashboard { id: String::new(), builtin: false, ..b.clone() };
            d.put(copy).map_err(|e| anyhow::anyhow!("{}: {e}", b.title))?;
            anyhow::ensure!(d.put(b.clone()).is_err(), "a builtin is read-only");
        }
        anyhow::ensure!(d.list().len() == builtins().len() * 2);
        let bad = Dashboard { id: String::new(), title: "x".into(), description: String::new(), template_variables: Vec::new(), widgets: vec![Widget { kind: "flame_graph".into(), ..widget("", "note", &[], (0, 0, 1, 1), Value::Null) }], builtin: false, created: 0, modified: 0 };
        anyhow::ensure!(d.put(bad).is_err(), "an unknown kind is refused");
        let reopened = Dashboards::open(dir.path());
        anyhow::ensure!(reopened.list().len() == builtins().len() * 2, "kept across a restart");
        Ok(())
    }
}
