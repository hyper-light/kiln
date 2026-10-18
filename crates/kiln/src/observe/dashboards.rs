//! Dashboards (`docs/observe.md` §11): what the server keeps of them, and the ones kiln brings.
//!
//! A dashboard is Datadog's, near enough: a title, template variables (`$host` in a widget's query
//! stands for `host:<value>`, or `*`), and widgets on a 12-column grid, each a kind with its queries
//! (`timeseries`, `query_value`, `toplist`, `table`), a log search (`logs`), an event search
//! (`events`), monitors' states (`monitors`), or text (`note`). The UI draws them; the API keeps
//! them (`<observe>/dashboards.json`). kiln's own dashboards (`builtin:…`: its hosts, CI and CD)
//! are always there and read-only: a copy is the way to change one.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Widgets a dashboard holds at the most, and bytes it may be.
const WIDGETS: usize = 100;
pub const BYTES: usize = 1 << 20;
const KINDS: &[&str] = &["timeseries", "query_value", "toplist", "table", "heatmap", "distribution", "logs", "events", "monitors", "note"];

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
    /// What the kind takes besides: a unit, a display (`line`, `bars`, `area`), how a query value or
    /// a toplist sums a series over the range (`aggregator`: `avg`, `sum`, `min`, `max`, `last`).
    #[serde(default)]
    pub options: Value,
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

    /// Creates or replaces a dashboard, checked: widgets of known kinds on the grid, at most
    /// [`WIDGETS`], each with an id.
    pub fn put(&self, mut d: Dashboard) -> Result<Dashboard, String> {
        if d.id.starts_with("builtin:") {
            return Err("kiln's own dashboards are read-only: copy one to change it".into());
        }
        if d.title.trim().is_empty() {
            return Err("a dashboard's title".into());
        }
        if d.widgets.len() > WIDGETS {
            return Err(format!("{} widgets: at most {WIDGETS}", d.widgets.len()));
        }
        let mut ids: std::collections::HashSet<String> = d.widgets.iter().map(|w| w.id.clone()).filter(|id| !id.is_empty()).collect();
        if ids.len() != d.widgets.iter().filter(|w| !w.id.is_empty()).count() {
            return Err("two widgets of one id".into());
        }
        for w in &mut d.widgets {
            if !KINDS.contains(&w.kind.as_str()) {
                return Err(format!("a widget of kind {:?}: one of {}", w.kind, KINDS.join(", ")));
            }
            if w.layout.w == 0 || w.layout.h == 0 || w.layout.x + w.layout.w > 12 {
                return Err("a widget's layout: on the grid's 12 columns, at least 1×1".into());
            }
            for q in &w.queries {
                // A query with template variables in it is checked once they are filled with `*`.
                let filled = d.template_variables.iter().fold(q.clone(), |q, v| q.replace(&format!("${}", v.name), "*"));
                kiln_obs::query::parse::parse(&filled).map_err(|e| format!("a widget's query {q:?}: {e}"))?;
            }
            if w.id.is_empty() {
                w.id = super::fresh_id("w", |id| ids.contains(id));
                ids.insert(w.id.clone());
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

fn widget(title: &str, kind: &str, queries: &[&str], (x, y, w, h): (u32, u32, u32, u32), options: Value) -> Widget {
    Widget { id: format!("{}-{x}-{y}", kind), title: title.into(), kind: kind.into(), queries: queries.iter().map(|q| (*q).to_owned()).collect(), search: String::new(), text: String::new(), layout: Layout { x, y, w, h }, options }
}

fn search_widget(title: &str, kind: &str, search: &str, (x, y, w, h): (u32, u32, u32, u32)) -> Widget {
    Widget { id: format!("{}-{x}-{y}", kind), title: title.into(), kind: kind.into(), queries: Vec::new(), search: search.into(), text: String::new(), layout: Layout { x, y, w, h }, options: Value::Null }
}

/// kiln's own dashboards: its hosts, its CI, its CD.
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
        let bad = Dashboard { id: String::new(), title: "x".into(), description: String::new(), template_variables: Vec::new(), widgets: vec![Widget { kind: "pie".into(), ..widget("", "note", &[], (0, 0, 1, 1), Value::Null) }], builtin: false, created: 0, modified: 0 };
        anyhow::ensure!(d.put(bad).is_err(), "an unknown kind is refused");
        let reopened = Dashboards::open(dir.path());
        anyhow::ensure!(reopened.list().len() == builtins().len() * 2, "kept across a restart");
        Ok(())
    }
}
