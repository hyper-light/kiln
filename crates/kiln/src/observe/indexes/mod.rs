//! Log indexes, Datadog's (`docs/observe.md` §8.10): which records are kept for search, how many a
//! day and how long. A record goes into the first index, in their order, whose filter it matches;
//! there the first of its enabled exclusion filters that matches decides alone, excluding it at
//! its rate (`route`). A record of no index, or excluded, or past its index's daily quota is not
//! kept for search, and is still seen live, archived, and counted by metrics from logs.
//!
//! Each index's records are a part of the store of their own (`kiln_obs::logs::store::Part`: its
//! files, its retention); those not kept for search are the part of those
//! ([`TRANSIT`](kiln_obs::logs::store::TRANSIT)), read live, archived and counted, then let go; kiln's
//! events are a part of their own, whatever the indexes say. Until one is made, there is one index,
//! `main`, of every record (Datadog's), its part the main one: every record before indexes.
//!
//! The server keeps them (`<observe>/indexes.json`: each in its order with its part, the names and
//! parts of those removed, never used again) and tells every runner the list; every node routes the
//! records it takes. Only a part said removed is let go: a part the list does not name (a list that
//! did not read, one not yet heard) is kept as long as the logs are.

pub mod http;
pub mod quota;
pub mod route;

use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};

/// Indexes at the most (Datadog's, unless asked of its support).
pub const INDEXES: usize = 100;
/// Exclusion filters an index has at the most.
pub const EXCLUSIONS: usize = 100;
/// A query's bytes at the most (the log search's own bound).
pub const QUERY_BYTES: usize = kiln_obs::logs::search::MAX_SEARCH_BYTES;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A query, as Datadog's API writes one.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Filter {
    #[serde(default)]
    pub query: String,
}

/// An exclusion filter's query, its rate, and what it samples by.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExclusionQuery {
    #[serde(default = "every")]
    pub query: String,
    /// Of the records it matches, the share excluded: 1.0 every one.
    pub sample_rate: f64,
    /// Whose values are sampled (`@ci.job_id`): a record without it kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_attribute: Option<String>,
}

fn every() -> String {
    "*".into()
}

/// An exclusion filter, Datadog's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExclusionDef {
    pub name: String,
    #[serde(default)]
    pub is_enabled: bool,
    pub filter: ExclusionQuery,
}

/// When a daily quota's day begins: `HH:00` at `(+|-)HH:00` from UTC.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reset {
    pub reset_time: String,
    pub reset_utc_offset: String,
}

/// An index, as Datadog's API takes and gives one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexDef {
    pub name: String,
    pub filter: Filter,
    #[serde(default)]
    pub exclusion_filters: Vec<ExclusionDef>,
    /// Records kept a day at the most (none: no quota).
    #[serde(default)]
    pub daily_limit: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily_limit_reset: Option<Reset>,
    /// Of the quota, the share at which a warning is said (50 to 99.99).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily_limit_warning_threshold_percentage: Option<f64>,
    /// Days its records are kept for search (none: the store's own retention).
    #[serde(default)]
    pub num_retention_days: Option<u32>,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// An index's name, Datadog's: a letter first, then lowercase letters, digits and hyphens.
pub fn check_name(name: &str) -> Result<(), String> {
    let ok = name.len() <= 100 && name.bytes().next().is_some_and(|b| b.is_ascii_lowercase()) && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if ok { Ok(()) } else { Err(format!("an index's name: a lowercase letter first, then lowercase letters, digits and hyphens, 100 at the most ({name:?})")) }
}

/// `HH:00`'s hour, 0 to 23.
fn hour_of(s: &str) -> Option<i64> {
    let (h, m) = s.split_once(':')?;
    (h.len() == 2 && m == "00").then(|| h.parse::<i64>().ok()).flatten().filter(|h| (0..24).contains(h))
}

/// `(+|-)HH:00`'s hours, -12 to +14.
fn offset_of(s: &str) -> Option<i64> {
    let (sign, rest) = (s.get(..1)?, s.get(1..)?);
    let h = hour_of(rest).or_else(|| (rest == "24:00").then_some(24))?;
    let h = match sign {
        "+" => h,
        "-" => -h,
        _ => return None,
    };
    (-12..=14).contains(&h).then_some(h)
}

impl IndexDef {
    /// Its quota's day's hour and offset from UTC: 14:00 UTC unless said (Datadog's).
    pub fn reset(&self) -> (i64, i64) {
        self.daily_limit_reset.as_ref().and_then(|r| Some((hour_of(&r.reset_time)?, offset_of(&r.reset_utc_offset)?))).unwrap_or((14, 0))
    }

    /// Checked as Datadog checks one, and its queries read.
    pub fn check(&self) -> Result<(), String> {
        check_name(&self.name)?;
        let query = |q: &str, of: &str| -> Result<(), String> {
            if q.len() > QUERY_BYTES {
                return Err(format!("{of}'s query: {QUERY_BYTES} bytes at the most"));
            }
            kiln_obs::logs::search::parse(q).map(drop).map_err(|e| format!("{of}'s query: {e}"))
        };
        query(&self.filter.query, "its filter")?;
        if self.exclusion_filters.len() > EXCLUSIONS {
            return Err(format!("{EXCLUSIONS} exclusion filters at the most"));
        }
        for e in &self.exclusion_filters {
            if e.name.trim().is_empty() || e.name.len() > 200 {
                return Err("an exclusion filter's name: 1 to 200 characters".into());
            }
            query(&e.filter.query, &format!("exclusion filter {:?}", e.name))?;
            if !(0.0..=1.0).contains(&e.filter.sample_rate) {
                return Err(format!("exclusion filter {:?}: a sample rate from 0 to 1", e.name));
            }
            if e.filter.sample_attribute.as_deref().is_some_and(|a| a.trim_start_matches('@').is_empty()) {
                return Err(format!("exclusion filter {:?}: an attribute to sample by, named", e.name));
            }
        }
        if let Some(r) = &self.daily_limit_reset {
            hour_of(&r.reset_time).ok_or("a quota's reset_time: HH:00, 00 to 23")?;
            offset_of(&r.reset_utc_offset).ok_or("a quota's reset_utc_offset: +HH:00 or -HH:00, -12 to +14")?;
        }
        if let Some(p) = self.daily_limit_warning_threshold_percentage
            && !(50.0..=99.99).contains(&p)
        {
            return Err("a quota's warning threshold: 50 to 99.99 percent".into());
        }
        if self.num_retention_days == Some(0) {
            return Err("a retention of a day at the least".into());
        }
        if self.tags.iter().any(|t| !t.contains(':')) {
            return Err("an index's tags: key:value".into());
        }
        Ok(())
    }
}

/// An index as kept: its definition and its part of the store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Kept {
    #[serde(flatten)]
    pub def: IndexDef,
    /// Its part of the store: `main`'s the main part.
    #[serde(default)]
    pub part: u16,
    #[serde(default)]
    pub created: i64,
    #[serde(default)]
    pub modified: i64,
}

// An index's numbers are checked (a rate from 0 to 1, a threshold from 50 to 99.99), never NaN:
// equal to themselves, as `Eq` asks (the wire's messages are compared whole).
impl Eq for Kept {}

/// The name of the index every record goes into until others are made (Datadog's).
pub const MAIN_NAME: &str = "main";

/// The parts indexes are given, from the first after the main one, never again once given.
const FIRST_PART: u16 = 1;
/// The last an index is given: those above are kiln's own (its events', those not kept).
const LAST_PART: u16 = kiln_obs::logs::store::EVENTS - 1;

/// `main` as it is before it is changed: every record, kept as long as the store keeps logs.
pub fn main_def() -> IndexDef {
    IndexDef {
        name: MAIN_NAME.into(),
        filter: Filter { query: "*".into() },
        exclusion_filters: Vec::new(),
        daily_limit: None,
        daily_limit_reset: None,
        daily_limit_warning_threshold_percentage: None,
        num_retention_days: None,
        tags: Vec::new(),
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Saved {
    /// In their order.
    #[serde(default)]
    indexes: Vec<Kept>,
    /// Those removed: their names are not used again (a record kept by one is not another's).
    #[serde(default)]
    removed: Vec<String>,
    /// Their parts: their records no longer read, their files let go (`main`'s until it is made
    /// again).
    #[serde(default)]
    removed_parts: Vec<u16>,
    /// The part the next index made is given.
    #[serde(default = "first_part")]
    next_part: u16,
}

fn first_part() -> u16 {
    FIRST_PART
}

impl Default for Saved {
    /// What there is before indexes are made: `main`.
    fn default() -> Self {
        Self { indexes: vec![Kept { def: main_def(), part: kiln_obs::logs::store::MAIN, created: 0, modified: 0 }], removed: Vec::new(), removed_parts: Vec::new(), next_part: FIRST_PART }
    }
}

/// The server's indexes.
pub struct Indexes {
    path: PathBuf,
    saved: Mutex<Saved>,
}

impl Indexes {
    /// The indexes kept in `dir`: `main` alone where there are none. A list that does not read is
    /// kept aside (`indexes.json.unread-<ms>`) and said; until indexes are made again, `main` takes
    /// every record, and no part is let go for it (only parts said removed are).
    pub fn open(dir: &Path) -> Self {
        let path = dir.join("indexes.json");
        let saved = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                let aside = dir.join(format!("indexes.json.unread-{}", crate::util::now_ms()));
                let kept = std::fs::rename(&path, &aside).map_or_else(|e| format!("not kept aside ({e})"), |()| format!("kept aside as {}", aside.display()));
                tracing::error!(error = %e, "observe's log indexes did not read, {kept}: main takes every record until indexes are made again");
                Saved::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Saved::default(),
            Err(e) => {
                tracing::error!(path = %path.display(), error = %e, "observe's log indexes did not read: main takes every record until they do");
                Saved::default()
            }
        };
        Self { path, saved: Mutex::new(saved) }
    }

    /// Every index in its order.
    pub fn list(&self) -> Vec<Kept> {
        lock(&self.saved).indexes.clone()
    }

    pub fn get(&self, name: &str) -> Option<Kept> {
        lock(&self.saved).indexes.iter().find(|i| i.def.name == name).cloned()
    }

    /// The indexes as every node routes by them, in order.
    pub fn defs(&self) -> Vec<Kept> {
        lock(&self.saved).indexes.clone()
    }

    /// The parts of indexes removed: their records no longer read, their files let go.
    pub fn removed_parts(&self) -> Vec<u16> {
        lock(&self.saved).removed_parts.clone()
    }

    fn save(&self, saved: &Saved) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(saved).map_err(|e| e.to_string())?;
        crate::sync::group::save_atomic(&self.path, &bytes).map_err(|e| e.to_string())
    }

    /// A new index, last, given a part never given before and not `taken` (one the store holds or
    /// has a cutoff for: an index's whose list did not read). `Err((true, …))`: one too many
    /// (Datadog's 422); else not made as asked.
    pub fn create(&self, def: IndexDef, taken: &dyn Fn(u16) -> bool) -> Result<Kept, (bool, String)> {
        def.check().map_err(|e| (false, e))?;
        let mut saved = lock(&self.saved);
        if saved.indexes.iter().any(|i| i.def.name == def.name) {
            return Err((false, format!("an index named {:?} is there already", def.name)));
        }
        if saved.removed.contains(&def.name) && def.name != MAIN_NAME {
            return Err((false, format!("an index named {:?} was removed: its name is not used again", def.name)));
        }
        if saved.indexes.len() >= INDEXES {
            return Err((true, format!("{INDEXES} indexes at the most")));
        }
        // `main`, removed, is made again as it was: of the main part (what it held before is past
        // its cutoff, which never goes back).
        let part = if def.name == MAIN_NAME && !saved.indexes.iter().any(|i| i.part == kiln_obs::logs::store::MAIN) {
            saved.removed_parts.retain(|p| *p != kiln_obs::logs::store::MAIN);
            kiln_obs::logs::store::MAIN
        } else {
            while saved.next_part <= LAST_PART && taken(saved.next_part) {
                saved.next_part += 1;
            }
            if saved.next_part > LAST_PART {
                return Err((true, "every part an index may have has been given".into()));
            }
            saved.next_part += 1;
            saved.next_part - 1
        };
        let now = crate::util::now_ms();
        let kept = Kept { def, part, created: now, modified: now };
        saved.indexes.push(kept.clone());
        self.save(&saved).map_err(|e| (false, e))?;
        Ok(kept)
    }

    /// An index changed whole, but its name (none: not there). `disable_daily_limit`: its quota
    /// gone, whatever the body says of it.
    pub fn put(&self, name: &str, mut def: IndexDef, disable_daily_limit: bool) -> Result<Option<Kept>, String> {
        def.name = name.to_owned();
        if disable_daily_limit {
            def.daily_limit = None;
        }
        def.check()?;
        let mut saved = lock(&self.saved);
        let Some(i) = saved.indexes.iter_mut().find(|i| i.def.name == name) else { return Ok(None) };
        i.def = def;
        i.modified = crate::util::now_ms();
        let out = i.clone();
        self.save(&saved)?;
        Ok(Some(out))
    }

    /// An index removed: whether it was there. Its name is not used again; its part is let go.
    pub fn delete(&self, name: &str) -> Result<bool, String> {
        let mut saved = lock(&self.saved);
        let Some(at) = saved.indexes.iter().position(|i| i.def.name == name) else { return Ok(false) };
        let gone = saved.indexes.remove(at);
        saved.removed.push(name.to_owned());
        if !saved.removed_parts.contains(&gone.part) {
            saved.removed_parts.push(gone.part);
        }
        self.save(&saved)?;
        Ok(true)
    }

    /// The indexes' names in their order.
    pub fn order(&self) -> Vec<String> {
        lock(&self.saved).indexes.iter().map(|i| i.def.name.clone()).collect()
    }

    /// The indexes put in the order given: every one of them, each once.
    pub fn set_order(&self, names: &[String]) -> Result<(), String> {
        let mut saved = lock(&self.saved);
        let have: std::collections::BTreeSet<&str> = saved.indexes.iter().map(|i| i.def.name.as_str()).collect();
        let given: std::collections::BTreeSet<&str> = names.iter().map(String::as_str).collect();
        if given != have || given.len() != names.len() {
            return Err("an order names every index, each once".into());
        }
        let mut by: std::collections::HashMap<String, Kept> = saved.indexes.drain(..).map(|i| (i.def.name.clone(), i)).collect();
        saved.indexes = names.iter().filter_map(|n| by.remove(n)).collect();
        self.save(&saved)
    }
}

/// The indexes as this node routes by them, compiled; and those over their daily quota.
#[derive(Default, Clone)]
pub struct Active {
    /// In order: each index's way in, its part and its name.
    routes: Vec<route::Route>,
    parts: Vec<u16>,
    names: Vec<String>,
    /// Every record but kiln's events into this part, none read: one index, of every record,
    /// nothing excluded (`main` as it is made).
    all_into: Option<u16>,
    /// The parts over their daily quota: what goes into them is not kept for search, until their
    /// day turns (the server says so).
    limited: Vec<u16>,
}

impl Active {
    /// The parts over their daily quota now.
    pub fn limited(&self) -> &[u16] {
        &self.limited
    }

    /// The name of the index of `part`, if there is one.
    pub fn name_of(&self, part: u16) -> Option<&str> {
        self.parts.iter().position(|p| *p == part).and_then(|i| self.names.get(i)).map(String::as_str)
    }
}

/// What this node routes by now.
fn active() -> &'static kiln_obs::lock::RwLock<std::sync::Arc<Active>> {
    static ACTIVE: std::sync::OnceLock<kiln_obs::lock::RwLock<std::sync::Arc<Active>>> = std::sync::OnceLock::new();
    ACTIVE.get_or_init(|| {
        let a = compile(&Saved::default().indexes, Vec::new());
        kiln_obs::lock::RwLock::new(std::sync::Arc::new(a))
    })
}

/// A query that matches every record: none, `*`.
fn matches_all(q: &str) -> bool {
    matches!(q.trim(), "" | "*")
}

/// `kept` compiled to route by, `limited` over their quota (a bench's, a test's; [`activate`]
/// makes this node's).
pub fn compile(kept: &[Kept], limited: Vec<u16>) -> Active {
    let mut a = Active { limited, ..Active::default() };
    for k in kept {
        let parse = |q: &str| kiln_obs::logs::search::parse(if q.trim().is_empty() { "*" } else { q });
        let filter = match parse(&k.def.filter.query) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(index = %k.def.name, error = %e, "an index's filter does not read here; it is skipped");
                continue;
            }
        };
        let mut exclusions = Vec::new();
        for e in &k.def.exclusion_filters {
            match parse(&e.filter.query) {
                Ok(query) => exclusions.push(route::Exclusion {
                    enabled: e.is_enabled,
                    query,
                    rate: e.filter.sample_rate,
                    attribute: e.filter.sample_attribute.as_deref().map(|a| a.trim_start_matches('@').to_owned()),
                }),
                Err(err) => tracing::warn!(index = %k.def.name, exclusion = %e.name, error = %err, "an exclusion filter does not read here; it is skipped"),
            }
        }
        a.routes.push(route::Route { filter, exclusions });
        a.parts.push(k.part);
        a.names.push(k.def.name.clone());
    }
    if let ([k], [r]) = (kept, a.routes.as_slice())
        && matches_all(&k.def.filter.query)
        && r.exclusions.iter().all(|e| !e.enabled || e.rate <= 0.0)
    {
        a.all_into = Some(k.part);
    }
    a
}

/// `kept` routed by from now on (one that does not read, a version's before, skipped and said).
pub fn activate(kept: &[Kept]) {
    let limited = snapshot().limited.clone();
    *active().write().unwrap_or_else(PoisonError::into_inner) = std::sync::Arc::new(compile(kept, limited));
}

/// The parts over their daily quota now (the server's word).
pub fn set_limited(parts: Vec<u16>) {
    let mut a = active().write().unwrap_or_else(PoisonError::into_inner);
    if a.limited != parts {
        *a = std::sync::Arc::new(Active { limited: parts, ..(**a).clone() });
    }
}

/// What this node routes by now.
pub fn snapshot() -> std::sync::Arc<Active> {
    active().read().unwrap_or_else(PoisonError::into_inner).clone()
}

/// What became of records, as Datadog's estimated usage counts them: by index (its part; none,
/// no index's) and whether excluded, how many and their bytes.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Usage {
    pub rows: Vec<UsageRow>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsageRow {
    pub part: Option<u16>,
    pub excluded: bool,
    pub records: u64,
    pub bytes: u64,
}

impl Usage {
    fn add(&mut self, fate: Fate, records: u64, bytes: u64) {
        let (part, excluded) = match fate {
            Fate::Kept(p) => (Some(p), false),
            Fate::Excluded(p) => (Some(p), true),
            Fate::Unindexed => (None, false),
            Fate::Event => return,
        };
        match self.rows.iter_mut().find(|r| r.part == part && r.excluded == excluded) {
            Some(r) => {
                r.records += records;
                r.bytes += bytes;
            }
            None => self.rows.push(UsageRow { part, excluded, records, bytes }),
        }
    }

    /// Another's counts added to these.
    pub fn merge(&mut self, other: &Usage) {
        for r in &other.rows {
            let fate = match (r.part, r.excluded) {
                (Some(p), false) => Fate::Kept(p),
                (Some(p), true) => Fate::Excluded(p),
                (None, _) => Fate::Unindexed,
            };
            self.add(fate, r.records, r.bytes);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.rows.iter().all(|r| r.records == 0)
    }
}

/// What became of a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fate {
    /// Kept by the index of this part (over its quota too: Datadog counts it its index's).
    Kept(u16),
    Excluded(u16),
    Unindexed,
    /// kiln's event: not an index's.
    Event,
}

/// A record's part, and what became of it.
fn place<F: kiln_obs::logs::search::Fields>(a: &Active, r: &F, ts: i64, s: &mut kiln_obs::logs::search::RowScratch) -> (u16, Fate) {
    use kiln_obs::logs::store::{EVENTS, TRANSIT};
    if r.source() == "kiln.event" {
        return (EVENTS, Fate::Event);
    }
    let routed = match a.all_into {
        Some(_) => route::Routed::Indexed(0),
        None => route::route(&a.routes, r, ts, s),
    };
    match routed {
        route::Routed::Indexed(i) => match a.parts.get(i).copied() {
            Some(part) if a.limited.contains(&part) => (TRANSIT, Fate::Kept(part)),
            Some(part) => (part, Fate::Kept(part)),
            None => (TRANSIT, Fate::Unindexed),
        },
        route::Routed::Excluded(i, _) => match a.parts.get(i).copied() {
            Some(part) => (TRANSIT, Fate::Excluded(part)),
            None => (TRANSIT, Fate::Unindexed),
        },
        route::Routed::Unindexed => (TRANSIT, Fate::Unindexed),
    }
}

/// Each run of `logs` given its part of the store (`Logs::into`), a run split where its records
/// go to more than one; and what became of them. Records a pipeline made rows (no event among
/// them) are read in place, and written again only where a run splits.
pub fn route_logs(logs: &mut super::ingest::Logs) -> Usage {
    route_logs_by(&snapshot(), logs)
}

/// [`route_logs`] by `a`.
pub fn route_logs_by(a: &Active, logs: &mut super::ingest::Logs) -> Usage {
    use super::ingest::LogPart;
    use kiln_obs::logs::store::{EVENTS, MAIN};
    let mut usage = Usage::default();
    // Every record into one part, a run at a time: nothing read but each record's source.
    if let Some(part) = a.all_into
        && a.limited.is_empty()
    {
        let mut parts = Vec::with_capacity(logs.parts.len());
        let mut into_parts = Vec::with_capacity(logs.parts.len());
        for p in std::mem::take(&mut logs.parts) {
            match p {
                LogPart::Records(run) => {
                    // One pass: kiln's events among them, and what the rest weigh.
                    let (mut events, mut bytes) = (0usize, 0u64);
                    for r in &run {
                        if r.source == "kiln.event" {
                            events += 1;
                        } else {
                            bytes += super::ingest::record_bytes(r) as u64;
                        }
                    }
                    let kept = run.len() - events;
                    if kept > 0 {
                        usage.add(Fate::Kept(part), kept as u64, bytes);
                    }
                    if events == 0 || kept == 0 {
                        into_parts.push(if events > 0 { EVENTS } else { part });
                        parts.push(LogPart::Records(run));
                    } else {
                        let (ev, rest): (Vec<_>, Vec<_>) = run.into_iter().partition(|r| r.source == "kiln.event");
                        into_parts.extend([part, EVENTS]);
                        parts.extend([LogPart::Records(rest), LogPart::Records(ev)]);
                    }
                }
                LogPart::Rows { raw, rows, last_ts } => {
                    usage.add(Fate::Kept(part), u64::from(rows), raw.len() as u64);
                    into_parts.push(part);
                    parts.push(LogPart::Rows { raw, rows, last_ts });
                }
            }
        }
        logs.parts = parts;
        logs.into = into_parts;
        return usage;
    }
    let mut s = kiln_obs::logs::search::RowScratch::default();
    let mut parts = Vec::with_capacity(logs.parts.len());
    let mut into_parts = Vec::with_capacity(logs.parts.len());
    // A run's records' parts, and a run of rows' each row's part, place and time: kept from run to
    // run (their buffers reused). Each part met, in order, its records and their bytes.
    let mut each: Vec<u16> = Vec::new();
    let mut at: Vec<(u16, usize, usize, i64)> = Vec::new();
    let mut tally: Vec<(u16, usize, usize)> = Vec::new();
    let count = |tally: &mut Vec<(u16, usize, usize)>, part: u16, bytes: usize| match tally.iter_mut().find(|(p, ..)| *p == part) {
        Some((_, n, b)) => {
            *n += 1;
            *b += bytes;
        }
        None => tally.push((part, 1, bytes)),
    };
    for p in std::mem::take(&mut logs.parts) {
        match p {
            LogPart::Records(run) => {
                each.clear();
                tally.clear();
                for r in &run {
                    let (part, fate) = place(a, r, r.ts, &mut s);
                    let bytes = super::ingest::record_bytes(r);
                    usage.add(fate, 1, bytes as u64);
                    count(&mut tally, part, bytes);
                    each.push(part);
                }
                // One part's: on whole, nothing moved.
                if let [(part, ..)] = tally.as_slice() {
                    into_parts.push(*part);
                    parts.push(LogPart::Records(run));
                    continue;
                }
                // Split: each part's records moved into a run of their own, made their size.
                let mut by: Vec<(u16, Vec<Record>)> = tally.iter().map(|&(part, n, _)| (part, Vec::with_capacity(n))).collect();
                for (r, part) in run.into_iter().zip(&each) {
                    if let Some((_, v)) = by.iter_mut().find(|(p, _)| p == part) {
                        v.push(r);
                    }
                }
                for (part, run) in by {
                    into_parts.push(part);
                    parts.push(LogPart::Records(run));
                }
            }
            LogPart::Rows { raw, rows, last_ts } => {
                // Each row's part, read in place; the run whole where they are one part's.
                at.clear();
                tally.clear();
                let mut it = kiln_obs::logs::record::Rows::new(&raw, rows as usize);
                let mut start = 0;
                while let Ok(Some((r, row))) = it.next_row_bytes() {
                    let (part, fate) = place(a, &r, r.ts, &mut s);
                    // A row's bytes as written: a run's are its bytes where it is not read (above),
                    // so what a collector's records come to is the same however many indexes.
                    usage.add(fate, 1, row.len() as u64);
                    count(&mut tally, part, row.len());
                    at.push((part, start, start + row.len(), r.ts));
                    start += row.len();
                }
                // One part's, or rows that do not read whole: on whole (the store's check of rows
                // refuses those as it would have; none is dropped here unsaid).
                if at.len() < rows as usize || tally.len() <= 1 {
                    into_parts.push(at.first().map_or(MAIN, |(p, ..)| *p));
                    parts.push(LogPart::Rows { raw, rows, last_ts });
                    continue;
                }
                // Split: each part's rows copied as written, but for their times, chained anew
                // from the part's row before; into buffers of their size (a time's few bytes more
                // a row at the most, as its gap from the row before grows).
                let mut by: Vec<(u16, Vec<u8>, u32, i64)> = tally.iter().map(|&(part, n, bytes)| (part, Vec::with_capacity(bytes + 4 * n), 0, 0)).collect();
                for &(part, start, end, ts) in &at {
                    let (Some(row), Some((_, out, n, prev))) = (raw.get(start..end), by.iter_mut().find(|(p, ..)| *p == part)) else { continue };
                    if kiln_obs::logs::record::put_row_rechained(out, row, ts, *prev) {
                        *prev = ts;
                        *n += 1;
                    }
                }
                for (part, raw, rows, last_ts) in by {
                    into_parts.push(part);
                    parts.push(LogPart::Rows { raw, rows, last_ts });
                }
            }
        }
    }
    logs.parts = parts;
    logs.into = into_parts;
    usage
}

use kiln_obs::logs::record::Record;

/// What records taken here came to since it was last written as metrics.
fn taken() -> &'static Mutex<Usage> {
    static T: std::sync::OnceLock<Mutex<Usage>> = std::sync::OnceLock::new();
    T.get_or_init(Mutex::default)
}

/// What routed records came to, counted once they are taken (an offer refused is offered again).
pub fn count(u: &Usage) {
    if !u.is_empty() {
        lock(taken()).merge(u);
    }
}

/// Datadog's estimated usage as metrics (`datadog.estimated_usage.logs.ingested_events`, `…_bytes`,
/// counts): what records taken here since the last time came to, by index (`datadog_index`, `N/A`
/// for none) and whether excluded (`datadog_is_excluded`, `N/A` for none), of this node.
pub fn usage_points(host: &str, now_ms: i64) -> Vec<(kiln_obs::batch::SeriesKey, Vec<(i64, f64)>)> {
    let u = std::mem::take(&mut *lock(taken()));
    let a = snapshot();
    let mut out = Vec::new();
    for r in u.rows {
        let index = r.part.map_or("N/A", |p| a.name_of(p).unwrap_or("N/A"));
        let excluded = match (r.part, r.excluded) {
            (None, _) => "N/A",
            (_, true) => "true",
            (_, false) => "false",
        };
        let tags = [("datadog_index", index), ("datadog_is_excluded", excluded), ("host", host)];
        #[allow(clippy::cast_precision_loss)]
        for (metric, v) in [("datadog.estimated_usage.logs.ingested_events", r.records), ("datadog.estimated_usage.logs.ingested_bytes", r.bytes)] {
            if let Ok(k) = kiln_obs::batch::SeriesKey::new(metric, tags, kiln_obs::batch::Kind::Count) {
                out.push((k, vec![(now_ms, v as f64)]));
            }
        }
    }
    out
}

/// How often what records came to is written as metrics: its own time, whatever a host's looks
/// are (a node may look once an hour, or never).
pub const USAGE_EVERY: std::time::Duration = std::time::Duration::from_secs(15);

/// Writes [`usage_points`] every `every`, from now, on a thread of its own.
pub fn start_usage(o: super::Observe, host: String, every: std::time::Duration) {
    if every.is_zero() {
        return;
    }
    let spawned = std::thread::Builder::new().name("kiln-observe-usage".into()).spawn(move || {
        loop {
            std::thread::sleep(every);
            let points = usage_points(&host, crate::util::now_ms());
            if !points.is_empty() {
                o.write(points);
            }
        }
    });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "observe's log usage did not start");
    }
}

/// Records of a part kept a short while: those not kept for search are read live, counted by
/// metrics from logs (within seconds) and archived (within a cut, fifteen minutes, and the
/// archives' half minute), then let go an hour after their time.
pub const TRANSIT_MS: i64 = 3_600_000;

/// [`TRANSIT_MS`], or `KILN_OBSERVE_TRANSIT_MS` (tests').
pub fn transit_ms() -> i64 {
    static MS: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    *MS.get_or_init(|| std::env::var("KILN_OBSERVE_TRANSIT_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(TRANSIT_MS))
}

/// The cutoffs (ms) the leader states for the parts that are kept for their own time: each
/// index's of a retention of its own; one whose retention was its own and no longer is, the logs'
/// (`logs_cutoff`); those `removed` (indexes') and still held, now (their records no longer read,
/// their files let go); those not kept for search, [`transit_ms`] back. A part held that no list
/// names is kept as the logs are: only a removal lets one go.
pub fn retentions(kept: &[Kept], removed: &[u16], held: &[u16], has_cutoff: &dyn Fn(u16) -> bool, now_ms: i64, logs_cutoff: i64) -> Vec<(u16, i64)> {
    use kiln_obs::logs::store::{EVENTS, TRANSIT};
    let mut out = Vec::new();
    for k in kept {
        match k.def.num_retention_days {
            Some(days) => out.push((k.part, now_ms.saturating_sub(i64::from(days).saturating_mul(86_400_000)))),
            None if has_cutoff(k.part) => out.push((k.part, logs_cutoff)),
            None => {}
        }
    }
    for &p in held {
        if p == TRANSIT {
            out.push((TRANSIT, now_ms.saturating_sub(transit_ms())));
        } else if p != EVENTS && removed.contains(&p) && !kept.iter().any(|k| k.part == p) {
            out.push((p, now_ms));
        }
    }
    out
}

/// What a daily quota said, as kiln's event (`event.type:logs.quota`), Datadog's words.
pub fn quota_event(said: &quota::Said, now_ms: i64) -> Record {
    let (message, severity, name) = match said {
        quota::Said::Warned { name, count, limit } => (format!("[Logs] Daily quota warning for index {name}: {count} of its {limit} logs a day kept"), 13, name),
        quota::Said::Reached { name, limit } => (format!("[Logs] Daily quota reached for index {name}: {limit} logs kept today; those after are not indexed until its day turns"), 17, name),
    };
    Record {
        ts: now_ms.saturating_mul(1_000_000),
        severity,
        service: "kiln.observe".into(),
        host: crate::os::hostname(),
        source: "kiln.event".into(),
        message,
        attrs: vec![("event.type".into(), "logs.quota".into()), ("datadog_index".into(), name.clone())],
        trace_id: None,
        span_id: None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use kiln_obs::logs::store::{EVENTS, MAIN, TRANSIT};

    use super::super::ingest::{LogPart, Logs};
    use super::*;

    fn rec(service: &str, status: u8, message: &str) -> Record {
        Record { ts: 1_000, severity: status, service: service.into(), host: "h".into(), source: "app".into(), message: message.into(), ..Record::default() }
    }

    fn rows(recs: &[Record]) -> LogPart {
        let mut raw = Vec::new();
        let mut prev = 0;
        for r in recs {
            kiln_obs::logs::record::put_row(&mut raw, r, prev);
            prev = r.ts;
        }
        LogPart::Rows { raw, rows: recs.len() as u32, last_ts: prev }
    }

    /// Each run's part and its records' messages, in order.
    fn parts(logs: &Logs) -> Vec<(u16, Vec<String>)> {
        logs.parts
            .iter()
            .zip(&logs.into)
            .map(|(p, &part)| {
                let messages = match p {
                    LogPart::Records(r) => r.iter().map(|r| r.message.clone()).collect(),
                    LogPart::Rows { raw, rows, .. } => {
                        let mut it = kiln_obs::logs::record::Rows::new(raw, *rows as usize);
                        let mut out = Vec::new();
                        while let Some(r) = it.next_row().unwrap() {
                            out.push(r.message.to_owned());
                        }
                        out
                    }
                };
                (part, messages)
            })
            .collect()
    }

    fn index(name: &str, part: u16, query: &str, exclusions: Vec<ExclusionDef>) -> Kept {
        Kept { def: IndexDef { name: name.into(), filter: Filter { query: query.into() }, exclusion_filters: exclusions, ..main_def() }, part, created: 0, modified: 0 }
    }

    /// Records and rows into the first index that takes them; those its exclusion filter excludes,
    /// and those of no index, out of search; kiln's events their own; a run split where its records
    /// go apart, each part's in order; what became of them counted by index and exclusion.
    #[test]
    fn records_go_into_their_index_and_the_rest_out_of_search() {
        let debug = ExclusionDef { name: "debug".into(), is_enabled: true, filter: ExclusionQuery { query: "status:debug".into(), sample_rate: 1.0, sample_attribute: None } };
        let a = compile(&[index("audit", 1, "service:audit", Vec::new()), index("web", 2, "service:web", vec![debug])], Vec::new());
        let event = Record { source: "kiln.event".into(), ..rec("kiln", 9, "deployed") };
        let mut logs = Logs::from(vec![
            LogPart::Records(vec![rec("audit", 9, "a1"), rec("web", 5, "w-debug"), rec("web", 9, "w1"), event, rec("batch", 9, "b1")]),
            rows(&[rec("web", 9, "w2"), rec("audit", 9, "a2"), rec("web", 9, "w3")]),
        ]);
        let usage = route_logs_by(&a, &mut logs);
        assert_eq!(
            parts(&logs),
            [
                (1, vec!["a1".to_owned()]),
                (TRANSIT, vec!["w-debug".to_owned(), "b1".to_owned()]),
                (2, vec!["w1".to_owned()]),
                (EVENTS, vec!["deployed".to_owned()]),
                (2, vec!["w2".to_owned(), "w3".to_owned()]),
                (1, vec!["a2".to_owned()]),
            ]
        );
        let count = |part: Option<u16>, excluded: bool| usage.rows.iter().find(|r| r.part == part && r.excluded == excluded).map_or(0, |r| r.records);
        assert_eq!((count(Some(1), false), count(Some(2), false), count(Some(2), true), count(None, false)), (2, 3, 1, 1), "the event no index's");
    }

    /// `main` as it is made: every record into the main part, rows whole and unread, kiln's events
    /// apart; an index over its quota, its records out of search but counted its own.
    #[test]
    fn main_alone_takes_all_and_an_index_over_its_quota_none() {
        let a = compile(&Saved::default().indexes, Vec::new());
        assert_eq!(a.all_into, Some(MAIN));
        let part = rows(&[rec("web", 9, "w1"), rec("web", 9, "w2")]);
        let LogPart::Rows { raw: before, .. } = &part else { unreachable_part() };
        let before = before.as_ptr();
        let event = Record { source: "kiln.event".into(), ..rec("kiln", 9, "deployed") };
        let mut logs = Logs::from(vec![part, LogPart::Records(vec![rec("api", 9, "r1"), event])]);
        let usage = route_logs_by(&a, &mut logs);
        assert_eq!(parts(&logs), [(MAIN, vec!["w1".to_owned(), "w2".to_owned()]), (MAIN, vec!["r1".to_owned()]), (EVENTS, vec!["deployed".to_owned()])]);
        let LogPart::Rows { raw, .. } = &logs.parts[0] else { unreachable_part() };
        assert_eq!(raw.as_ptr(), before, "rows taken whole");
        assert_eq!(usage.rows, [UsageRow { part: Some(MAIN), excluded: false, records: 3, bytes: usage.rows[0].bytes }]);
        let limited = compile(&Saved::default().indexes, vec![MAIN]);
        let mut logs = Logs::from(vec![rows(&[rec("web", 9, "w1")])]);
        let usage = route_logs_by(&limited, &mut logs);
        assert_eq!(parts(&logs), [(TRANSIT, vec!["w1".to_owned()])]);
        assert_eq!((usage.rows[0].part, usage.rows[0].excluded), (Some(MAIN), false), "Datadog counts it its index's");
    }

    fn unreachable_part() -> ! {
        panic!("a run of rows")
    }

    /// A run of rows split as its records go: each part's rows the bytes its records are written
    /// as alone (`put_row`, each time chained from the part's row before), in order, with their
    /// count and last time; the parts in the order they were first met.
    #[test]
    fn a_split_run_of_rows_is_each_part_s_rows_as_written() {
        let recs: Vec<Record> = (0u8..40)
            .map(|i| Record {
                ts: 1_700_000_000_000_000_000 + i64::from(i) * 1_000_003,
                attrs: vec![("k".into(), format!("v{i}"))],
                trace_id: i.is_multiple_of(3).then_some([i; 16]),
                ..rec(["web", "audit", "batch"][usize::from(i % 3)], 9, &format!("line {i}"))
            })
            .collect();
        let a = compile(&[index("audit", 1, "service:audit", Vec::new()), index("web", 2, "service:web", Vec::new())], Vec::new());
        let mut logs = Logs::from(vec![rows(&recs)]);
        route_logs_by(&a, &mut logs);
        let want = |part: u16, of: &str| {
            let (mut raw, mut prev, mut n) = (Vec::new(), 0, 0);
            for r in recs.iter().filter(|r| r.service == of) {
                kiln_obs::logs::record::put_row(&mut raw, r, prev);
                (prev, n) = (r.ts, n + 1);
            }
            (part, raw, n, prev)
        };
        let got: Vec<(u16, Vec<u8>, u32, i64)> = logs
            .parts
            .iter()
            .zip(&logs.into)
            .map(|(p, &part)| match p {
                LogPart::Rows { raw, rows, last_ts } => (part, raw.clone(), *rows, *last_ts),
                LogPart::Records(_) => unreachable_part(),
            })
            .collect();
        assert_eq!(got, [want(2, "web"), want(1, "audit"), want(TRANSIT, "batch")]);
    }

    /// What a collector's rows come to, in bytes, is the same whether `main` alone takes them
    /// whole or indexes read and split them: their bytes as written.
    #[test]
    fn rows_count_the_same_bytes_however_many_indexes() {
        let recs = [rec("web", 9, "GET /a"), rec("audit", 9, "login ok"), rec("web", 5, "debug detail"), rec("batch", 9, "done")];
        let LogPart::Rows { raw, .. } = rows(&recs) else { unreachable_part() };
        let bytes = |a: &Active| {
            let u = route_logs_by(a, &mut Logs::from(vec![rows(&recs)]));
            (u.rows.iter().map(|r| r.records).sum::<u64>(), u.rows.iter().map(|r| r.bytes).sum::<u64>())
        };
        let debug = ExclusionDef { name: "debug".into(), is_enabled: true, filter: ExclusionQuery { query: "status:debug".into(), sample_rate: 1.0, sample_attribute: None } };
        let three = compile(&[index("audit", 1, "service:audit", Vec::new()), index("web", 2, "service:web", vec![debug])], Vec::new());
        assert_eq!(bytes(&compile(&Saved::default().indexes, Vec::new())), (4, raw.len() as u64));
        assert_eq!(bytes(&three), (4, raw.len() as u64));
    }

    /// The leader's cutoffs: an index of its own retention its days back; one whose retention is
    /// gone, the logs'; a removed index's part held, now; those not kept for search, an hour back;
    /// kiln's events, never (the logs'); a part held that no list names (a list not heard yet, or
    /// one that did not read), never.
    #[test]
    fn each_part_is_kept_its_own_time() {
        let now = 1_000_000_000_000;
        let audit = Kept { def: IndexDef { num_retention_days: Some(30), ..main_def() }, part: 1, created: 0, modified: 0 };
        let web = index("web", 2, "*", Vec::new());
        let kept = vec![audit, web];
        let held = [MAIN, 1, 2, 3, 4, EVENTS, TRANSIT];
        let got = retentions(&kept, &[MAIN, 3], &held, &|p| p == 2, now, now - 15 * 86_400_000);
        assert_eq!(got, [(1, now - 30 * 86_400_000), (2, now - 15 * 86_400_000), (MAIN, now), (3, now), (TRANSIT, now - transit_ms())]);
        assert_eq!(retentions(&[], &[], &held, &|_| false, now, 0), [(TRANSIT, now - transit_ms())], "none heard: nothing let go");
    }

    /// Kept in a file: a removed index's name never used again and its part let go; `main` made
    /// again of its own part; a part the store holds never given (a list that did not read, kept
    /// aside); a list that does not read, `main` alone.
    #[test]
    fn removals_are_kept_and_a_list_that_does_not_read_lets_nothing_go() {
        let dir = std::env::temp_dir().join(format!("kiln-indexes-{}-{}", std::process::id(), crate::util::now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let ix = Indexes::open(&dir);
        let none = |_: u16| false;
        let web = ix.create(IndexDef { name: "web".into(), ..main_def() }, &none).unwrap();
        assert_eq!(web.part, 1);
        assert!(ix.delete("web").unwrap() && ix.delete(MAIN_NAME).unwrap());
        assert_eq!(ix.removed_parts(), [1, MAIN]);
        assert!(ix.create(IndexDef { name: "web".into(), ..main_def() }, &none).is_err(), "a removed name");
        assert_eq!(ix.create(main_def(), &none).unwrap().part, MAIN);
        assert_eq!(ix.removed_parts(), [1], "main's part read again");
        assert_eq!(ix.create(IndexDef { name: "api".into(), ..main_def() }, &|p| p == 2 || p == 3).unwrap().part, 4, "parts the store holds passed over");
        let again = Indexes::open(&dir);
        assert_eq!((again.order(), again.removed_parts()), (vec![MAIN_NAME.to_owned(), "api".to_owned()], vec![1]));
        std::fs::write(dir.join("indexes.json"), b"{ not json").unwrap();
        let unread = Indexes::open(&dir);
        assert_eq!((unread.order(), unread.removed_parts()), (vec![MAIN_NAME.to_owned()], vec![]));
        let aside = std::fs::read_dir(&dir).unwrap().filter_map(Result::ok).filter(|e| e.file_name().to_string_lossy().starts_with("indexes.json.unread-")).count();
        assert_eq!(aside, 1, "the list that did not read kept aside");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
