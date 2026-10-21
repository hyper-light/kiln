//! Rehydration from archives (`docs/observe.md` §10.6), Datadog's "rehydrate from archives": an
//! archive's records of a time range that a search takes, read back from its store into a view of
//! their own (Datadog's historical view), searched, counted and faceted as the live logs are by
//! naming the view (`view` in a logs query), kept for its retention, then dropped. What is read is
//! Datadog's archive format (`kiln_obs::logs::archive`), so an archive Datadog wrote reads alike.
//!
//! - A rehydration lists its range's folders (`<path>/dt=…/` for a whole day, `…/hour=…/` for
//!   an hour) and reads their objects in their names' order, each gunzip'd and its events read as
//!   records as it comes; those of its range that its search takes are kept, with the archive's
//!   rehydration tags. It stops at its scan's bound (bytes of objects read: its own, else the
//!   archive's `rehydration_max_scan_size_in_gb`), and says so.
//! - One at a time, in the order asked; a view is searched as it fills.
//! - A view is a store of its own on the server (`<observe>/rehydrated/<id>`), beside the
//!   replicated one: nothing of it goes to the group, and it goes whole when it expires or is
//!   removed. A rehydration the server stopped in is begun again when it starts, its view made anew.

use std::collections::{BTreeSet, HashMap};
use std::io::{BufRead as _, Read as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant};

use kiln_obs::logs::archive;
use kiln_obs::logs::record::Record;
use kiln_obs::machine::Shared;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::archives::{Archive, Archives, Refusal};

/// A view's retention unless said (Datadog's historical views'), and the most it may be.
const RETENTION_DAYS: u32 = 15;
const RETENTION_MAX_DAYS: u32 = 180;
/// Views kept at once at the most.
const VIEWS: usize = 32;
/// A rehydration's range at the most: a year and a month's hours.
const RANGE_MAX_HOURS: i64 = 400 * 24;
/// Records a write to a view holds at the most, and the bytes of writes between its cuts: what
/// a cut has not taken waits in memory, as much as the live store's does (it is cut each half
/// cadence of its log, `kiln_obs::group::due`).
const BATCH: usize = 2_000;
const CUT_BYTES: u64 = kiln_obs::group::CADENCE_BYTES / 2;
/// Chunks of an object read waiting for its reader at the most.
const CHUNKS: usize = 16;
/// Bytes an object unzips to at the most: an archive's object holds 100 MB of events
/// (`kiln_obs::logs::archive::PART_BYTES`); one that unzips to ten times that is refused.
const UNZIPPED_MAX: u64 = 1 << 30;
/// Objects of a rehydration at the most.
const OBJECTS_MAX: usize = 1_000_000;
/// Attributes a record reads at the most, and the depth of the objects they are nested in
/// (`kiln_obs::logs::archive` nests 16 deep).
const ATTRS: usize = 256;
const DEPTH: usize = 16;
/// How often a rehydration's progress is written down.
const SAVE_EVERY: Duration = Duration::from_secs(2);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A rehydration as it is asked.
#[derive(Debug, Clone, Deserialize)]
pub struct RehydrationIn {
    pub name: String,
    pub archive_id: String,
    /// A log search: the records kept. Every record unless said.
    #[serde(default)]
    pub query: Option<String>,
    /// Milliseconds since the epoch: the records' times kept, `[from, to)`.
    pub from: i64,
    pub to: i64,
    #[serde(default)]
    pub retention_days: Option<u32>,
    /// Bytes of objects read at the most, in GB (10^9): the archive's
    /// `rehydration_max_scan_size_in_gb` bounds it too.
    #[serde(default)]
    pub max_scan_size_in_gb: Option<f64>,
}

/// A rehydration: what it was asked, and how its reading goes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rehydration {
    pub id: String,
    pub name: String,
    pub archive_id: String,
    /// The archive's name when it was asked.
    pub archive: String,
    pub query: String,
    pub from: i64,
    pub to: i64,
    pub retention_days: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_scan_bytes: Option<u64>,
    pub created: i64,
    /// When its view goes (ms): its retention from when it was read.
    pub expires: i64,
    /// `queued`, `reading`, `done` or `failed`.
    pub state: String,
    #[serde(default)]
    pub error: Option<String>,
    /// Objects of its range, and those read so far, their bytes.
    #[serde(default)]
    pub objects: u64,
    #[serde(default)]
    pub objects_read: u64,
    #[serde(default)]
    pub bytes_read: u64,
    /// Events read, records kept, and lines that were no event.
    #[serde(default)]
    pub events_read: u64,
    #[serde(default)]
    pub records: u64,
    #[serde(default)]
    pub malformed: u64,
    /// Whether its scan's bound stopped it before its range's last object.
    #[serde(default)]
    pub limited: bool,
    #[serde(default)]
    pub started: Option<i64>,
    #[serde(default)]
    pub finished: Option<i64>,
}

#[derive(Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    rehydrations: Vec<Rehydration>,
}

/// The rehydrations, kept in `<observe>/rehydrations.json`, their views in
/// `<observe>/rehydrated/<id>`.
pub struct Rehydrations {
    path: PathBuf,
    dir: PathBuf,
    all: Mutex<Vec<Rehydration>>,
    views: Mutex<HashMap<String, Shared>>,
    /// Rehydrations removed while they are read: their reading stops.
    removed: Mutex<BTreeSet<String>>,
    archives: Arc<Archives>,
}

impl Rehydrations {
    /// The rehydrations kept in `observe`: each view read opened, each not read to its end begun
    /// again.
    pub fn open(observe: &Path, archives: Arc<Archives>) -> Arc<Self> {
        let path = observe.join("rehydrations.json");
        let dir = observe.join("rehydrated");
        let saved: Saved = super::archives::read_or_aside(&path);
        let mut views = HashMap::new();
        let mut all = saved.rehydrations;
        for r in &mut all {
            match r.state.as_str() {
                "done" => match open_view(&dir.join(&r.id)) {
                    Ok(v) => {
                        views.insert(r.id.clone(), v);
                    }
                    Err(e) => (r.state, r.error) = ("failed".into(), Some(format!("its view could not be opened: {e}"))),
                },
                "failed" => {}
                _ => {
                    // Stopped as it read: begun again, its view made anew.
                    r.state = "queued".into();
                }
            }
        }
        // Views no rehydration names (one removed as the server stopped): gone.
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if !all.iter().any(|r| r.id == name) {
                    let _ = std::fs::remove_dir_all(e.path());
                }
            }
        }
        let me = Arc::new(Self { path, dir, all: Mutex::new(all), views: Mutex::new(views), removed: Mutex::new(BTreeSet::new()), archives });
        me.saved();
        me
    }

    fn save(&self, all: &[Rehydration]) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(&serde_json::json!({ "rehydrations": all })).map_err(|e| e.to_string())?;
        crate::sync::group::save_atomic(&self.path, &bytes).map_err(|e| format!("{}: {e}", self.path.display()))
    }

    fn saved(&self) {
        let all = lock(&self.all).clone();
        if let Err(e) = self.save(&all) {
            tracing::warn!(error = %e, "observe's rehydrations could not be written down");
        }
    }

    pub fn list(&self) -> Vec<Rehydration> {
        lock(&self.all).clone()
    }

    pub fn get(&self, id: &str) -> Option<Rehydration> {
        lock(&self.all).iter().find(|r| r.id == id).cloned()
    }

    /// A rehydration's view: its store, searched as the live logs are.
    pub fn view(&self, id: &str) -> Option<Shared> {
        lock(&self.views).get(id).cloned()
    }

    /// A rehydration asked: checked, queued.
    pub fn create(&self, r: RehydrationIn) -> Result<Rehydration, Refusal> {
        let name = r.name.trim().to_owned();
        if name.is_empty() || name.len() > 100 {
            return Err((400, "a name, 100 characters at the most".into()));
        }
        let (archive, _) = self.archives.get(&r.archive_id).ok_or_else(|| (400, format!("archive_id: no archive {:?}", r.archive_id)))?;
        let query = r.query.as_deref().map(str::trim).filter(|q| !q.is_empty()).unwrap_or("*").to_owned();
        kiln_obs::logs::search::parse(&query).map_err(|e| (400, format!("its query: {e}")))?;
        if r.from < 0 || r.to <= r.from {
            return Err((400, "from and to: milliseconds since the epoch, from before to".into()));
        }
        if (r.to - r.from) / 3_600_000 > RANGE_MAX_HOURS {
            return Err((400, format!("a range of {} days at the most", RANGE_MAX_HOURS / 24)));
        }
        let retention_days = r.retention_days.unwrap_or(RETENTION_DAYS);
        if !(1..=RETENTION_MAX_DAYS).contains(&retention_days) {
            return Err((400, format!("retention_days: 1 to {RETENTION_MAX_DAYS}")));
        }
        let gb = |g: f64| if g.is_finite() && g > 0.0 { Ok((g * 1e9) as u64) } else { Err((400, "a scan's bound: more than 0 GB".to_owned())) };
        let own = r.max_scan_size_in_gb.map(gb).transpose()?;
        let archive_s = archive.rehydration_max_scan_size_in_gb.map(|g| g.saturating_mul(1_000_000_000));
        let max_scan_bytes = match (own, archive_s) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let now = crate::util::now_ms();
        let mut all = lock(&self.all);
        if all.len() >= VIEWS {
            return Err((400, format!("{VIEWS} rehydrations at once at the most: remove one")));
        }
        if all.iter().any(|x| x.name == name) {
            return Err((409, format!("a rehydration named {name:?} already")));
        }
        let made = Rehydration {
            id: super::fresh_id("r", |id| all.iter().any(|x| x.id == id)),
            name,
            archive_id: archive.id.clone(),
            archive: archive.name.clone(),
            query,
            from: r.from,
            to: r.to,
            retention_days,
            max_scan_bytes,
            created: now,
            expires: now.saturating_add(i64::from(retention_days) * 86_400_000),
            state: "queued".into(),
            error: None,
            objects: 0,
            objects_read: 0,
            bytes_read: 0,
            events_read: 0,
            records: 0,
            malformed: 0,
            limited: false,
            started: None,
            finished: None,
        };
        all.push(made.clone());
        self.save(&all).map_err(|e| (500, e))?;
        Ok(made)
    }

    /// A rehydration removed, and its view with it (its reading stopped, if it is read).
    pub fn delete(&self, id: &str) -> Result<(), Refusal> {
        let mut all = lock(&self.all);
        let Some(at) = all.iter().position(|r| r.id == id) else { return Err((404, "no such rehydration".into())) };
        let r = all.remove(at);
        self.save(&all).map_err(|e| (500, e))?;
        drop(all);
        lock(&self.views).remove(id);
        if r.state == "reading" {
            // Its reading stops at its next object, and its view goes then.
            lock(&self.removed).insert(id.to_owned());
        } else {
            let _ = std::fs::remove_dir_all(self.dir.join(id));
        }
        Ok(())
    }

    /// `f` of rehydration `id` (if it is still kept), written down when `save`.
    fn set(&self, id: &str, save: bool, f: impl FnOnce(&mut Rehydration)) {
        let mut all = lock(&self.all);
        if let Some(r) = all.iter_mut().find(|r| r.id == id) {
            f(r);
        }
        if save && let Err(e) = self.save(&all) {
            tracing::warn!(error = %e, "observe's rehydrations could not be written down");
        }
    }

    fn removed(&self, id: &str) -> bool {
        lock(&self.removed).contains(id)
    }

    /// Views past their retention: gone.
    fn expire(&self, now: i64) {
        let gone: Vec<String> = {
            let mut all = lock(&self.all);
            let gone: Vec<String> = all.iter().filter(|r| matches!(r.state.as_str(), "done" | "failed") && r.expires <= now).map(|r| r.id.clone()).collect();
            if gone.is_empty() {
                return;
            }
            all.retain(|r| !gone.contains(&r.id));
            if let Err(e) = self.save(&all) {
                tracing::warn!(error = %e, "observe's rehydrations could not be written down");
            }
            gone
        };
        for id in gone {
            lock(&self.views).remove(&id);
            let _ = std::fs::remove_dir_all(self.dir.join(&id));
            tracing::info!(rehydration = %id, "a rehydration's view expired: removed");
        }
    }

    /// Reads each rehydration queued, in the order asked, and lets views go as they expire, until
    /// the server stops.
    pub async fn run(self: Arc<Self>, shutdown: crate::rt::Shutdown) {
        while shutdown.sleep(Duration::from_secs(1)).await {
            self.expire(crate::util::now_ms());
            let next = lock(&self.all).iter().find(|r| r.state == "queued").map(|r| r.id.clone());
            if let Some(id) = next {
                self.rehydrate(&id).await;
            }
        }
    }

    async fn rehydrate(&self, id: &str) {
        let now = crate::util::now_ms();
        self.set(id, true, |r| {
            (r.state, r.error, r.started, r.finished) = ("reading".into(), None, Some(now), None);
            (r.objects, r.objects_read, r.bytes_read, r.events_read, r.records, r.malformed, r.limited) = (0, 0, 0, 0, 0, 0, false);
        });
        let Some(r) = self.get(id) else { return };
        let outcome = self.read(&r).await;
        let removed = lock(&self.removed).remove(id);
        if removed {
            lock(&self.views).remove(id);
            let _ = std::fs::remove_dir_all(self.dir.join(id));
            return;
        }
        let now = crate::util::now_ms();
        match outcome {
            Ok(limited) => {
                tracing::info!(rehydration = %r.name, limited, "a rehydration was read");
                self.set(id, true, |r| (r.state, r.limited, r.finished, r.expires) = ("done".into(), limited, Some(now), now.saturating_add(i64::from(r.retention_days) * 86_400_000)));
            }
            Err(e) => {
                // What it read goes: a view is a rehydration's whole range or none.
                tracing::warn!(rehydration = %r.name, error = %e, "a rehydration failed");
                lock(&self.views).remove(id);
                let _ = std::fs::remove_dir_all(self.dir.join(id));
                self.set(id, true, |r| (r.state, r.error, r.finished, r.expires) = ("failed".into(), Some(e), Some(now), now.saturating_add(i64::from(r.retention_days) * 86_400_000)));
            }
        }
    }

    /// Rehydration `r` read into a view made anew: whether its scan's bound stopped it.
    async fn read(&self, r: &Rehydration) -> Result<bool, String> {
        let (a, _) = self.archives.get(&r.archive_id).ok_or_else(|| "its archive was removed".to_owned())?;
        let search = kiln_obs::logs::search::parse(&r.query).map_err(|e| format!("its query: {e}"))?;
        let tags: Vec<(String, String)> = a.rehydration_tags.iter().filter_map(|t| tag(t)).collect();
        let dir = self.dir.join(&r.id);
        let _ = std::fs::remove_dir_all(&dir);
        let store = open_view(&dir)?;
        lock(&self.views).insert(r.id.clone(), Arc::clone(&store));
        let objects = self.objects(&a, r).await?;
        let total = objects.len() as u64;
        self.set(&r.id, true, |x| x.objects = total);
        let filter = Arc::new(Filter { search, from: r.from.saturating_mul(1_000_000), to: r.to.saturating_mul(1_000_000), tags });
        let mut writer = ViewWriter { store, builder: kiln_obs::batch::Builder::new(), point: 0, since_cut: 0 };
        let (mut bytes, mut limited) = (0u64, false);
        let mut saved = Instant::now();
        for (key, size) in objects {
            if self.removed(&r.id) {
                return Err("removed".into());
            }
            if r.max_scan_bytes.is_some_and(|max| bytes.saturating_add(size) > max) {
                limited = true;
                break;
            }
            // Read as it comes: its bytes to the reader on the blocking pool, the channel's room all
            // that waits. Each side stops when the other does.
            let (tx, rx) = async_channel::bounded(CHUNKS);
            let f = Arc::clone(&filter);
            let decoding = crate::rt::blocking(move || {
                let counts = read_object(Coming { rx, chunk: bytes::Bytes::new() }, &f, &mut writer, UNZIPPED_MAX);
                (writer, counts)
            });
            let reading = async {
                let read = self.archives.stream_object(&a, &key, &tx).await;
                drop(tx);
                read
            };
            let (decoded, read) = futures::future::join(decoding, reading).await;
            let (back, counts) = decoded.map_err(|e| format!("{e:#}"))?;
            writer = back;
            // A read cut short ends its reader's too: the read's error is what happened.
            bytes = bytes.saturating_add(read.map_err(|e| format!("{key}: {e}"))?);
            let c = counts.map_err(|e| format!("{key}: {e}"))?;
            let save = saved.elapsed() >= SAVE_EVERY;
            if save {
                saved = Instant::now();
            }
            self.set(&r.id, save, |x| {
                (x.objects_read, x.bytes_read) = (x.objects_read + 1, bytes);
                (x.events_read, x.records, x.malformed) = (x.events_read + c.events, x.records + c.kept, x.malformed + c.malformed);
            });
        }
        crate::rt::blocking(move || writer.cut()).await.map_err(|e| format!("{e:#}"))??;
        Ok(limited)
    }

    /// The objects of `r`'s range's folders ([`folders`]), in their names' order (a folder's objects
    /// are its hour's; their names begin with their newest event's time).
    async fn objects(&self, a: &Archive, r: &Rehydration) -> Result<Vec<(String, u64)>, String> {
        let mut out = Vec::new();
        for folder in folders(a.destination.path(), r.from, r.to) {
            if self.removed(&r.id) {
                return Err("removed".into());
            }
            out.extend(self.archives.list_objects(a, &folder).await.map_err(|e| format!("{folder}: {e}"))?);
            if out.len() > OBJECTS_MAX {
                return Err(format!("more than {OBJECTS_MAX} objects in its range: a narrower one"));
            }
        }
        out.sort();
        out.dedup();
        Ok(out)
    }
}

/// The folders of the hours of `[from, to)` (ms): a day's whole (one listing for its hours) where
/// the range holds all of it, its hours' otherwise.
fn folders(path: &str, from: i64, to: i64) -> Vec<String> {
    let first = archive::hour_of(from.saturating_mul(1_000_000));
    let last = archive::hour_of(to.saturating_mul(1_000_000).saturating_sub(1));
    let mut out = Vec::new();
    let mut hour = first;
    while hour <= last {
        let day = hour.div_euclid(24);
        if hour.rem_euclid(24) == 0 && hour + 23 <= last {
            out.push(archive::day_folder(path, day));
            hour += 24;
        } else {
            out.push(archive::folder(path, hour));
            hour += 1;
        }
    }
    out
}

/// An object's bytes as they come (its read's chunks), read as a file's: their end where its read
/// ends.
struct Coming {
    rx: async_channel::Receiver<bytes::Bytes>,
    chunk: bytes::Bytes,
}

impl std::io::Read for Coming {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while self.chunk.is_empty() {
            match self.rx.recv_blocking() {
                Ok(c) => self.chunk = c,
                Err(_) => return Ok(0),
            }
        }
        let n = buf.len().min(self.chunk.len());
        let head = self.chunk.split_to(n);
        if let Some(to) = buf.get_mut(..n) {
            to.copy_from_slice(&head);
        }
        Ok(n)
    }
}

/// A view's store at `dir`, made if it is not there.
fn open_view(dir: &Path) -> Result<Shared, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let store = kiln_obs::store::Store::open(dir, kiln_obs::hash::Keyed::new()).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(Arc::new(RwLock::new(store)))
}

/// `key:value`, or a bare tag (its value empty).
fn tag(t: &str) -> Option<(String, String)> {
    let t = t.trim();
    if t.is_empty() {
        return None;
    }
    Some(t.split_once(':').map_or_else(|| (t.to_owned(), String::new()), |(k, v)| (k.to_owned(), v.to_owned())))
}

/// What a rehydration keeps: its search, its range (ns, `[from, to)`) and the tags it adds.
struct Filter {
    search: kiln_obs::logs::search::Search,
    from: i64,
    to: i64,
    tags: Vec<(String, String)>,
}

/// What an object held: events, records kept, lines that were no event.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Counts {
    events: u64,
    kept: u64,
    malformed: u64,
}

/// What writes a view: its store, a batch built, where its last cut was, and the bytes written
/// since.
struct ViewWriter {
    store: Shared,
    builder: kiln_obs::batch::Builder,
    point: u64,
    since_cut: u64,
}

impl ViewWriter {
    fn write(&mut self, records: &[Record]) -> Result<(), String> {
        if records.is_empty() {
            return Ok(());
        }
        self.builder.logs(records);
        let batch = self.builder.finish();
        self.store.write().unwrap_or_else(PoisonError::into_inner).apply(&batch).map_err(|e| format!("its view: {e}"))?;
        self.since_cut += batch.len() as u64;
        if self.since_cut >= CUT_BYTES {
            self.cut()?;
        }
        Ok(())
    }

    /// The view's records written into its files.
    fn cut(&mut self) -> Result<(), String> {
        self.point += 1;
        self.store.write().unwrap_or_else(PoisonError::into_inner).cut(kiln_obs::manifest::Point { index: self.point, term: 1 }).map_err(|e| format!("its view: {e}"))?;
        self.since_cut = 0;
        Ok(())
    }
}

/// An object's events (gzip'd JSON lines, one an event: Datadog's archive format), those `f` keeps
/// written to `w`; one that unzips past `max` bytes refused.
fn read_object(body: impl std::io::Read, f: &Filter, w: &mut ViewWriter, max: u64) -> Result<Counts, String> {
    let mut c = Counts::default();
    let gz = flate2::read::MultiGzDecoder::new(body);
    let mut lines = std::io::BufReader::with_capacity(256 << 10, gz.take(max.saturating_add(1)));
    let (mut line, mut unzipped) = (Vec::new(), 0u64);
    let mut rs = kiln_obs::logs::search::RowScratch::default();
    let mut batch: Vec<Record> = Vec::with_capacity(BATCH);
    loop {
        line.clear();
        let n = lines.read_until(b'\n', &mut line).map_err(|e| format!("not gzip'd JSON lines: {e}"))?;
        if n == 0 {
            break;
        }
        unzipped += n as u64;
        if unzipped > max {
            return Err(format!("it unzips past {max} bytes"));
        }
        let text = line.trim_ascii();
        if text.is_empty() {
            continue;
        }
        c.events += 1;
        let Some(mut r) = record_of(text) else {
            c.malformed += 1;
            continue;
        };
        if r.ts < f.from || r.ts >= f.to || !f.search.record(&r, &mut rs) {
            continue;
        }
        for (k, v) in &f.tags {
            if !r.attrs.iter().any(|(a, _)| a == k) {
                r.attrs.push((k.clone(), v.clone()));
            }
        }
        batch.push(r);
        c.kept += 1;
        if batch.len() >= BATCH {
            w.write(&batch)?;
            batch.clear();
        }
    }
    w.write(&batch)?;
    Ok(c)
}

/// An archived event (Datadog's: `date`, `host`, `source`, `service`, `status`, `message`, `tags`,
/// `trace_id`, `span_id`, and the rest under `attributes`, nested) as a record: `date` RFC 3339 or
/// milliseconds; a tag `key:value` an attribute where none of that key is (service, host and
/// source are the record's own); ids hex, or a decimal (a Datadog tracer's 64-bit ids). None for a
/// line that is no event.
pub(crate) fn record_of(line: &[u8]) -> Option<Record> {
    let Value::Object(mut o) = serde_json::from_slice::<Value>(line).ok()? else { return None };
    let ts = match o.get("date")? {
        Value::String(s) => super::cloud::parse_time_ns(s)?,
        Value::Number(n) => n.as_i64()?.checked_mul(1_000_000)?,
        _ => return None,
    };
    let mut take = |k: &str| match o.remove(k) {
        Some(Value::String(s)) => s,
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    };
    let (service, host, source, message, status) = (take("service"), take("host"), take("source"), take("message"), take("status"));
    let severity = kiln_obs::logs::record::severities(&status).map_or(if status.is_empty() { 9 } else { 0 }, |r| *r.start());
    let mut attrs = Vec::new();
    if let Some(a) = o.remove("attributes") {
        flatten(None, a, 0, &mut attrs);
    }
    if let Some(Value::Array(tags)) = o.remove("tags") {
        for t in tags {
            let Value::String(t) = t else { continue };
            let Some((k, v)) = tag(&t) else { continue };
            if attrs.len() < ATTRS && !matches!(k.as_str(), "service" | "host" | "source") && !attrs.iter().any(|(a, _)| *a == k) {
                attrs.push((k, v));
            }
        }
    }
    let trace_id = o.get("trace_id").and_then(|v| id_of(v, 16)).map(|b| {
        let mut t = [0u8; 16];
        for (d, s) in t.iter_mut().rev().zip(b.iter().rev()) {
            *d = *s;
        }
        t
    });
    let span_id = o.get("span_id").and_then(|v| id_of(v, 8)).map(|b| {
        let mut s = [0u8; 8];
        for (d, x) in s.iter_mut().rev().zip(b.iter().rev()) {
            *d = *x;
        }
        s
    });
    Some(Record { ts, severity, service, host, source, message, attrs, trace_id, span_id })
}

/// An id's bytes (`len` at the most): hex of `len` bytes or fewer, or a decimal (a 64-bit id).
fn id_of(v: &Value, len: usize) -> Option<Vec<u8>> {
    let s = match v {
        Value::String(s) => s.trim().to_owned(),
        Value::Number(n) => n.as_u64()?.to_string(),
        _ => return None,
    };
    if s.is_empty() || s.len() > 2 * len {
        return None;
    }
    // Hex of a whole id's width (or a tracer's 64-bit one in 128): hex; digits alone: a decimal.
    let hex_width = s.len() == 2 * len || s.len() == 16;
    if !hex_width && s.bytes().all(|b| b.is_ascii_digit()) {
        return s.parse::<u64>().ok().map(|n| n.to_be_bytes().to_vec());
    }
    if s.len() % 2 != 0 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    hex::decode(&s).ok()
}

/// `v`'s values, their keys their paths joined by dots ([`DEPTH`] deep: deeper, the rest as
/// JSON), a string as it is and any other value as JSON; [`ATTRS`] at the most.
fn flatten(prefix: Option<&str>, v: Value, depth: usize, out: &mut Vec<(String, String)>) {
    if out.len() >= ATTRS {
        return;
    }
    match v {
        Value::Object(o) if depth < DEPTH => {
            for (k, v) in o {
                let key = match prefix {
                    Some(p) => format!("{p}.{k}"),
                    None => k,
                };
                flatten(Some(&key), v, depth + 1, out);
            }
        }
        Value::Null => {}
        Value::String(s) => {
            if let Some(p) = prefix {
                out.push((p.to_owned(), s));
            }
        }
        other => {
            if let Some(p) = prefix {
                out.push((p.to_owned(), other.to_string()));
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// Records written as archived events (kiln's writer) read back as themselves, every field and
    /// attribute: nested, flat where they must be, every character; tags not doubled.
    #[test]
    fn an_archived_event_reads_back_as_its_record() {
        let mut rng = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let parts = ["a", "b", "http", "status_code", "x-y", "été", "q\"z", "a\\b"];
        for i in 0..2_000u64 {
            let n = next();
            let mut attrs: Vec<(String, String)> = Vec::new();
            for j in 0..(n % 9) {
                let m = next();
                let k: Vec<&str> = (0..1 + m % 3).map(|d| parts[((m >> (8 * d)) % parts.len() as u64) as usize]).collect();
                let k = k.join(".");
                if !attrs.iter().any(|(a, _)| *a == k) {
                    attrs.push((k, format!("v{j}\n\"{}\"", m % 100)));
                }
            }
            let r = Record {
                ts: 1_800_000_000_000_000_000 + (n % 1_000_000_000) as i64 * 1_000_000,
                severity: [9, 13, 17, 21, 5, 1][(n % 6) as usize],
                service: format!("svc-{}", n % 5),
                host: if n % 7 == 0 { String::new() } else { format!("h{}", n % 3) },
                source: "app".into(),
                message: format!("message {i} \u{1}\t\"quoted\" ☃"),
                attrs,
                trace_id: (n % 2 == 0).then_some([((n >> 8) % 256) as u8; 16]),
                span_id: (n % 3 == 0).then_some([((n >> 16) % 256) as u8; 8]),
            };
            let mut raw = Vec::new();
            kiln_obs::logs::record::put_row(&mut raw, &r, 0);
            let row = kiln_obs::logs::record::Rows::new(&raw, 1).next_row().unwrap().unwrap();
            let mut line = Vec::new();
            archive::event(&row, "id", n % 4 == 0, &mut line, &mut archive::Scratch::default());
            let back = record_of(&line);
            assert!(back.is_some(), "{}", String::from_utf8_lossy(&line));
            let back = back.unwrap();
            let level = |s: u8| kiln_obs::logs::record::severities(kiln_obs::logs::record::level(s)).map_or(0, |r| *r.start());
            assert_eq!((back.ts, back.severity, &back.service, &back.host, &back.source, &back.message), (r.ts, level(r.severity), &r.service, &r.host, &r.source, &r.message));
            assert_eq!((back.trace_id, back.span_id), (r.trace_id, r.span_id));
            // Its attributes as they were; tags (when written) are its env's and version's, already there.
            let mut want = r.attrs.clone();
            let mut got = back.attrs.clone();
            want.sort();
            got.sort();
            assert_eq!(got, want, "{}", String::from_utf8_lossy(&line));
        }
    }

    /// An event as Datadog's own archives hold one: tags as attributes, numbers as text, a
    /// tracer's decimal trace id, a status of Datadog's names; what is no event is none.
    #[test]
    fn datadog_s_own_archived_events_read() {
        let line = br#"{"_id":"AAABc2dXYZ","date":"2020-08-17T15:31:24.507Z","host":"i-0abc","source":"python","service":"web","status":"warn","message":"Request took 1.2s","tags":["env:prod","service:web","team:core","version:1.4"],"trace_id":"1232391219124912494","span_id":"9876543210","attributes":{"http":{"status_code":502,"method":"GET"},"duration":1200000000,"usr":{"id":"u-1"},"ok":false}}"#;
        let r = record_of(line).unwrap();
        assert_eq!(r.ts, 1_597_678_284_507_000_000);
        assert_eq!((r.severity, r.service.as_str(), r.host.as_str(), r.source.as_str()), (13, "web", "i-0abc", "python"));
        let attr = |k: &str| r.attrs.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str());
        assert_eq!((attr("http.status_code"), attr("http.method"), attr("duration"), attr("usr.id"), attr("ok")), (Some("502"), Some("GET"), Some("1200000000"), Some("u-1"), Some("false")));
        assert_eq!((attr("env"), attr("team"), attr("version"), attr("service")), (Some("prod"), Some("core"), Some("1.4"), None));
        let mut t = [0u8; 16];
        t[8..].copy_from_slice(&1_232_391_219_124_912_494u64.to_be_bytes());
        assert_eq!(r.trace_id, Some(t));
        assert_eq!(r.span_id, Some(9_876_543_210u64.to_be_bytes()));
        // A date in ms; a 128-bit trace id in hex; a 64-bit one in hex.
        let r = record_of(br#"{"date":1597678284507,"message":"m","trace_id":"0af7651916cd43dd8448eb211c80319c","span_id":"b7ad6b7169203331"}"#).unwrap();
        assert_eq!(r.ts, 1_597_678_284_507_000_000);
        assert_eq!(r.trace_id.map(hex::encode).as_deref(), Some("0af7651916cd43dd8448eb211c80319c"));
        assert_eq!(r.span_id.map(hex::encode).as_deref(), Some("b7ad6b7169203331"));
        for bad in [&b"not json"[..], br#"[1,2]"#, br#"{"message":"no date"}"#, br#"{"date":"yesterday"}"#] {
            assert!(record_of(bad).is_none(), "{}", String::from_utf8_lossy(bad));
        }
    }

    /// Objects read into a view: a search and a range keep what they say, the archive's tags added,
    /// lines that are no event counted; an object that unzips past its bound refused.
    #[test]
    fn an_object_s_events_go_to_the_view_as_the_filter_says() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_view(dir.path()).unwrap();
        let mut w = ViewWriter { store: Arc::clone(&store), builder: kiln_obs::batch::Builder::new(), point: 0, since_cut: 0 };
        let mut lines = Vec::new();
        for i in 0..100i64 {
            let r = Record { ts: 1_800_000_000_000_000_000 + i * 1_000_000_000, severity: if i % 4 == 0 { 17 } else { 9 }, service: "web".into(), message: format!("m{i}"), ..Record::default() };
            let mut raw = Vec::new();
            kiln_obs::logs::record::put_row(&mut raw, &r, 0);
            let row = kiln_obs::logs::record::Rows::new(&raw, 1).next_row().unwrap().unwrap();
            archive::event(&row, "id", false, &mut lines, &mut archive::Scratch::default());
        }
        lines.extend_from_slice(b"not an event\n\n");
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut gz, &lines).unwrap();
        let body = gz.finish().unwrap();
        let f = Filter {
            search: kiln_obs::logs::search::parse("status:error").unwrap(),
            from: 1_800_000_000_000_000_000 + 10 * 1_000_000_000,
            to: 1_800_000_000_000_000_000 + 90 * 1_000_000_000,
            tags: vec![("team".into(), "intake".into())],
        };
        let c = read_object(body.as_slice(), &f, &mut w, UNZIPPED_MAX).unwrap();
        w.cut().unwrap();
        // Errors (every fourth) of seconds 10 to 89: 12, 16, … 88.
        assert_eq!(c, Counts { events: 101, kept: 20, malformed: 1 });
        let at = store.read().unwrap().logs_at();
        let found = at.search(&kiln_obs::logs::search::parse("*").unwrap(), 0, i64::MAX, 1000).unwrap();
        assert_eq!(found.records.len(), 20);
        assert!(found.records.iter().all(|r| r.severity == 17 && r.attrs.iter().any(|(k, v)| k == "team" && v == "intake")));
        // A bomb: past the bound, refused (a bound of a MiB, two MiB of it).
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut gz, &vec![b'\n'; 2 << 20]).unwrap();
        let bomb = gz.finish().unwrap();
        assert!(read_object(bomb.as_slice(), &f, &mut w, 1 << 20).unwrap_err().contains("unzips past"));
        // As it comes, in chunks of every size (a byte, odd, past a line's end), from another
        // thread: what it holds, the same; and a reader that stops ends the read.
        let dir = tempfile::tempdir().unwrap();
        let mut w = ViewWriter { store: open_view(dir.path()).unwrap(), builder: kiln_obs::batch::Builder::new(), point: 0, since_cut: 0 };
        let (tx, rx) = async_channel::bounded(CHUNKS);
        let sizes = [1usize, 7, 4096, 3, 65_536, 1];
        let chunks: Vec<bytes::Bytes> = {
            let (mut out, mut at, mut i) = (Vec::new(), 0, 0);
            while at < body.len() {
                let n = sizes[i % sizes.len()].min(body.len() - at);
                out.push(bytes::Bytes::copy_from_slice(&body[at..at + n]));
                (at, i) = (at + n, i + 1);
            }
            out
        };
        let sender = std::thread::spawn(move || chunks.into_iter().all(|c| tx.send_blocking(c).is_ok()));
        assert_eq!(read_object(Coming { rx, chunk: bytes::Bytes::new() }, &f, &mut w, UNZIPPED_MAX).unwrap(), Counts { events: 101, kept: 20, malformed: 1 });
        assert!(sender.join().unwrap());
        let (tx, rx) = async_channel::bounded(CHUNKS);
        let sender = std::thread::spawn(move || (0..1_000).all(|_| tx.send_blocking(bytes::Bytes::from_static(b"not gzip")).is_ok()));
        assert!(read_object(Coming { rx, chunk: bytes::Bytes::new() }, &f, &mut w, UNZIPPED_MAX).is_err());
        assert!(!sender.join().unwrap(), "the read goes on though its reader stopped");
    }

    /// A range's folders: a day whole where it holds the day, its hours where it holds part.
    #[test]
    fn a_range_is_listed_by_its_days_and_hours() {
        let day = 17_666i64;
        let h = |hour: i64| (day * 24 + hour) * 3_600_000;
        assert_eq!(folders("p", h(14) + 5, h(14) + 6), vec!["p/dt=20180515/hour=14/"]);
        assert_eq!(folders("p", h(14), h(15)), vec!["p/dt=20180515/hour=14/"]);
        assert_eq!(folders("p", h(23), h(25) + 1), vec!["p/dt=20180515/hour=23/", "p/dt=20180516/hour=00/", "p/dt=20180516/hour=01/"]);
        assert_eq!(folders("", h(0), h(24)), vec!["dt=20180515/"]);
        assert_eq!(folders("p", h(22), h(48) + 1), vec!["p/dt=20180515/hour=22/", "p/dt=20180515/hour=23/", "p/dt=20180516/", "p/dt=20180517/hour=00/"]);
        // A range of 400 days: a folder a day.
        assert_eq!(folders("p", h(0), h(400 * 24)).len(), 400);
    }
}
