//! Incident management (`docs/observe.md` §9.5), Datadog's: an incident declared (through the API,
//! from kiln's pages, or by a monitor whose message names `@incident`), its severity (`UNKNOWN`,
//! `SEV-0` to `SEV-5`) and state (active, stable, resolved), its commander, its customer impact,
//! its fields (a root cause, a summary, services, teams, how it was found), the handles it tells
//! (kiln's integrations: `@slack-ops`, `@pagerduty-oncall`, …), its timeline (notes, each change,
//! its monitor's alerts and recoveries), its todos and its attachments (a postmortem, links); and
//! Datadog's figures of it: how long to detect, to respond, to repair and to resolve.
//!
//! The server keeps them, each one a file of its own written whole as it changes
//! (`<observe>/incidents/<public id>.json`), bounded ([`INCIDENTS`] of them, [`BYTES`] of them: the
//! oldest resolved let go first; each timeline [`CELLS`] cells).

pub mod http;
pub mod search;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Severities, Datadog's, the most severe first after `UNKNOWN`.
pub const SEVERITIES: [&str; 7] = ["UNKNOWN", "SEV-0", "SEV-1", "SEV-2", "SEV-3", "SEV-4", "SEV-5"];
/// States, Datadog's.
pub const STATES: [&str; 3] = ["active", "stable", "resolved"];
/// The field types whose value is one text, and those whose value is a list.
pub const SINGLE: [&str; 2] = ["dropdown", "textbox"];
pub const MULTI: [&str; 4] = ["multiselect", "textarray", "metrictag", "autocomplete"];

/// Incidents kept at the most, and their bytes on disk (about what they take in memory): past
/// either, the oldest resolved let go; none to let go, none new.
pub const INCIDENTS: usize = 10_000;
pub const BYTES: usize = 64 << 20;
/// A timeline's cells, an incident's todos, attachments, fields and handles, at the most.
pub const CELLS: usize = 1_000;
pub const TODOS: usize = 200;
pub const ATTACHMENTS: usize = 100;
pub const FIELDS: usize = 64;
pub const HANDLES: usize = 32;
/// Texts' bounds: a title, a scope or a field's value, a note.
pub const TITLE: usize = 512;
pub const TEXT: usize = 4_096;
pub const NOTE: usize = 64 << 10;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A field: Datadog's, `{"type": "dropdown", "value": "SEV-2"}` or a list's
/// `{"type": "multiselect", "value": ["checkout"]}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Field {
    #[serde(rename = "type")]
    pub kind: String,
    pub value: Value,
}

impl Field {
    /// Whether it is one Datadog takes: a type it has, a value of its shape (text, or a list of
    /// texts; none, cleared), within bounds.
    pub fn check(&self) -> Result<(), String> {
        let text = |s: &str| s.len() <= TEXT;
        match (&self.value, self.kind.as_str()) {
            (Value::Null, k) if SINGLE.contains(&k) || MULTI.contains(&k) => Ok(()),
            (Value::String(s), k) if SINGLE.contains(&k) && text(s) => Ok(()),
            (Value::Array(a), k) if MULTI.contains(&k) && a.len() <= 64 && a.iter().all(|v| v.as_str().is_some_and(text)) => Ok(()),
            _ => Err(format!("a field of type {}: dropdown or textbox a text; multiselect, textarray, metrictag or autocomplete a list of texts (each at most {TEXT} bytes)", self.kind)),
        }
    }

    /// Its value's texts.
    pub fn values(&self) -> Vec<&str> {
        match &self.value {
            Value::String(s) => vec![s.as_str()],
            Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        }
    }
}

/// Whom an incident tells: a handle as a monitor's message names an integration (`@slack-ops`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handle {
    #[serde(default)]
    pub display_name: String,
    pub handle: String,
}

/// What a timeline cell says.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cell_type", rename_all = "snake_case")]
pub enum Cell {
    /// A note, Markdown.
    Markdown {
        content: String,
        #[serde(default)]
        important: bool,
    },
    /// A change: its severity, state, commander or title, from what to what.
    Changed { what: String, from: String, to: String },
    /// Its monitor's group changed state: alerting again, recovered.
    Monitor {
        monitor_id: String,
        monitor: String,
        state: String,
        #[serde(default)]
        value: Option<f64>,
        #[serde(default)]
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimelineCell {
    pub id: String,
    /// Unix ms.
    pub created: i64,
    pub author: String,
    #[serde(flatten)]
    pub cell: Cell,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Todo {
    pub id: String,
    pub content: String,
    #[serde(default)]
    pub assignees: Vec<String>,
    /// As given (Datadog's: a date or a time).
    #[serde(default)]
    pub due_date: Option<String>,
    /// Unix ms it was done.
    #[serde(default)]
    pub completed: Option<i64>,
    pub created: i64,
    pub modified: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub id: String,
    /// `postmortem` or `link`.
    pub attachment_type: String,
    pub url: String,
    pub title: String,
    pub modified: i64,
}

/// The monitor that declared it, and its group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FromMonitor {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub group: Vec<(String, String)>,
}

/// An incident. Times are Unix ms.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Incident {
    pub id: String,
    pub public_id: u64,
    pub title: String,
    #[serde(default)]
    pub customer_impacted: bool,
    #[serde(default)]
    pub customer_impact_scope: Option<String>,
    #[serde(default)]
    pub customer_impact_start: Option<i64>,
    #[serde(default)]
    pub customer_impact_end: Option<i64>,
    pub severity: String,
    pub state: String,
    /// Its other fields, by name.
    #[serde(default)]
    pub fields: BTreeMap<String, Field>,
    #[serde(default)]
    pub commander: Option<String>,
    #[serde(default)]
    pub notification_handles: Vec<Handle>,
    pub created: i64,
    pub modified: i64,
    pub detected: i64,
    pub declared: i64,
    #[serde(default)]
    pub resolved: Option<i64>,
    pub created_by: String,
    pub last_modified_by: String,
    #[serde(default)]
    pub is_test: bool,
    #[serde(default)]
    pub monitor: Option<FromMonitor>,
    #[serde(default)]
    pub timeline: Vec<TimelineCell>,
    #[serde(default)]
    pub todos: Vec<Todo>,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    /// The next number its cells', todos' and attachments' ids take.
    #[serde(default)]
    pub next: u64,
}

impl Incident {
    /// Datadog's figures of it, seconds (none below zero): how long from the customers' impact
    /// starting to its detection (to detect), from its detection to its declaring (to respond) and
    /// to the impact ending (to repair), and from its declaring to its resolving (to resolve); and
    /// the impact's duration.
    pub fn durations(&self) -> Durations {
        let s = |later: Option<i64>, earlier: Option<i64>| later.zip(earlier).map(|(a, b)| a.saturating_sub(b).max(0) / 1000);
        Durations {
            time_to_detect: s(Some(self.detected), self.customer_impact_start),
            time_to_internal_response: s(Some(self.created), Some(self.detected)),
            time_to_repair: s(self.customer_impact_end, Some(self.detected)),
            time_to_resolve: s(self.resolved, Some(self.created)),
            customer_impact_duration: s(self.customer_impact_end, self.customer_impact_start),
        }
    }

    fn next_id(&mut self, prefix: &str) -> String {
        self.next += 1;
        format!("{prefix}{}", self.next)
    }

    /// A cell on its timeline (its first cells kept past the bound: the declaring and its first
    /// notes; the oldest after them let go).
    pub fn add_cell(&mut self, created: i64, author: &str, cell: Cell) -> String {
        let id = self.next_id("c");
        if self.timeline.len() >= CELLS {
            self.timeline.remove(CELLS / 10);
        }
        self.timeline.push(TimelineCell { id: id.clone(), created, author: author.to_owned(), cell });
        id
    }

    /// Whether it has a postmortem.
    pub fn has_postmortem(&self) -> bool {
        self.attachments.iter().any(|a| a.attachment_type == "postmortem")
    }

    pub fn open(&self) -> bool {
        self.state != "resolved"
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Durations {
    pub time_to_detect: Option<i64>,
    pub time_to_internal_response: Option<i64>,
    pub time_to_repair: Option<i64>,
    pub time_to_resolve: Option<i64>,
    pub customer_impact_duration: Option<i64>,
}

/// What a declaring or a change asks: Datadog's attributes, each optional in a change.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Change {
    pub title: Option<String>,
    pub customer_impacted: Option<bool>,
    pub customer_impact_scope: Option<Option<String>>,
    pub customer_impact_start: Option<Option<i64>>,
    pub customer_impact_end: Option<Option<i64>>,
    pub detected: Option<i64>,
    pub severity: Option<String>,
    pub state: Option<String>,
    /// Fields set (a null value removes one).
    pub fields: BTreeMap<String, Field>,
    pub commander: Option<Option<String>>,
    pub notification_handles: Option<Vec<Handle>>,
    pub is_test: Option<bool>,
    /// Notes put first on a new one's timeline.
    pub initial_cells: Vec<(String, bool)>,
}

/// Why something was refused: what is wrong (400), or what is not there (404).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refused {
    #[error("{0}")]
    Invalid(String),
    #[error("no such incident")]
    NoIncident,
    #[error("no such {0}")]
    NoItem(&'static str),
    #[error("{0}")]
    Full(String),
}

fn invalid(s: impl Into<String>) -> Refused {
    Refused::Invalid(s.into())
}

/// A text within `max` bytes, cut at a character's end; trimmed.
fn bounded(s: &str, max: usize, what: &str) -> Result<String, Refused> {
    let t = s.trim();
    if t.len() > max {
        return Err(invalid(format!("{what}: {max} bytes at the most")));
    }
    Ok(t.to_owned())
}

fn check_handles(hs: &[Handle]) -> Result<Vec<Handle>, Refused> {
    if hs.len() > HANDLES {
        return Err(invalid(format!("notification_handles: {HANDLES} at the most")));
    }
    hs.iter()
        .map(|h| {
            let handle = bounded(&h.handle, 256, "a handle")?;
            if handle.is_empty() {
                return Err(invalid("a handle names whom it tells (@slack-ops)"));
            }
            Ok(Handle { display_name: bounded(&h.display_name, 256, "a handle's display_name")?, handle })
        })
        .collect()
}

/// The handles' mentions (`@slack-ops` → `slack-ops`), as a monitor's message names them.
pub fn mentions(hs: &[Handle]) -> Vec<String> {
    hs.iter().filter_map(|h| h.handle.trim().strip_prefix('@').map(str::to_owned).or_else(|| (!h.handle.contains('@')).then(|| h.handle.trim().to_owned()))).filter(|m| !m.is_empty()).collect()
}

/// What the server keeps in memory: each incident by its public id (the oldest first), their
/// public ids by id, each one's bytes on disk and all of theirs, the next public id.
#[derive(Debug, Default)]
struct Saved {
    incidents: BTreeMap<u64, Incident>,
    by_id: HashMap<String, u64>,
    sizes: HashMap<u64, usize>,
    bytes: usize,
    next_public_id: u64,
}

impl Saved {
    /// The public id of the incident `id` names: its id, or its public id.
    fn key(&self, id: &str) -> Option<u64> {
        match id.parse::<u64>() {
            Ok(p) => self.incidents.contains_key(&p).then_some(p),
            Err(_) => self.by_id.get(id).copied(),
        }
    }

    /// An incident kept as it is now, `bytes` of it on disk.
    fn put(&mut self, i: Incident, bytes: usize) {
        let p = i.public_id;
        if !self.by_id.contains_key(&i.id) {
            self.by_id.insert(i.id.clone(), p);
        }
        self.incidents.insert(p, i);
        self.bytes = self.bytes + bytes - self.sizes.insert(p, bytes).unwrap_or(0);
    }

    fn take(&mut self, p: u64) -> Option<Incident> {
        let i = self.incidents.remove(&p)?;
        self.by_id.remove(&i.id);
        self.bytes -= self.sizes.remove(&p).unwrap_or(0);
        Some(i)
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Next {
    next_public_id: u64,
}

/// A change told: the incident as it now is, and what changed (its declaring, its severity's or
/// state's change), for whom its handles name.
#[derive(Debug, Clone)]
pub struct Told {
    pub incident: Incident,
    pub what: &'static str,
}

/// The server's incidents: each one a file of its own (`<observe>/incidents/<public id>.json`,
/// written whole as it changes, and only it: an incident changes as people work it, not as data
/// comes), and the next public id (`next.json`: a removed one's never given again).
pub struct Incidents {
    dir: PathBuf,
    saved: Mutex<Saved>,
    /// Its bounds: [`INCIDENTS`] and [`BYTES`] (less in tests).
    bounds: (usize, usize),
    /// What a change tells (the server's notifier; none in tests).
    tell: OnceLock<Box<dyn Fn(Told) + Send + Sync>>,
}

static INCIDENTS_KEPT: OnceLock<Arc<Incidents>> = OnceLock::new();

pub fn get() -> Option<&'static Arc<Incidents>> {
    INCIDENTS_KEPT.get()
}

pub fn set(i: Arc<Incidents>) {
    let _ = INCIDENTS_KEPT.set(i);
}

impl Incidents {
    /// The incidents kept in `<dir>/incidents`: each file read (one that does not read said, and
    /// left as it is; what a write cut short left, removed).
    pub fn open(dir: &Path) -> Arc<Self> {
        Self::open_bounded(dir, INCIDENTS, BYTES)
    }

    fn open_bounded(dir: &Path, incidents: usize, bytes: usize) -> Arc<Self> {
        let dir = dir.join("incidents");
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::warn!(dir = %dir.display(), error = %e, "the incidents' folder was not made");
        }
        let mut saved = Saved::default();
        if let Ok(b) = std::fs::read(dir.join("next.json"))
            && let Ok(n) = serde_json::from_slice::<Next>(&b)
        {
            saved.next_public_id = n.next_public_id;
        }
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.extension().and_then(|x| x.to_str()).is_some_and(|x| x.starts_with("tmp-")) {
                let _ = std::fs::remove_file(&path);
                continue;
            }
            let Some(p) = path.file_stem().and_then(|s| s.to_str()).and_then(|s| s.parse::<u64>().ok()).filter(|_| path.extension().is_some_and(|x| x == "json")) else { continue };
            // Its id never given again, whether it reads or not (what does not read is not written over).
            saved.next_public_id = saved.next_public_id.max(p);
            match std::fs::read(&path).map_err(|e| e.to_string()).and_then(|b| serde_json::from_slice::<Incident>(&b).map(|i| (i, b.len())).map_err(|e| e.to_string())) {
                Ok((i, bytes)) => {
                    saved.next_public_id = saved.next_public_id.max(i.public_id);
                    saved.put(i, bytes);
                }
                Err(e) => tracing::warn!(file = %path.display(), error = %e, "an incident's file does not read: left as it is"),
            }
        }
        Arc::new(Self { dir, saved: Mutex::new(saved), bounds: (incidents, bytes), tell: OnceLock::new() })
    }

    /// What tells a change (once).
    pub fn on_change(&self, f: impl Fn(Told) + Send + Sync + 'static) {
        let _ = self.tell.set(Box::new(f));
    }

    fn told(&self, t: Told) {
        if let Some(f) = self.tell.get() {
            f(t);
        }
    }

    fn file(&self, public_id: u64) -> PathBuf {
        self.dir.join(format!("{public_id}.json"))
    }

    /// An incident written whole (atomically: as it was, or as it is): its bytes on disk.
    fn write(&self, i: &Incident) -> Result<usize, Refused> {
        let bytes = serde_json::to_vec(i).map_err(|e| invalid(e.to_string()))?;
        crate::sync::group::save_atomic(&self.file(i.public_id), &bytes).map_err(|e| Refused::Full(format!("the incident was not saved: {e}")))?;
        Ok(bytes.len())
    }

    /// Incident `p` let go: the next public id kept first (the newest's id never given again, its
    /// file gone), its file, then it.
    fn forget(&self, saved: &mut Saved, p: u64) -> Result<(), Refused> {
        let next = serde_json::to_vec(&Next { next_public_id: saved.next_public_id }).map_err(|e| invalid(e.to_string()))?;
        crate::sync::group::save_atomic(&self.dir.join("next.json"), &next).map_err(|e| Refused::Full(format!("the incident was not removed: {e}")))?;
        match std::fs::remove_file(self.file(p)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Refused::Full(format!("the incident was not removed: {e}"))),
        }
        saved.take(p);
        Ok(())
    }

    /// Room for one more of `bytes`, besides incident `keep`: the oldest resolved let go while the
    /// incidents number [`INCIDENTS`] or weigh [`BYTES`]; none to let go, refused (a declaring) or
    /// left over (a change).
    fn room(&self, saved: &mut Saved, more: usize, bytes: usize, keep: Option<u64>) -> Result<(), Refused> {
        let (most, weight) = self.bounds;
        while saved.incidents.len() + more > most || saved.bytes + bytes > weight {
            let Some(oldest) = saved.incidents.values().find(|i| !i.open() && Some(i.public_id) != keep).map(|i| i.public_id) else {
                return Err(Refused::Full(format!("{most} incidents, or {} MiB of them, open at once at the most", weight >> 20)));
            };
            tracing::info!(incident = oldest, "the oldest resolved incident let go: the incidents are at their bound");
            self.forget(saved, oldest)?;
        }
        Ok(())
    }

    /// Every incident, the newest first (a copy of each: [`Incidents::read_all`] reads them in place).
    pub fn list(&self) -> Vec<Incident> {
        lock(&self.saved).incidents.values().rev().cloned().collect()
    }

    /// One, by its id or its public id.
    pub fn get(&self, id: &str) -> Option<Incident> {
        self.read(id, Incident::clone)
    }

    /// What `f` makes of one (by its id or public id), read in place.
    pub fn read<T>(&self, id: &str, f: impl FnOnce(&Incident) -> T) -> Option<T> {
        let saved = lock(&self.saved);
        saved.key(id).and_then(|p| saved.incidents.get(&p)).map(f)
    }

    /// What `f` makes of every incident, the newest first, read in place (a page of them, what a
    /// search matched counted): none copied.
    pub fn read_all<T>(&self, f: impl FnOnce(&[&Incident]) -> T) -> T {
        let saved = lock(&self.saved);
        let all: Vec<&Incident> = saved.incidents.values().rev().collect();
        f(&all)
    }

    /// An incident declared: what `c` says (a title at the least), `by` whom, now.
    pub fn declare(&self, c: Change, by: &str, now: i64) -> Result<Incident, Refused> {
        let i = {
            let mut saved = lock(&self.saved);
            self.declare_in(&mut saved, c, by, now, |_| {})?
        };
        self.told(Told { incident: i.clone(), what: "declared" });
        Ok(i)
    }

    /// An incident declared within the lock, `more` done to it before it is kept; not told.
    fn declare_in(&self, saved: &mut Saved, c: Change, by: &str, now: i64, more: impl FnOnce(&mut Incident)) -> Result<Incident, Refused> {
        let title = bounded(c.title.as_deref().unwrap_or(""), TITLE, "title")?;
        if title.is_empty() {
            return Err(invalid("an incident has a title"));
        }
        let public_id = saved.next_public_id + 1;
        let mut i = Incident {
            id: uuid::Uuid::now_v7().hyphenated().to_string(),
            public_id,
            title,
            customer_impacted: false,
            customer_impact_scope: None,
            customer_impact_start: None,
            customer_impact_end: None,
            severity: "UNKNOWN".into(),
            state: "active".into(),
            fields: BTreeMap::new(),
            commander: None,
            notification_handles: Vec::new(),
            created: now,
            modified: now,
            detected: now,
            declared: now,
            resolved: None,
            created_by: by.to_owned(),
            last_modified_by: by.to_owned(),
            is_test: false,
            monitor: None,
            timeline: Vec::new(),
            todos: Vec::new(),
            attachments: Vec::new(),
            next: 0,
        };
        let initial = c.initial_cells.clone();
        apply(&mut i, Change { title: None, initial_cells: Vec::new(), ..c }, by, now, false)?;
        if i.state == "resolved" {
            i.resolved = Some(now);
        }
        i.add_cell(now, by, Cell::Changed { what: "declared".into(), from: String::new(), to: format!("{} · {}", i.severity, i.state) });
        for (content, important) in initial {
            let content = bounded(&content, NOTE, "a note")?;
            i.add_cell(now, by, Cell::Markdown { content, important });
        }
        more(&mut i);
        self.room(saved, 1, 0, None)?;
        // Its file its id's (the newest file's id, or `next.json`'s, the next one's start).
        let bytes = self.write(&i)?;
        saved.next_public_id = public_id;
        saved.put(i.clone(), bytes);
        self.room(saved, 0, 0, Some(public_id)).ok();
        Ok(i)
    }

    /// An incident changed as `c` says, `by` whom: each change of its severity, state, commander or
    /// title a cell of its timeline; a severity's or state's told.
    pub fn update(&self, id: &str, c: Change, by: &str, now: i64) -> Result<Incident, Refused> {
        let mut before = None;
        let changed = self.with(id, by, now, |i| {
            before = Some((i.severity.clone(), i.state.clone()));
            apply(i, c, by, now, true)?;
            Ok(i.clone())
        })?;
        if let Some((severity, state)) = before {
            if state != changed.state {
                self.told(Told { incident: changed.clone(), what: "state" });
            } else if severity != changed.severity {
                self.told(Told { incident: changed.clone(), what: "severity" });
            }
        }
        Ok(changed)
    }

    pub fn remove(&self, id: &str) -> Result<bool, Refused> {
        let mut saved = lock(&self.saved);
        let Some(p) = saved.key(id) else { return Ok(false) };
        self.forget(&mut saved, p)?;
        Ok(true)
    }

    /// Something done to one incident: done to a copy, the copy written, then kept (what is not
    /// written is not kept); what `f` returns.
    fn with<T>(&self, id: &str, by: &str, now: i64, f: impl FnOnce(&mut Incident) -> Result<T, Refused>) -> Result<T, Refused> {
        let mut saved = lock(&self.saved);
        let p = saved.key(id).ok_or(Refused::NoIncident)?;
        self.with_in(&mut saved, p, by, now, f)
    }

    /// [`Incidents::with`] within the lock, of incident `p`.
    fn with_in<T>(&self, saved: &mut Saved, p: u64, by: &str, now: i64, f: impl FnOnce(&mut Incident) -> Result<T, Refused>) -> Result<T, Refused> {
        let mut i = saved.incidents.get(&p).cloned().ok_or(Refused::NoIncident)?;
        i.modified = now;
        i.last_modified_by = by.to_owned();
        let out = f(&mut i)?;
        let bytes = self.write(&i)?;
        saved.put(i, bytes);
        // Past its bounds, the oldest resolved let go (never this one).
        self.room(saved, 0, 0, Some(p)).ok();
        Ok(out)
    }

    /// A note on its timeline.
    pub fn note(&self, id: &str, content: &str, important: bool, by: &str, now: i64) -> Result<TimelineCell, Refused> {
        let content = bounded(content, NOTE, "a note")?;
        if content.is_empty() {
            return Err(invalid("a note says something"));
        }
        self.with(id, by, now, |i| {
            let cid = i.add_cell(now, by, Cell::Markdown { content, important });
            i.timeline.iter().find(|c| c.id == cid).cloned().ok_or(Refused::NoItem("cell"))
        })
    }

    /// A todo made.
    pub fn add_todo(&self, id: &str, content: &str, assignees: Vec<String>, due_date: Option<String>, by: &str, now: i64) -> Result<Todo, Refused> {
        let content = bounded(content, TEXT, "a todo's content")?;
        if content.is_empty() {
            return Err(invalid("a todo says what is to do"));
        }
        let assignees = assignees.iter().take(20).map(|a| bounded(a, 256, "an assignee")).collect::<Result<Vec<_>, _>>()?;
        let due_date = due_date.map(|d| bounded(&d, 64, "a due date")).transpose()?;
        self.with(id, by, now, |i| {
            if i.todos.len() >= TODOS {
                return Err(Refused::Full(format!("{TODOS} todos an incident at the most")));
            }
            let t = Todo { id: i.next_id("t"), content, assignees, due_date, completed: None, created: now, modified: now };
            i.todos.push(t.clone());
            Ok(t)
        })
    }

    /// A todo changed: its content, assignees, due date, or done (or not).
    #[allow(clippy::too_many_arguments)]
    pub fn change_todo(
        &self,
        id: &str,
        todo: &str,
        content: Option<String>,
        assignees: Option<Vec<String>>,
        due_date: Option<Option<String>>,
        completed: Option<bool>,
        by: &str,
        now: i64,
    ) -> Result<Todo, Refused> {
        let content = content.map(|c| bounded(&c, TEXT, "a todo's content")).transpose()?;
        let assignees = assignees.map(|a| a.iter().take(20).map(|a| bounded(a, 256, "an assignee")).collect::<Result<Vec<_>, _>>()).transpose()?;
        let due_date = due_date.map(|d| d.map(|d| bounded(&d, 64, "a due date")).transpose()).transpose()?;
        self.with(id, by, now, |i| {
            let t = i.todos.iter_mut().find(|t| t.id == todo).ok_or(Refused::NoItem("todo"))?;
            if let Some(c) = content.filter(|c| !c.is_empty()) {
                t.content = c;
            }
            if let Some(a) = assignees {
                t.assignees = a;
            }
            if let Some(d) = due_date {
                t.due_date = d;
            }
            if let Some(done) = completed {
                t.completed = done.then(|| t.completed.unwrap_or(now));
            }
            t.modified = now;
            Ok(t.clone())
        })
    }

    pub fn remove_todo(&self, id: &str, todo: &str, by: &str, now: i64) -> Result<(), Refused> {
        self.with(id, by, now, |i| {
            let n = i.todos.len();
            i.todos.retain(|t| t.id != todo);
            if i.todos.len() == n { Err(Refused::NoItem("todo")) } else { Ok(()) }
        })
    }

    /// An attachment made: a postmortem (one an incident) or a link, a URL of HTTP(S).
    pub fn attach(&self, id: &str, kind: &str, url: &str, title: &str, by: &str, now: i64) -> Result<Attachment, Refused> {
        if !matches!(kind, "postmortem" | "link") {
            return Err(invalid("an attachment is a postmortem or a link"));
        }
        let url = bounded(url, 2048, "an attachment's URL")?;
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return Err(invalid("an attachment's URL is HTTP(S)"));
        }
        let title = bounded(title, TITLE, "an attachment's title")?;
        self.with(id, by, now, |i| {
            if kind == "postmortem" && i.has_postmortem() {
                return Err(invalid("an incident has one postmortem: change it"));
            }
            if i.attachments.len() >= ATTACHMENTS {
                return Err(Refused::Full(format!("{ATTACHMENTS} attachments an incident at the most")));
            }
            let a = Attachment { id: i.next_id("a"), attachment_type: kind.to_owned(), url, title, modified: now };
            i.attachments.push(a.clone());
            if kind == "postmortem" {
                i.add_cell(now, by, Cell::Changed { what: "postmortem".into(), from: String::new(), to: a.title.clone() });
            }
            Ok(a)
        })
    }

    pub fn change_attachment(&self, id: &str, attachment: &str, url: Option<String>, title: Option<String>, by: &str, now: i64) -> Result<Attachment, Refused> {
        let url = url.map(|u| bounded(&u, 2048, "an attachment's URL")).transpose()?;
        if url.as_deref().is_some_and(|u| !(u.starts_with("https://") || u.starts_with("http://"))) {
            return Err(invalid("an attachment's URL is HTTP(S)"));
        }
        let title = title.map(|t| bounded(&t, TITLE, "an attachment's title")).transpose()?;
        self.with(id, by, now, |i| {
            let a = i.attachments.iter_mut().find(|a| a.id == attachment).ok_or(Refused::NoItem("attachment"))?;
            if let Some(u) = url {
                a.url = u;
            }
            if let Some(t) = title {
                a.title = t;
            }
            a.modified = now;
            Ok(a.clone())
        })
    }

    pub fn remove_attachment(&self, id: &str, attachment: &str, by: &str, now: i64) -> Result<(), Refused> {
        self.with(id, by, now, |i| {
            let n = i.attachments.len();
            i.attachments.retain(|a| a.id != attachment);
            if i.attachments.len() == n { Err(Refused::NoItem("attachment")) } else { Ok(()) }
        })
    }

    /// A monitor's group changed state, its message naming `@incident` (or `@incident-<name>`;
    /// `@incident-sev2` its severity): alerting with no incident of it open, one declared, its
    /// commander none, its handles the message's other mentions; one open, its timeline told.
    #[allow(clippy::too_many_arguments)]
    pub fn from_monitor(&self, m: &FromMonitor, mention: &str, state: &str, value: Option<f64>, message: &str, mentions: &[String], now: i64) -> Option<Incident> {
        let cell = Cell::Monitor { monitor_id: m.id.clone(), monitor: m.name.clone(), state: state.to_owned(), value, message: message.chars().take(TEXT).collect() };
        let by = format!("monitor:{}", m.id);
        let mut saved = lock(&self.saved);
        // The newest open of its group: told.
        let open = saved.incidents.values().rev().find(|i| i.open() && i.monitor.as_ref().is_some_and(|f| f.id == m.id && f.group == m.group)).map(|i| i.public_id);
        if let Some(p) = open {
            let told = self.with_in(&mut saved, p, &by, now, |i| {
                i.add_cell(now, &by, cell);
                Ok(i.clone())
            });
            drop(saved);
            return told.map_err(|e| tracing::warn!(monitor = %m.id, incident = p, error = %e, "the monitor's change was not put on its incident")).ok();
        }
        if state != "ALERT" {
            return None;
        }
        let severity =
            mention.strip_prefix("incident-").and_then(|n| n.strip_prefix("sev")).and_then(|d| d.parse::<u8>().ok()).filter(|d| *d <= 5).map_or_else(|| "UNKNOWN".to_owned(), |d| format!("SEV-{d}"));
        let group = m.group.iter().map(|(k, v)| format!("{k}:{v}")).collect::<Vec<_>>().join(",");
        let title = if group.is_empty() { m.name.clone() } else { format!("{} on {group}", m.name) };
        let handles = mentions.iter().filter(|x| !x.starts_with("incident")).map(|x| Handle { display_name: x.clone(), handle: format!("@{x}") }).collect();
        let c = Change { title: Some(title.chars().take(TITLE).collect()), severity: Some(severity), notification_handles: Some(handles), ..Change::default() };
        // Declared with its monitor, how it was found and the alert in one write, under the lock
        // that found none open: one incident a group's alert, however its monitor is evaluated.
        let declared = self.declare_in(&mut saved, c, &by, now, |i| {
            i.monitor = Some(m.clone());
            i.fields.insert("detection_method".into(), Field { kind: "dropdown".into(), value: Value::String("monitor".into()) });
            i.add_cell(now, &by, cell);
        });
        drop(saved);
        match declared {
            Ok(i) => {
                self.told(Told { incident: i.clone(), what: "declared" });
                Some(i)
            }
            Err(e) => {
                tracing::warn!(monitor = %m.id, error = %e, "the monitor's incident was not declared");
                None
            }
        }
    }
}

/// `c` applied to `i` (a change of one kept, or a new one's attributes), each change of what a
/// timeline tells a cell of it when `cells`.
fn apply(i: &mut Incident, c: Change, by: &str, now: i64, cells: bool) -> Result<(), Refused> {
    let mut changed: Vec<Cell> = Vec::new();
    let mut note = |what: &str, from: &str, to: &str| {
        if from != to {
            changed.push(Cell::Changed { what: what.to_owned(), from: from.to_owned(), to: to.to_owned() });
        }
    };
    if let Some(t) = c.title {
        let t = bounded(&t, TITLE, "title")?;
        if t.is_empty() {
            return Err(invalid("an incident has a title"));
        }
        note("title", &i.title, &t);
        i.title = t;
    }
    // Severity and state: as attributes, or as Datadog's fields of them.
    let mut fields = c.fields;
    let from_field = |fields: &mut BTreeMap<String, Field>, name: &str| fields.remove(name).and_then(|f| f.value.as_str().map(str::to_owned));
    let severity = c.severity.or_else(|| from_field(&mut fields, "severity"));
    let state = c.state.or_else(|| from_field(&mut fields, "state"));
    if let Some(s) = severity {
        let s = s.trim().to_ascii_uppercase();
        if !SEVERITIES.contains(&s.as_str()) {
            return Err(invalid(format!("severity: {}", SEVERITIES.join(", "))));
        }
        note("severity", &i.severity, &s);
        i.severity = s;
    }
    if let Some(s) = state {
        let s = s.trim().to_ascii_lowercase();
        if !STATES.contains(&s.as_str()) {
            return Err(invalid(format!("state: {}", STATES.join(", "))));
        }
        note("state", &i.state, &s);
        if s == "resolved" && i.state != "resolved" {
            i.resolved = Some(now);
        } else if s != "resolved" {
            i.resolved = None;
        }
        i.state = s;
    }
    if let Some(v) = c.customer_impacted {
        i.customer_impacted = v;
    }
    if let Some(v) = c.customer_impact_scope {
        i.customer_impact_scope = v.map(|s| bounded(&s, TEXT, "customer_impact_scope")).transpose()?.filter(|s| !s.is_empty());
    }
    if let Some(v) = c.customer_impact_start {
        i.customer_impact_start = v;
    }
    if let Some(v) = c.customer_impact_end {
        i.customer_impact_end = v;
    }
    if i.customer_impacted && i.customer_impact_scope.is_none() {
        return Err(invalid("customer_impact_scope says how customers were impacted"));
    }
    if let (Some(s), Some(e)) = (i.customer_impact_start, i.customer_impact_end)
        && e < s
    {
        return Err(invalid("customer_impact_end comes after customer_impact_start"));
    }
    if let Some(d) = c.detected {
        i.detected = d;
    }
    for (name, f) in fields {
        if name.is_empty() || name.len() > 128 {
            return Err(invalid("a field's name: 1 to 128 bytes"));
        }
        f.check().map_err(invalid)?;
        if f.value.is_null() {
            i.fields.remove(&name);
        } else {
            if !i.fields.contains_key(&name) && i.fields.len() >= FIELDS {
                return Err(invalid(format!("{FIELDS} fields an incident at the most")));
            }
            i.fields.insert(name, f);
        }
    }
    if let Some(cmd) = c.commander {
        let cmd = cmd.map(|s| bounded(&s, 256, "commander")).transpose()?.filter(|s| !s.is_empty());
        note("commander", i.commander.as_deref().unwrap_or(""), cmd.as_deref().unwrap_or(""));
        i.commander = cmd;
    }
    if let Some(hs) = c.notification_handles {
        i.notification_handles = check_handles(&hs)?;
    }
    if let Some(t) = c.is_test {
        i.is_test = t;
    }
    if cells {
        for cell in changed {
            i.add_cell(now, by, cell);
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    fn kept() -> (tempfile::TempDir, Arc<Incidents>) {
        let dir = tempfile::tempdir().unwrap();
        let i = Incidents::open(dir.path());
        (dir, i)
    }

    fn titled(t: &str) -> Change {
        Change { title: Some(t.into()), ..Change::default() }
    }

    /// Declared, changed and resolved: each change of its severity, state and commander a cell of
    /// its timeline; its figures Datadog's; its public ids one after another; what is refused,
    /// saying why; kept through the file.
    #[test]
    fn an_incident_is_declared_worked_and_resolved() {
        let (dir, kept) = kept();
        let t0 = 1_792_324_800_000;
        let c = Change {
            title: Some("Checkout is down".into()),
            severity: Some("sev-2".into()),
            customer_impacted: Some(true),
            customer_impact_scope: Some(Some("Every checkout fails".into())),
            customer_impact_start: Some(Some(t0)),
            detected: Some(t0 + 60_000),
            initial_cells: vec![("Paged the payments team.".into(), true)],
            ..Change::default()
        };
        let i = kept.declare(c, "user:ada", t0 + 120_000).unwrap();
        assert_eq!((i.public_id, i.severity.as_str(), i.state.as_str(), i.created_by.as_str()), (1, "SEV-2", "active", "user:ada"));
        assert_eq!(i.timeline.len(), 2, "its declaring and its first note");
        let two = kept.declare(titled("Another"), "user:bob", t0 + 130_000).unwrap();
        assert_eq!(two.public_id, 2);
        // Worked: its commander, stable, then resolved with its impact's end.
        let c = Change { commander: Some(Some("user:ada".into())), state: Some("stable".into()), ..Change::default() };
        kept.update(&i.id, c, "user:ada", t0 + 600_000).unwrap();
        let c = Change {
            state: Some("resolved".into()),
            customer_impact_end: Some(Some(t0 + 900_000)),
            fields: [("root_cause".to_owned(), Field { kind: "textbox".into(), value: Value::String("A bad deploy".into()) })].into(),
            ..Change::default()
        };
        let i = kept.update("1", c, "user:ada", t0 + 1_200_000).unwrap();
        assert_eq!((i.state.as_str(), i.resolved, i.commander.as_deref()), ("resolved", Some(t0 + 1_200_000), Some("user:ada")));
        let changes: Vec<(String, String, String)> = i
            .timeline
            .iter()
            .filter_map(|c| match &c.cell {
                Cell::Changed { what, from, to } if what != "declared" => Some((what.clone(), from.clone(), to.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(changes, vec![("state".into(), "active".into(), "stable".into()), ("commander".into(), String::new(), "user:ada".into()), ("state".into(), "stable".into(), "resolved".into())]);
        let d = i.durations();
        assert_eq!((d.time_to_detect, d.time_to_internal_response, d.time_to_repair, d.time_to_resolve, d.customer_impact_duration), (Some(60), Some(60), Some(840), Some(1_080), Some(900)));
        // An impact said to start after its detection: detected at once, never below zero.
        let early = Incident { customer_impact_start: Some(i.detected + 30_000), ..i.clone() };
        assert_eq!(early.durations().time_to_detect, Some(0));
        // Reopened: no longer resolved.
        let i = kept.update(&i.id, Change { state: Some("active".into()), ..Change::default() }, "user:ada", t0 + 1_300_000).unwrap();
        assert_eq!(i.resolved, None);
        // Refused, saying why.
        for (c, says) in [
            (titled(""), "title"),
            (Change { severity: Some("SEV-9".into()), ..titled("x") }, "severity"),
            (Change { state: Some("closed".into()), ..titled("x") }, "state"),
            (Change { customer_impacted: Some(true), ..titled("x") }, "customer_impact_scope"),
            (Change { fields: [("services".to_owned(), Field { kind: "multiselect".into(), value: Value::String("one".into()) })].into(), ..titled("x") }, "multiselect"),
        ] {
            let e = kept.declare(c, "u", t0).unwrap_err().to_string();
            assert!(e.contains(says), "{e}");
        }
        assert_eq!(kept.update("nonesuch", Change::default(), "u", t0).unwrap_err(), Refused::NoIncident);
        // Kept through its file.
        let again = Incidents::open(dir.path());
        assert_eq!(again.get(&i.id), Some(i.clone()));
        assert_eq!(again.list().iter().map(|i| i.public_id).collect::<Vec<_>>(), vec![2, 1]);
        assert!(again.remove("2").unwrap() && !again.remove("2").unwrap());
    }

    /// Its notes, todos and attachments: made, changed, done and removed; one postmortem an
    /// incident; a URL of HTTP(S) alone.
    #[test]
    fn notes_todos_and_attachments_are_kept() {
        let (_dir, kept) = kept();
        let i = kept.declare(titled("Slow search"), "u", 1).unwrap();
        let n = kept.note(&i.id, "Rolled back **search-api**.", true, "u", 2).unwrap();
        assert!(matches!(n.cell, Cell::Markdown { important: true, .. }));
        let t = kept.add_todo(&i.id, "Write the postmortem", vec!["ada".into()], Some("2026-10-30".into()), "u", 3).unwrap();
        let done = kept.change_todo(&i.id, &t.id, None, None, None, Some(true), "u", 4).unwrap();
        assert_eq!(done.completed, Some(4));
        let undone = kept.change_todo(&i.id, &t.id, Some("Write it".into()), None, Some(None), Some(false), "u", 5).unwrap();
        assert_eq!((undone.completed, undone.due_date, undone.content.as_str()), (None, None, "Write it"));
        let pm = kept.attach(&i.id, "postmortem", "https://docs.example.com/pm/1", "Postmortem", "u", 6).unwrap();
        assert!(kept.attach(&i.id, "postmortem", "https://docs.example.com/pm/2", "Another", "u", 7).unwrap_err().to_string().contains("one postmortem"));
        assert!(kept.attach(&i.id, "link", "javascript:alert(1)", "x", "u", 7).is_err());
        let link = kept.attach(&i.id, "link", "https://grafana.example.com/d/1", "Dashboard", "u", 8).unwrap();
        let changed = kept.change_attachment(&i.id, &link.id, None, Some("The dashboard".into()), "u", 9).unwrap();
        assert_eq!(changed.title, "The dashboard");
        kept.remove_attachment(&i.id, &pm.id, "u", 10).unwrap();
        kept.remove_todo(&i.id, &t.id, "u", 11).unwrap();
        assert_eq!(kept.remove_todo(&i.id, &t.id, "u", 12).unwrap_err(), Refused::NoItem("todo"));
        let i = kept.get(&i.id).unwrap();
        assert_eq!((i.todos.len(), i.attachments.len(), i.has_postmortem()), (0, 1, false));
        assert!(i.timeline.iter().any(|c| matches!(&c.cell, Cell::Changed { what, .. } if what == "postmortem")));
    }

    /// A monitor's group alerting declares one (its severity its mention's, its handles the
    /// message's others); alerting again and recovering are its timeline's; alerting after it is
    /// resolved declares another; a recovery with none open declares none.
    #[test]
    fn a_monitor_declares_and_tells_its_incident() {
        let (_dir, kept) = kept();
        let m = FromMonitor { id: "m1".into(), name: "Checkout errors".into(), group: vec![("service".into(), "checkout".into())] };
        let mentions = vec!["incident-sev1".to_owned(), "slack-ops".to_owned()];
        assert!(kept.from_monitor(&m, "incident-sev1", "OK", None, "fine", &mentions, 1).is_none(), "a recovery declares nothing");
        let i = kept.from_monitor(&m, "incident-sev1", "ALERT", Some(42.0), "errors at 42", &mentions, 2).unwrap();
        assert_eq!((i.title.as_str(), i.severity.as_str(), i.created_by.as_str()), ("Checkout errors on service:checkout", "SEV-1", "monitor:m1"));
        assert_eq!(i.notification_handles, vec![Handle { display_name: "slack-ops".into(), handle: "@slack-ops".into() }]);
        assert_eq!(i.fields.get("detection_method").map(|f| f.values()), Some(vec!["monitor"]));
        let again = kept.from_monitor(&m, "incident-sev1", "ALERT", Some(50.0), "again", &mentions, 3).unwrap();
        assert_eq!(again.id, i.id, "the same one, told");
        let ok = kept.from_monitor(&m, "incident-sev1", "OK", None, "recovered", &mentions, 4).unwrap();
        let monitors: Vec<&str> = ok.timeline.iter().filter_map(|c| if let Cell::Monitor { state, .. } = &c.cell { Some(state.as_str()) } else { None }).collect();
        assert_eq!(monitors, vec!["ALERT", "ALERT", "OK"]);
        kept.update(&i.id, Change { state: Some("resolved".into()), ..Change::default() }, "u", 5).unwrap();
        let next = kept.from_monitor(&m, "incident", "ALERT", None, "again", &mentions, 6).unwrap();
        assert!(next.id != i.id && next.severity == "UNKNOWN");
        // Another group: another incident.
        let other = FromMonitor { group: vec![("service".into(), "cart".into())], ..m };
        assert!(kept.from_monitor(&other, "incident", "ALERT", None, "x", &mentions, 7).is_some_and(|o| o.id != next.id));
    }

    /// Kept a file each: past its bounds (a number, bytes) the oldest resolved let go, never an
    /// open one (none to let go, none declared); a removed one's id never given again, through a
    /// restart; a file that does not read left as it is; what a write cut short left, removed.
    #[test]
    fn incidents_are_kept_a_file_each_within_their_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let kept = Incidents::open_bounded(dir.path(), 3, BYTES);
        let files = || {
            let mut f: Vec<String> = std::fs::read_dir(dir.path().join("incidents")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
            f.sort();
            f
        };
        for t in ["one", "two", "three"] {
            kept.declare(titled(t), "u", 1).unwrap();
        }
        assert_eq!(files(), ["1.json", "2.json", "3.json"]);
        assert!(matches!(kept.declare(titled("four"), "u", 2), Err(Refused::Full(_))), "every one open: none let go");
        kept.update("2", Change { state: Some("resolved".into()), ..Change::default() }, "u", 3).unwrap();
        kept.update("1", Change { state: Some("resolved".into()), ..Change::default() }, "u", 3).unwrap();
        let four = kept.declare(titled("four"), "u", 4).unwrap();
        assert_eq!((four.public_id, kept.get("1"), kept.get("2").is_some()), (4, None, true), "the oldest resolved let go");
        assert_eq!(files(), ["2.json", "3.json", "4.json", "next.json"]);
        // The newest removed: its id never given again, through a restart.
        assert!(kept.remove("4").unwrap());
        drop(kept);
        std::fs::write(dir.path().join("incidents/9.json"), b"{ not an incident").unwrap();
        std::fs::write(dir.path().join("incidents/3.tmp-cut"), b"half").unwrap();
        let again = Incidents::open_bounded(dir.path(), 3, BYTES);
        assert_eq!(again.list().iter().map(|i| i.public_id).collect::<Vec<_>>(), [3, 2]);
        assert_eq!(again.declare(titled("five"), "u", 5).unwrap().public_id, 10, "past the ids of every file, read or not");
        assert!(dir.path().join("incidents/9.json").exists() && !dir.path().join("incidents/3.tmp-cut").exists());
        // By its bytes: the oldest resolved let go as a change outgrows them; an open one kept.
        let dir = tempfile::tempdir().unwrap();
        let kept = Incidents::open_bounded(dir.path(), 100, 6_000);
        let a = kept.declare(titled("a"), "u", 1).unwrap();
        kept.update(&a.id, Change { state: Some("resolved".into()), ..Change::default() }, "u", 2).unwrap();
        let b = kept.declare(titled("b"), "u", 3).unwrap();
        for _ in 0..20 {
            kept.note(&b.id, &"x".repeat(400), false, "u", 4).unwrap();
        }
        assert!(kept.get(&a.id).is_none() && kept.get(&b.id).is_some(), "the resolved let go, the open kept");
        let weight = lock(&kept.saved).bytes;
        assert_eq!(weight, std::fs::metadata(dir.path().join(format!("incidents/{}.json", b.public_id))).unwrap().len() as usize, "its bytes counted as written");
    }

    /// A monitor's incident written whole at once: its monitor, how it was found, its alert.
    #[test]
    fn a_monitor_s_incident_is_written_whole_at_once() {
        let (dir, kept) = kept();
        let m = FromMonitor { id: "m9".into(), name: "Disk full".into(), group: vec![("host".into(), "db-1".into())] };
        let i = kept.from_monitor(&m, "incident", "ALERT", Some(97.0), "full", &["incident".into()], 1).unwrap();
        let on_disk: Incident = serde_json::from_slice(&std::fs::read(dir.path().join(format!("incidents/{}.json", i.public_id))).unwrap()).unwrap();
        assert_eq!(on_disk, i);
        assert!(on_disk.monitor.is_some() && on_disk.fields.contains_key("detection_method") && on_disk.timeline.iter().any(|c| matches!(c.cell, Cell::Monitor { .. })));
    }

    /// A timeline past its bound keeps its first cells and lets the oldest after them go.
    #[test]
    fn a_timeline_is_bounded() {
        let (_dir, kept) = kept();
        let i = kept.declare(titled("Noisy"), "u", 1).unwrap();
        for n in 0..CELLS + 50 {
            kept.note(&i.id, &format!("note {n}"), false, "u", 2).unwrap();
        }
        let i = kept.get(&i.id).unwrap();
        assert_eq!(i.timeline.len(), CELLS);
        assert!(matches!(&i.timeline[0].cell, Cell::Changed { what, .. } if what == "declared"), "its declaring kept");
        assert!(matches!(&i.timeline[CELLS - 1].cell, Cell::Markdown { content, .. } if content == &format!("note {}", CELLS + 49)));
    }
}
