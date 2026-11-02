//! Notebooks (`docs/observe.md` §11.1), Datadog's: a document of cells, Markdown and graphs
//! (Datadog's timeseries, top list, heatmap, distribution and log stream definitions, as its
//! dashboards' widgets have them), over a time (the last hour, or from one time to another), each
//! cell its own time and size if it says; its type (a postmortem, a runbook, an investigation,
//! documentation, a report) and whether it is a template. A postmortem is one, made of a template and
//! an incident (`postmortem`).
//!
//! The server keeps them, each a file of its own (`<observe>/notebooks/<id>.json`, written whole as
//! it changes), and in memory only what lists them (its name, author, type, times, size): a
//! notebook's cells are read from its file when it is read. Bounded: [`NOTEBOOKS`] of them,
//! [`BYTES`] of their files, each [`CELLS`] cells and [`NOTEBOOK`] bytes at the most.

pub mod http;
pub mod postmortem;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Notebooks kept at the most, their files' bytes, a notebook's cells and bytes.
pub const NOTEBOOKS: usize = 10_000;
pub const BYTES: usize = 1 << 30;
pub const CELLS: usize = 500;
pub const NOTEBOOK: usize = 4 << 20;
/// A name's characters at the most (Datadog's).
pub const NAME: usize = 80;
/// Template variables a notebook's at the most.
pub const VARIABLES: usize = 64;

/// A notebook's types, Datadog's.
pub const TYPES: [&str; 5] = ["postmortem", "runbook", "investigation", "documentation", "report"];
/// A cell's kinds, Datadog's.
pub const CELL_TYPES: [&str; 6] = ["markdown", "timeseries", "toplist", "heatmap", "distribution", "log_stream"];
/// A graph's sizes, Datadog's.
pub const GRAPH_SIZES: [&str; 5] = ["xs", "s", "m", "l", "xl"];
/// The spans a time may be the last of (Datadog's `WidgetLiveSpan`), each in ms (none: since a
/// week's or a month's start, or the alert's).
pub const LIVE_SPANS: [(&str, Option<i64>); 17] = [
    ("1m", Some(60_000)),
    ("5m", Some(300_000)),
    ("10m", Some(600_000)),
    ("15m", Some(900_000)),
    ("30m", Some(1_800_000)),
    ("1h", Some(3_600_000)),
    ("4h", Some(14_400_000)),
    ("1d", Some(86_400_000)),
    ("2d", Some(172_800_000)),
    ("1w", Some(604_800_000)),
    ("1mo", Some(2_592_000_000)),
    ("3mo", Some(7_776_000_000)),
    ("6mo", Some(15_552_000_000)),
    ("week_to_date", None),
    ("month_to_date", None),
    ("1y", Some(31_536_000_000)),
    ("alert", None),
];

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A cell: its id and Datadog's attributes as given (its `definition`, `graph_size`, `split_by`,
/// `time`), checked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cell {
    pub id: String,
    pub attributes: Value,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Metadata {
    #[serde(default)]
    pub is_template: bool,
    #[serde(default)]
    pub take_snapshots: bool,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
}

/// A notebook. Times are Unix ms.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notebook {
    pub id: u64,
    pub name: String,
    pub cells: Vec<Cell>,
    /// Datadog's global time: `{"live_span": "1h"}`, or `{"start": …, "end": …, "live": …}`.
    pub time: Value,
    #[serde(default)]
    pub metadata: Metadata,
    #[serde(default)]
    pub template_variables: Option<Vec<Value>>,
    pub author: String,
    pub created: i64,
    pub modified: i64,
    /// The incident it is the postmortem of (kiln's).
    #[serde(default)]
    pub incident: Option<String>,
    /// The next number its cells' ids take.
    #[serde(default)]
    pub next: u64,
}

/// What a notebook's list says of it, kept in memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub id: u64,
    pub name: String,
    pub author: String,
    pub created: i64,
    pub modified: i64,
    pub metadata: Metadata,
    pub incident: Option<String>,
    pub bytes: usize,
}

impl Summary {
    fn of(n: &Notebook, bytes: usize) -> Self {
        Self { id: n.id, name: n.name.clone(), author: n.author.clone(), created: n.created, modified: n.modified, metadata: n.metadata.clone(), incident: n.incident.clone(), bytes }
    }
}

/// What a making or a change says: Datadog's attributes, checked.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Draft {
    pub name: String,
    /// Each cell's attributes, and its id when it is one kept (a change's).
    pub cells: Vec<(Option<String>, Value)>,
    pub time: Value,
    pub metadata: Metadata,
    pub template_variables: Option<Vec<Value>>,
}

/// Why something was refused: what is wrong (400), what is not there (404), no room (409).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refused {
    #[error("{0}")]
    Invalid(String),
    #[error("no such notebook")]
    NoNotebook,
    #[error("{0}")]
    Full(String),
}

fn invalid(s: impl Into<String>) -> Refused {
    Refused::Invalid(s.into())
}

/// A time, Datadog's: the last of a span (`live_span`), or from `start` to `end` (RFC 3339, `start`
/// before `end`), `live` or not.
pub fn check_time(t: &Value, what: &str) -> Result<(), Refused> {
    let o = t.as_object().ok_or_else(|| invalid(format!("{what}: {{\"live_span\": \"1h\"}}, or {{\"start\": …, \"end\": …}}")))?;
    if let Some(span) = o.get("live_span") {
        let s = span.as_str().unwrap_or("");
        if !LIVE_SPANS.iter().any(|(n, _)| *n == s) {
            return Err(invalid(format!("{what}.live_span: {}", LIVE_SPANS.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", "))));
        }
        return Ok(());
    }
    let at = |k: &str| -> Result<i64, Refused> {
        let s = o.get(k).and_then(Value::as_str).ok_or_else(|| invalid(format!("{what}.{k}: a time, RFC 3339")))?;
        time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
            .map(|t| i64::try_from(t.unix_timestamp_nanos() / 1_000_000).unwrap_or(i64::MAX))
            .map_err(|_| invalid(format!("{what}.{k}: RFC 3339 (2026-10-22T12:00:00Z)")))
    };
    if at("start")? >= at("end")? {
        return Err(invalid(format!("{what}: its start before its end")));
    }
    if o.get("live").is_some_and(|l| !l.is_boolean()) {
        return Err(invalid(format!("{what}.live: true or false")));
    }
    Ok(())
}

/// A cell's attributes as Datadog's API takes them: a definition of a kind it has (Markdown its
/// `text`; a graph its `requests`, a heatmap's and a distribution's one; a log stream its
/// `query`), a size and a time of Datadog's if it says, within [`NOTEBOOK`].
pub fn check_cell(a: &Value, at: usize) -> Result<(), Refused> {
    let what = format!("cells[{at}]");
    let o = a.as_object().ok_or_else(|| invalid(format!("{what}.attributes: an object")))?;
    let def = o.get("definition").and_then(Value::as_object).ok_or_else(|| invalid(format!("{what}.attributes.definition: an object")))?;
    let kind = def.get("type").and_then(Value::as_str).unwrap_or("");
    if !CELL_TYPES.contains(&kind) {
        return Err(invalid(format!("{what}: a cell of type {}", CELL_TYPES.join(", "))));
    }
    let requests = def.get("requests").and_then(Value::as_array).map(Vec::len);
    match kind {
        "markdown" if !def.get("text").is_some_and(Value::is_string) => return Err(invalid(format!("{what}: a Markdown cell's text"))),
        "timeseries" | "toplist" if requests.is_none_or(|n| n == 0) => return Err(invalid(format!("{what}: a {kind} cell's requests"))),
        "heatmap" | "distribution" if requests != Some(1) => return Err(invalid(format!("{what}: a {kind} cell's one request"))),
        "log_stream" if def.get("query").is_some_and(|q| !q.is_string()) => return Err(invalid(format!("{what}: a log stream's query, a search"))),
        _ => {}
    }
    if let Some(size) = o.get("graph_size")
        && !size.as_str().is_some_and(|s| GRAPH_SIZES.contains(&s))
    {
        return Err(invalid(format!("{what}.graph_size: {}", GRAPH_SIZES.join(", "))));
    }
    if let Some(t) = o.get("time")
        && !t.is_null()
    {
        check_time(t, &format!("{what}.time"))?;
    }
    if let Some(s) = o.get("split_by") {
        let ok =
            s.get("keys").is_some_and(|k| k.as_array().is_some_and(|k| k.iter().all(Value::is_string))) && s.get("tags").is_some_and(|k| k.as_array().is_some_and(|k| k.iter().all(Value::is_string)));
        if !ok {
            return Err(invalid(format!("{what}.split_by: {{\"keys\": […], \"tags\": […]}}")));
        }
    }
    Ok(())
}

/// A making's or a change's attributes checked: a name of [`NAME`] characters at the most, its
/// cells, its time, its type of Datadog's, its template variables.
pub fn check(d: &Draft) -> Result<(), Refused> {
    if d.name.chars().count() > NAME {
        return Err(invalid(format!("name: {NAME} characters at the most")));
    }
    if d.cells.len() > CELLS {
        return Err(invalid(format!("cells: {CELLS} at the most")));
    }
    for (at, (_, a)) in d.cells.iter().enumerate() {
        check_cell(a, at)?;
    }
    check_time(&d.time, "time")?;
    if let Some(k) = &d.metadata.kind
        && !TYPES.contains(&k.as_str())
    {
        return Err(invalid(format!("metadata.type: {}", TYPES.join(", "))));
    }
    if let Some(vs) = &d.template_variables {
        if vs.len() > VARIABLES {
            return Err(invalid(format!("template_variables: {VARIABLES} at the most")));
        }
        if vs.iter().any(|v| !v.get("name").is_some_and(|n| n.as_str().is_some_and(|n| !n.is_empty()))) {
            return Err(invalid("template_variables: each its name"));
        }
    }
    Ok(())
}

/// What a notebook's file says of it besides its cells and time (serde skips those).
#[derive(Debug, Deserialize)]
struct Head {
    id: u64,
    name: String,
    author: String,
    created: i64,
    modified: i64,
    #[serde(default)]
    metadata: Metadata,
    #[serde(default)]
    incident: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Next {
    next_id: u64,
}

#[derive(Debug, Default)]
struct Index {
    notebooks: BTreeMap<u64, Summary>,
    bytes: usize,
    next_id: u64,
}

/// The server's notebooks.
pub struct Notebooks {
    dir: PathBuf,
    index: Mutex<Index>,
    /// Its bounds: [`NOTEBOOKS`] and [`BYTES`] (less in tests).
    bounds: (usize, usize),
    /// Where the server's pages are (its public URL): a postmortem's link to its notebook.
    pub link_base: String,
}

static NOTEBOOKS_KEPT: OnceLock<Arc<Notebooks>> = OnceLock::new();

pub fn get() -> Option<&'static Arc<Notebooks>> {
    NOTEBOOKS_KEPT.get()
}

pub fn set(n: Arc<Notebooks>) {
    let _ = NOTEBOOKS_KEPT.set(n);
}

impl Notebooks {
    /// The notebooks kept in `<dir>/notebooks`: each file read once for its summary (one that does
    /// not read said, left as it is, its id never given again); what a write cut short left, removed.
    pub fn open(dir: &Path, link_base: &str) -> Arc<Self> {
        Self::open_bounded(dir, NOTEBOOKS, BYTES, link_base)
    }

    fn open_bounded(dir: &Path, most: usize, weight: usize, link_base: &str) -> Arc<Self> {
        let dir = dir.join("notebooks");
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::warn!(dir = %dir.display(), error = %e, "the notebooks' folder was not made");
        }
        let mut index = Index::default();
        if let Ok(b) = std::fs::read(dir.join("next.json"))
            && let Ok(n) = serde_json::from_slice::<Next>(&b)
        {
            index.next_id = n.next_id;
        }
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.extension().and_then(|x| x.to_str()).is_some_and(|x| x.starts_with("tmp-")) {
                let _ = std::fs::remove_file(&path);
                continue;
            }
            let Some(id) = path.file_stem().and_then(|s| s.to_str()).and_then(|s| s.parse::<u64>().ok()).filter(|_| path.extension().is_some_and(|x| x == "json")) else { continue };
            index.next_id = index.next_id.max(id);
            // Its summary alone read: its cells skipped, not made.
            match std::fs::read(&path).map_err(|e| e.to_string()).and_then(|b| serde_json::from_slice::<Head>(&b).map(|h| (h, b.len())).map_err(|e| e.to_string())) {
                Ok((h, bytes)) if h.id == id => {
                    index.bytes += bytes;
                    index.notebooks.insert(id, Summary { id, name: h.name, author: h.author, created: h.created, modified: h.modified, metadata: h.metadata, incident: h.incident, bytes });
                }
                Ok(_) => tracing::warn!(file = %path.display(), "a notebook's file holds another's id: left as it is"),
                Err(e) => tracing::warn!(file = %path.display(), error = %e, "a notebook's file does not read: left as it is"),
            }
        }
        Arc::new(Self { dir, index: Mutex::new(index), bounds: (most, weight), link_base: link_base.trim_end_matches('/').to_owned() })
    }

    fn file(&self, id: u64) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    /// Every notebook's summary, by id.
    pub fn summaries(&self) -> Vec<Summary> {
        lock(&self.index).notebooks.values().cloned().collect()
    }

    /// What `f` makes of every notebook's summary, read in place.
    pub fn read_summaries<T>(&self, f: impl FnOnce(&BTreeMap<u64, Summary>) -> T) -> T {
        f(&lock(&self.index).notebooks)
    }

    /// A notebook, read from its file.
    pub fn get(&self, id: u64) -> Option<Notebook> {
        let b = std::fs::read(self.file(id)).ok()?;
        serde_json::from_slice(&b).ok()
    }

    /// `n` written whole (atomically), its summary kept: its bytes, or why not.
    fn write(&self, index: &mut Index, n: &Notebook, keep: impl FnOnce(&Index) -> Result<(), Refused>) -> Result<(), Refused> {
        let bytes = serde_json::to_vec(n).map_err(|e| invalid(e.to_string()))?;
        if bytes.len() > NOTEBOOK {
            return Err(invalid(format!("a notebook of {} MiB at the most", NOTEBOOK >> 20)));
        }
        let was = index.notebooks.get(&n.id).map_or(0, |s| s.bytes);
        if index.bytes - was + bytes.len() > self.bounds.1 {
            return Err(Refused::Full(format!("the notebooks' files are at their bound ({} MiB)", self.bounds.1 >> 20)));
        }
        keep(index)?;
        crate::sync::group::save_atomic(&self.file(n.id), &bytes).map_err(|e| Refused::Full(format!("the notebook was not saved: {e}")))?;
        index.bytes = index.bytes - was + bytes.len();
        index.notebooks.insert(n.id, Summary::of(n, bytes.len()));
        Ok(())
    }

    /// The cells of a draft: each its attributes, its id kept when `old` has it, a new one else.
    fn cells_of(d: &mut Draft, old: &[Cell], next: &mut u64) -> Vec<Cell> {
        std::mem::take(&mut d.cells)
            .into_iter()
            .map(|(id, attributes)| {
                let id = id.filter(|id| old.iter().any(|c| &c.id == id)).unwrap_or_else(|| {
                    *next += 1;
                    format!("c{next}")
                });
                Cell { id, attributes }
            })
            .collect()
    }

    /// A notebook made: what `d` says, `by` whom, now; `incident` the incident it is the postmortem of.
    pub fn create(&self, mut d: Draft, by: &str, now: i64, incident: Option<String>) -> Result<Notebook, Refused> {
        check(&d)?;
        let mut index = lock(&self.index);
        if index.notebooks.len() >= self.bounds.0 {
            return Err(Refused::Full(format!("{} notebooks at the most", self.bounds.0)));
        }
        let id = index.next_id + 1;
        let mut next = 0;
        let cells = Self::cells_of(&mut d, &[], &mut next);
        let n = Notebook { id, name: d.name, cells, time: d.time, metadata: d.metadata, template_variables: d.template_variables, author: by.to_owned(), created: now, modified: now, incident, next };
        self.write(&mut index, &n, |_| Ok(()))?;
        index.next_id = id;
        Ok(n)
    }

    /// A notebook changed: what `d` says, whole (Datadog's), its cells' ids kept where given.
    pub fn update(&self, id: u64, mut d: Draft, now: i64) -> Result<Notebook, Refused> {
        check(&d)?;
        let mut index = lock(&self.index);
        if !index.notebooks.contains_key(&id) {
            return Err(Refused::NoNotebook);
        }
        let old = self.get(id).ok_or(Refused::NoNotebook)?;
        let mut next = old.next;
        let cells = Self::cells_of(&mut d, &old.cells, &mut next);
        let n = Notebook { cells, name: d.name, time: d.time, metadata: d.metadata, template_variables: d.template_variables, modified: now, next, ..old };
        self.write(&mut index, &n, |_| Ok(()))?;
        Ok(n)
    }

    /// A notebook removed: the next id kept first (the newest's never given again), its file, it.
    pub fn remove(&self, id: u64) -> Result<bool, Refused> {
        let mut index = lock(&self.index);
        let Some(s) = index.notebooks.get(&id).cloned() else { return Ok(false) };
        let next = serde_json::to_vec(&Next { next_id: index.next_id }).map_err(|e| invalid(e.to_string()))?;
        crate::sync::group::save_atomic(&self.dir.join("next.json"), &next).map_err(|e| Refused::Full(format!("the notebook was not removed: {e}")))?;
        match std::fs::remove_file(self.file(id)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Refused::Full(format!("the notebook was not removed: {e}"))),
        }
        index.notebooks.remove(&id);
        index.bytes -= s.bytes;
        Ok(true)
    }
}

/// A draft from Datadog's body (`data.attributes`: `name`, `cells` each `{"type": "notebook_cells",
/// "attributes": …}` and, a change's, its `id`; `time`; `status`; `metadata`; `template_variables`).
pub fn draft_of(body: &Value) -> Result<Draft, Refused> {
    let data = body.get("data").and_then(Value::as_object).ok_or_else(|| invalid("the body is Datadog's: {\"data\": {\"type\": \"notebooks\", \"attributes\": …}}"))?;
    if data.get("type").and_then(Value::as_str).is_some_and(|t| t != "notebooks") {
        return Err(invalid("data.type: notebooks"));
    }
    let empty = Map::new();
    let a = data.get("attributes").and_then(Value::as_object).unwrap_or(&empty);
    let name = a.get("name").and_then(Value::as_str).ok_or_else(|| invalid("name: a text"))?.trim().to_owned();
    let cells = a.get("cells").and_then(Value::as_array).ok_or_else(|| invalid("cells: a list"))?;
    let mut out = Vec::with_capacity(cells.len().min(CELLS));
    for (at, c) in cells.iter().enumerate() {
        if c.get("type").and_then(Value::as_str).is_some_and(|t| t != "notebook_cells") {
            return Err(invalid(format!("cells[{at}].type: notebook_cells")));
        }
        let attrs = c.get("attributes").cloned().ok_or_else(|| invalid(format!("cells[{at}].attributes")))?;
        out.push((c.get("id").and_then(Value::as_str).map(str::to_owned), attrs));
    }
    let time = a.get("time").cloned().ok_or_else(|| invalid("time: {\"live_span\": \"1h\"}, or {\"start\": …, \"end\": …}"))?;
    if a.get("status").is_some_and(|s| s.as_str() != Some("published")) {
        return Err(invalid("status: published"));
    }
    let metadata = match a.get("metadata") {
        None | Some(Value::Null) => Metadata::default(),
        Some(m) => serde_json::from_value(m.clone()).map_err(|_| invalid("metadata: {\"is_template\": …, \"take_snapshots\": …, \"type\": …}"))?,
    };
    let template_variables = match a.get("template_variables") {
        None | Some(Value::Null) => None,
        Some(Value::Array(v)) => Some(v.clone()),
        Some(_) => return Err(invalid("template_variables: a list")),
    };
    Ok(Draft { name, cells: out, time, metadata, template_variables })
}

/// The notebooks' summaries that `pick` keeps, by what `sort` orders, as Datadog's list asks:
/// `author_handle` theirs alone, `exclude_author_handle` not, `query` a part of the name or the
/// author's handle (case aside), `is_template` templates or not, `type` of that type.
#[derive(Debug, Clone, Default)]
pub struct Pick {
    pub author: Option<String>,
    pub not_author: Option<String>,
    /// Lowercased (`Pick::query`).
    query: Option<String>,
    pub is_template: bool,
    pub kind: Option<String>,
}

impl Pick {
    /// `self` with `q` its query, lowercased once.
    #[must_use]
    pub fn query(self, q: Option<&str>) -> Self {
        Self { query: q.map(str::to_lowercase), ..self }
    }

    /// `self` keeping notebooks of type `k` alone.
    #[must_use]
    pub fn with_kind(self, k: &str) -> Self {
        Self { kind: Some(k.to_owned()), ..self }
    }

    /// Whether it keeps `s`: nothing allocated (a name of more than ASCII aside).
    pub fn keeps(&self, s: &Summary) -> bool {
        use super::incidents::search::contains_folded;
        self.author.as_deref().is_none_or(|a| s.author == a)
            && self.not_author.as_deref().is_none_or(|a| s.author != a)
            && self.query.as_deref().is_none_or(|q| contains_folded(&s.name, q) || contains_folded(&s.author, q))
            && s.metadata.is_template == self.is_template
            && self.kind.as_deref().is_none_or(|k| s.metadata.kind.as_deref() == Some(k))
    }
}

/// Two names' order, case aside: a byte at a time for ASCII, a character at a time else; nothing
/// allocated.
fn folded(a: &str, b: &str) -> std::cmp::Ordering {
    if a.is_ascii() && b.is_ascii() {
        a.bytes().map(|c| c.to_ascii_lowercase()).cmp(b.bytes().map(|c| c.to_ascii_lowercase()))
    } else {
        a.chars().flat_map(char::to_lowercase).cmp(b.chars().flat_map(char::to_lowercase))
    }
}

/// How a list is ordered: by `modified` (Datadog's default), `name` or `created`, `desc` (its
/// default) or `asc`.
pub fn sort(list: &mut [&Summary], field: &str, desc: bool) -> Result<(), Refused> {
    match field {
        "modified" => list.sort_by_key(|s| (s.modified, s.id)),
        "created" => list.sort_by_key(|s| (s.created, s.id)),
        "name" => list.sort_by(|a, b| folded(&a.name, &b.name).then(a.id.cmp(&b.id))),
        _ => return Err(invalid("sort_field: modified, name or created")),
    }
    if desc {
        list.reverse();
    }
    Ok(())
}

/// A notebook's time in ms, from and to, at `now`: the last of its span, or its start and end
/// (a span without a fixed length, the last day).
pub fn window(t: &Value, now: i64) -> (i64, i64) {
    let at = |k: &str| {
        t.get(k)
            .and_then(Value::as_str)
            .and_then(|s| time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok())
            .map(|t| i64::try_from(t.unix_timestamp_nanos() / 1_000_000).unwrap_or(i64::MAX))
    };
    match (at("start"), at("end")) {
        (Some(s), Some(e)) => (s, e),
        _ => {
            let span = t.get("live_span").and_then(Value::as_str).and_then(|s| LIVE_SPANS.iter().find(|(n, _)| *n == s)).and_then(|(_, ms)| *ms).unwrap_or(86_400_000);
            (now - span, now)
        }
    }
}

/// Cells' counts by kind (what a list of them says).
pub fn kinds(cells: &[Cell]) -> HashMap<&str, usize> {
    let mut out = HashMap::new();
    for c in cells {
        if let Some(k) = c.attributes.pointer("/definition/type").and_then(Value::as_str) {
            *out.entry(k).or_default() += 1;
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use serde_json::json;

    use super::*;

    fn markdown(text: &str) -> Value {
        json!({ "definition": { "type": "markdown", "text": text } })
    }

    fn graph(q: &str) -> Value {
        json!({ "definition": { "type": "timeseries", "requests": [{ "q": q, "display_type": "line" }] }, "graph_size": "m" })
    }

    fn draft(name: &str, cells: Vec<Value>) -> Draft {
        Draft { name: name.into(), cells: cells.into_iter().map(|c| (None, c)).collect(), time: json!({ "live_span": "1h" }), ..Draft::default() }
    }

    /// Made, read, changed (its cells' ids kept where given, new ones given), listed and removed;
    /// kept through its file; what Datadog's API refuses refused, saying why.
    #[test]
    fn a_notebook_is_made_changed_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let kept = Notebooks::open(dir.path(), "");
        let n = kept.create(draft("Checkout latency", vec![markdown("# Why"), graph("avg:app.latency.p95{service:checkout}")]), "user:ada", 1, None).unwrap();
        assert_eq!((n.id, n.cells.len(), n.cells[0].id.as_str(), n.cells[1].id.as_str()), (1, 2, "c1", "c2"));
        assert_eq!(kept.get(1), Some(n.clone()));
        // Changed: its first cell kept by id, a new one added, the second dropped.
        let d = Draft { cells: vec![(Some("c1".into()), markdown("# Why, again")), (None, markdown("A note")), (Some("c9".into()), markdown("not its"))], ..draft("Checkout latency, after", vec![]) };
        let n = kept.update(1, d, 2).unwrap();
        assert_eq!(n.cells.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["c1", "c3", "c4"], "an id it does not have is a new one's");
        assert_eq!((n.name.as_str(), n.created, n.modified, n.author.as_str()), ("Checkout latency, after", 1, 2, "user:ada"));
        // Refused, saying why.
        for (d, says) in [
            (draft(&"x".repeat(81), vec![]), "name"),
            (draft("x", vec![json!({ "definition": { "type": "geomap" } })]), "cell of type"),
            (draft("x", vec![json!({ "definition": { "type": "markdown" } })]), "Markdown cell's text"),
            (draft("x", vec![json!({ "definition": { "type": "timeseries", "requests": [] } })]), "requests"),
            (draft("x", vec![json!({ "definition": { "type": "heatmap", "requests": [{}, {}] } })]), "one request"),
            (draft("x", vec![json!({ "definition": { "type": "markdown", "text": "" }, "graph_size": "xxl" })]), "graph_size"),
            (Draft { time: json!({ "live_span": "2h" }), ..draft("x", vec![]) }, "live_span"),
            (Draft { time: json!({ "start": "2026-10-22T12:00:00Z", "end": "2026-10-22T11:00:00Z" }), ..draft("x", vec![]) }, "start before its end"),
            (Draft { metadata: Metadata { kind: Some("diary".into()), ..Metadata::default() }, ..draft("x", vec![]) }, "metadata.type"),
        ] {
            let e = kept.create(d, "u", 3, None).unwrap_err().to_string();
            assert!(e.contains(says), "{says}: {e}");
        }
        assert_eq!(kept.update(7, draft("x", vec![]), 3).unwrap_err(), Refused::NoNotebook);
        // Kept through its file; the newest removed, its id not given again.
        let two = kept.create(draft("Runbook", vec![]), "user:bob", 4, None).unwrap();
        drop(kept);
        let again = Notebooks::open(dir.path(), "");
        assert_eq!(again.summaries().iter().map(|s| (s.id, s.name.as_str())).collect::<Vec<_>>(), [(1, "Checkout latency, after"), (2, "Runbook")]);
        assert!(again.remove(two.id).unwrap() && !again.remove(two.id).unwrap());
        drop(again);
        let again = Notebooks::open(dir.path(), "");
        assert_eq!(again.create(draft("Next", vec![]), "u", 5, None).unwrap().id, 3);
    }

    /// Datadog's body read as a draft; a list picked and sorted as Datadog's list asks.
    #[test]
    fn datadog_s_body_reads_and_its_list_picks() {
        let body = json!({ "data": { "type": "notebooks", "attributes": {
            "name": "Example", "status": "published", "time": { "live_span": "1h" },
            "cells": [{ "type": "notebook_cells", "attributes": markdown("hi") }, { "id": "c1", "type": "notebook_cells", "attributes": graph("avg:x{*}") }],
            "metadata": { "is_template": false, "take_snapshots": false, "type": "investigation" }
        } } });
        let d = draft_of(&body).unwrap();
        assert_eq!((d.name.as_str(), d.cells.len(), d.cells[1].0.as_deref(), d.metadata.kind.as_deref()), ("Example", 2, Some("c1"), Some("investigation")));
        for bad in [json!({}), json!({ "data": { "type": "dashboards", "attributes": {} } }), json!({ "data": { "attributes": { "name": "x", "cells": [], "time": {}, "status": "draft" } } })] {
            assert!(draft_of(&bad).is_err(), "{bad}");
        }
        let s = |id: u64, name: &str, author: &str, modified: i64, kind: Option<&str>, template: bool| Summary {
            id,
            name: name.into(),
            author: author.into(),
            created: id as i64,
            modified,
            metadata: Metadata { is_template: template, take_snapshots: false, kind: kind.map(str::to_owned) },
            incident: None,
            bytes: 1,
        };
        let all = [
            s(1, "Checkout postmortem", "ada", 30, Some("postmortem"), false),
            s(2, "DB runbook", "bob", 20, Some("runbook"), false),
            s(3, "Postmortem template", "ada", 10, Some("postmortem"), true),
        ];
        let picked = |p: Pick| -> Vec<u64> {
            let mut l: Vec<&Summary> = all.iter().filter(|x| p.keeps(x)).collect();
            sort(&mut l, "modified", true).unwrap();
            l.iter().map(|x| x.id).collect()
        };
        assert_eq!(picked(Pick::default()), [1, 2], "templates apart, as Datadog's list keeps them");
        assert_eq!(picked(Pick { is_template: true, ..Pick::default() }), [3]);
        assert_eq!(picked(Pick::default().query(Some("POSTMORTEM"))), [1]);
        assert_eq!(picked(Pick { author: Some("bob".into()), ..Pick::default() }), [2]);
        assert_eq!(picked(Pick { not_author: Some("bob".into()), kind: Some("postmortem".into()), ..Pick::default() }), [1]);
        let mut l: Vec<&Summary> = all.iter().collect();
        sort(&mut l, "name", false).unwrap();
        assert_eq!(l.iter().map(|x| x.id).collect::<Vec<_>>(), [1, 2, 3]);
        assert!(sort(&mut l, "size", true).is_err());
    }

    /// Bounded: a notebook past its bytes refused; the files past their bound, none more made.
    #[test]
    fn notebooks_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let kept = Notebooks::open_bounded(dir.path(), 2, 4_000, "");
        kept.create(draft("one", vec![markdown(&"x".repeat(1_000))]), "u", 1, None).unwrap();
        assert!(matches!(kept.create(draft("two", vec![markdown(&"x".repeat(3_000))]), "u", 2, None), Err(Refused::Full(_))), "past the bytes");
        kept.create(draft("two", vec![]), "u", 2, None).unwrap();
        assert!(matches!(kept.create(draft("three", vec![]), "u", 3, None), Err(Refused::Full(_))), "past the count");
        let big = draft("big", vec![markdown(&"x".repeat(NOTEBOOK))]);
        assert!(matches!(Notebooks::open(tempfile::tempdir().unwrap().path(), "").create(big, "u", 1, None), Err(Refused::Invalid(_))), "a notebook past its own bound");
        assert_eq!(window(&json!({ "live_span": "4h" }), 100_000_000), (100_000_000 - 14_400_000, 100_000_000));
        assert_eq!(window(&json!({ "start": "1970-01-01T00:00:01Z", "end": "1970-01-01T00:00:02Z" }), 0), (1_000, 2_000));
    }
}
