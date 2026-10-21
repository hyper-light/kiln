//! Log archives (`docs/observe.md` §10.5): the records kiln takes, written to an object store as
//! Datadog's Log Archives hold them (`kiln_obs::logs::archive`: their events, their objects'
//! names), by the server, file by file of its log store.
//!
//! An archive is Datadog's (its v2 API's shape): a name, a search (the records it takes: each record
//! goes to the first archive in order whose search it matches, as Datadog's), a destination (an S3
//! bucket, any S3-compatible store's, a Cloud Storage bucket or an Azure Storage container, and a
//! path within it), the kiln cloud integration whose credentials write there (`integration.cloud`,
//! its name), and whether its events carry tags. kiln's own events (`source:kiln.event`) are not
//! archived unless a search names them, as a log monitor's.
//!
//! What is archived is the log store's files, in their order, each once: a file is a cut's records
//! (`kiln_obs::store::Store::log_files`), written once and never changed (only expired whole), so
//! every record is in exactly one. A file is read with no hold of the store, each record to its
//! archive's objects: gzip'd parts of an hour's events, 100 MB of them at the most, each named of the
//! file's contents, the archive, the hour and the part, and written as it closes. A file archived
//! again writes the same objects over themselves, never twice:
//!
//! - Each archive keeps its own place (the last file every object of which is written for it): one
//!   failing holds back no other, and is tried again after a minute (two rounds), doubling to 16.
//! - A file is begun with the archives as they are then (their searches, order and tags) and kept to
//!   them until every archive it was begun with is done with it: a change applies to the files cut
//!   after it, and a file tried again splits into the same parts. A part written (to a destination
//!   as it still is) is not written again.
//! - An archive made begins after the newest file held (Datadog's begins with what comes after); one
//!   whose place was lost begins with the oldest, written over what it wrote before.
//! - An install numbers the files anew: each place follows its file by its contents.
//!
//! What it keeps: `<observe>/archives.json` (the archives, in their order) and `archives.state.json`
//! (each archive's place, the files begun, the parts written).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use bytes::Bytes;
use kiln_obs::batch::{Kind, SeriesKey};
use kiln_obs::logs::archive;
use kiln_obs::logs::file::LogFile;
use kiln_obs::logs::record::Record;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::cloud::{Api, Call, Cloud, Clouds, aws, azure, gcp, sigv4};
use crate::http::client::Client;

/// Archives at the most: each record is matched against them in order until one takes it.
const ARCHIVES: usize = 64;
/// How often the archiver looks for files to archive: a cut comes about once a minute under load,
/// and within `kiln_obs::group::CUT_AFTER` when records come slowly.
const EVERY: Duration = Duration::from_secs(30);
/// How long an object's upload may take: a part of 100 MB of events is some 10 MB gzip'd.
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(300);
/// gzip's level: zlib's default, as Datadog's writers (Vector's sink, the Observability Pipelines
/// Worker) use.
const GZIP_LEVEL: u32 = 6;
/// Parts of one archive open at once in a file (each an hour's events): past it, the one written to
/// longest ago is closed. A file's records span an hour or two but for late ones.
const OPEN_PARTS: usize = 8;
/// Files archived at once (their encoding on the blocking pool, their parts written as they close):
/// an encoder writes some 53 MB/s of log lines a core at gzip's level 6 (§12.26) and the store
/// applies 106 at the most (§12.8), so three keep pace with a store at its busiest; fewer on a
/// machine of fewer cores (one left to the rest).
const IN_FLIGHT: usize = 3;
/// A failing archive is tried again after two rounds (a minute), doubling to this.
const BACKOFF_MAX: Duration = Duration::from_secs(16 * 60);
/// Azure Storage's REST version blobs are written with (a bearer token needs 2017-11-09 or later; a
/// block blob of up to 5,000 MiB in one PUT, 2019-12-12).
const AZURE_VERSION: &str = "2023-11-03";
/// Azure Storage's token audience, and Cloud Storage's scope for writing objects.
const AZURE_STORAGE: &str = "https://storage.azure.com";
const GCS_SCOPE: &str = "https://www.googleapis.com/auth/devstorage.read_write";
const GZIP: &str = "application/gzip";

type Counted<'a> = &'a (dyn Fn(Vec<(SeriesKey, Vec<(i64, f64)>)>) + Sync);
type Said<'a> = &'a (dyn Fn(Record) + Sync);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// How often the archiver looks; `KILN_OBSERVE_ARCHIVE_EVERY_MS` makes it sooner, for a test.
fn every() -> Duration {
    static EVERY_MS: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *EVERY_MS.get_or_init(|| std::env::var("KILN_OBSERVE_ARCHIVE_EVERY_MS").ok().and_then(|v| v.parse().ok()).map_or(EVERY, Duration::from_millis))
}

// ---------------------------------------------------------------------------------- archives

/// The kiln cloud integration whose credentials write an archive's objects (`cloud`, its name);
/// Datadog's own fields (`account_id`, `role_name`, `client_email`, …) kept as given.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Integration {
    #[serde(default)]
    pub cloud: String,
    #[serde(flatten)]
    pub datadog: BTreeMap<String, Value>,
}

/// S3's server-side encryption of an archive's objects (Datadog's `encryption`): the bucket's own
/// (`NO_OVERRIDE`), S3's keys (`SSE_S3`), or a KMS key's (`SSE_KMS`, its ARN).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Encryption {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

/// Where an archive's objects go.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Destination {
    S3 {
        bucket: String,
        #[serde(default)]
        path: String,
        #[serde(default)]
        integration: Integration,
        /// The bucket's region; none: the cloud integration's first, else `AWS_REGION`, else
        /// us-east-1.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        region: Option<String>,
        /// S3's storage class (`STANDARD_IA`, `GLACIER_IR`, …); none: the bucket's.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        storage_class: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        encryption: Option<Encryption>,
        /// An S3-compatible store's address (MinIO's, R2's, …): objects at
        /// `<endpoint>/<bucket>/<key>`; none: AWS's, at the bucket's own host.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        endpoint: Option<String>,
    },
    Gcs {
        bucket: String,
        #[serde(default)]
        path: String,
        #[serde(default)]
        integration: Integration,
        /// Cloud Storage's address (an emulator's); none: Google's.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        endpoint: Option<String>,
    },
    Azure {
        storage_account: String,
        container: String,
        #[serde(default)]
        path: String,
        #[serde(default)]
        integration: Integration,
        /// The account's blob address (an emulator's: `http://host:10000/<account>`); none:
        /// `https://<account>.blob.core.windows.net`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        endpoint: Option<String>,
    },
}

impl Destination {
    fn integration(&self) -> &Integration {
        match self {
            Self::S3 { integration, .. } | Self::Gcs { integration, .. } | Self::Azure { integration, .. } => integration,
        }
    }

    fn path(&self) -> &str {
        match self {
            Self::S3 { path, .. } | Self::Gcs { path, .. } | Self::Azure { path, .. } => path,
        }
    }

    /// The kind of cloud integration that writes here.
    fn cloud_kind(&self) -> &'static str {
        match self {
            Self::S3 { .. } => "aws",
            Self::Gcs { .. } => "gcp",
            Self::Azure { .. } => "azure",
        }
    }

    /// Where its objects lie (its store, bucket and path; not how they are written): a part
    /// written to one place is not written to another.
    fn place(&self) -> u64 {
        let at = match self {
            Self::S3 { bucket, path, endpoint, .. } => format!("s3\0{bucket}\0{path}\0{}", endpoint.as_deref().unwrap_or("")),
            Self::Gcs { bucket, path, endpoint, .. } => format!("gcs\0{bucket}\0{path}\0{}", endpoint.as_deref().unwrap_or("")),
            Self::Azure { storage_account, container, path, endpoint, .. } => format!("azure\0{storage_account}\0{container}\0{path}\0{}", endpoint.as_deref().unwrap_or("")),
        };
        let d = <sha2::Sha256 as sha2::Digest>::digest(at.as_bytes());
        let mut b = [0u8; 8];
        for (dst, src) in b.iter_mut().zip(d.iter()) {
            *dst = *src;
        }
        u64::from_be_bytes(b)
    }
}

/// An archive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Archive {
    #[serde(default)]
    pub id: String,
    pub name: String,
    /// A log search: the records it takes.
    #[serde(default = "every_record")]
    pub query: String,
    pub destination: Destination,
    #[serde(default)]
    pub include_tags: bool,
    /// Datadog's, kept as given (kiln rehydrates nothing yet).
    #[serde(default)]
    pub rehydration_tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rehydration_max_scan_size_in_gb: Option<u64>,
    #[serde(default)]
    pub created: i64,
    #[serde(default)]
    pub modified: i64,
}

fn every_record() -> String {
    "*".into()
}

/// A bucket's, a container's or an account's name as object stores name them: letters, digits and
/// `.-_`.
fn store_name(what: &str, v: &str) -> Result<(), String> {
    if v.is_empty() || v.len() > 255 || !v.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_')) {
        return Err(format!("{what}: letters, digits, '.', '-' and '_' ({v:?})"));
    }
    Ok(())
}

/// An endpoint: an http or https URL, its trailing slashes gone.
fn url_of(e: &str) -> Result<&str, String> {
    let e = e.trim().trim_end_matches('/');
    if !(e.starts_with("https://") || e.starts_with("http://")) || e.len() <= "https://".len() {
        return Err(format!("an endpoint is an http or https URL ({e:?})"));
    }
    Ok(e)
}

impl Archive {
    /// Whether it is one to keep: a name, a search that parses, a destination named as its store
    /// names them, a path of no `..`, `\` or control characters, a cloud integration to write with.
    pub fn check(&self) -> Result<(), String> {
        if self.name.trim().is_empty() || self.name.len() > 100 {
            return Err("a name, 100 characters at the most".into());
        }
        kiln_obs::logs::search::parse(&self.query).map_err(|e| format!("its query: {e}"))?;
        let path = self.destination.path();
        if path.len() > 512 || path.split('/').any(|p| p == "..") || path.chars().any(|c| c.is_control() || c == '\\') {
            return Err("a path of 512 bytes at the most, without '..', '\\' or control characters".into());
        }
        if self.destination.integration().cloud.trim().is_empty() {
            return Err("integration.cloud: the name of the kiln cloud integration whose credentials write here".into());
        }
        let endpoint = match &self.destination {
            Destination::S3 { bucket, encryption, endpoint, .. } => {
                store_name("its bucket", bucket)?;
                match encryption.as_ref().map(|e| (e.kind.as_str(), e.key.as_deref().map(str::trim))) {
                    None | Some(("NO_OVERRIDE" | "SSE_S3", _)) => {}
                    Some(("SSE_KMS", Some(k))) if !k.is_empty() => {}
                    Some(("SSE_KMS", _)) => return Err("encryption SSE_KMS: its key (an ARN)".into()),
                    Some((other, _)) => return Err(format!("encryption {other:?}: NO_OVERRIDE, SSE_S3 or SSE_KMS")),
                }
                endpoint
            }
            Destination::Gcs { bucket, endpoint, .. } => {
                store_name("its bucket", bucket)?;
                endpoint
            }
            Destination::Azure { storage_account, container, endpoint, .. } => {
                store_name("its storage_account", storage_account)?;
                store_name("its container", container)?;
                endpoint
            }
        };
        if let Some(e) = endpoint {
            url_of(e)?;
        }
        Ok(())
    }
}

/// How an archive fares: Datadog's `state` (`UNKNOWN` until it writes or fails, then `WORKING` or
/// `FAILING`), and what kiln counted of it since the server started.
#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub state: &'static str,
    pub error: Option<String>,
    pub events: u64,
    pub objects: u64,
    /// Bytes written (gzip'd).
    pub bytes: u64,
    /// Unix ms: its last object written.
    pub last_written: Option<i64>,
    /// Log files held not yet archived to it.
    pub pending: u64,
    #[serde(skip)]
    failures: u32,
    #[serde(skip)]
    retry_at: Option<Instant>,
}

impl Default for Status {
    fn default() -> Self {
        Self { state: "UNKNOWN", error: None, events: 0, objects: 0, bytes: 0, last_written: None, pending: 0, failures: 0, retry_at: None }
    }
}

// ------------------------------------------------------------------------------------- state

/// A log file by its contents: what an install keeps under a new number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct FileKey {
    crc: u32,
    bytes: u64,
    min_ts: i64,
    max_ts: i64,
}

impl FileKey {
    fn of(f: &LogFile) -> Self {
        Self { crc: f.crc, bytes: f.bytes, min_ts: f.min_ts, max_ts: f.max_ts }
    }

    fn seed(&self, archive: &str) -> [u8; 8] {
        archive::seed(archive, self.crc, self.bytes, self.min_ts, self.max_ts)
    }
}

/// An archive as it routes a file: what takes the file's records, in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Route {
    id: String,
    query: String,
    tags: bool,
}

fn routes_of(archives: &[Archive]) -> Vec<Route> {
    archives.iter().map(|a| Route { id: a.id.clone(), query: a.query.clone(), tags: a.include_tags }).collect()
}

/// Where an archive is: the last file all of whose objects are written for it (its number and
/// contents).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Place {
    seq: u64,
    file: FileKey,
}

/// A file begun (its contents, and its number as last seen), and the routing it was begun with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Begun {
    file: FileKey,
    seq: u64,
    routing: usize,
}

/// A part written: an archive's, of a file, an hour (since the epoch) and its number in it, to a
/// place ([`Destination::place`]).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct Written {
    archive: String,
    file: FileKey,
    hour: i64,
    part: u32,
    place: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    /// Each archive's place (none: before every file); an archive without one begins with the
    /// oldest file held.
    #[serde(default)]
    done: BTreeMap<String, Option<Place>>,
    /// The routings files were begun with, each once.
    #[serde(default)]
    routings: Vec<Vec<Route>>,
    #[serde(default)]
    begun: Vec<Begun>,
    #[serde(default)]
    written: BTreeSet<Written>,
}

impl State {
    /// Whether archive `id` is yet to write file `seq`.
    fn wants(&self, id: &str, seq: u64) -> bool {
        self.done.get(id).copied().flatten().is_none_or(|p| seq > p.seq)
    }

    fn routing(&self, file: &FileKey) -> Option<&Vec<Route>> {
        self.begun.iter().find(|b| b.file == *file).and_then(|b| self.routings.get(b.routing))
    }

    /// File `seq` (`file`) begun with `routes`, unless it was.
    fn pin(&mut self, file: FileKey, seq: u64, routes: &[Route]) {
        if self.begun.iter().any(|b| b.file == file) {
            return;
        }
        let routing = match self.routings.iter().position(|r| r == routes) {
            Some(i) => i,
            None => {
                self.routings.push(routes.to_vec());
                self.routings.len() - 1
            }
        };
        self.begun.push(Begun { file, seq, routing });
    }

    /// Kept to the files held (`files`, their numbers and contents, oldest first) and the archives
    /// kept: each place and file begun follows its file to its number now; a file begun is done
    /// with once no archive kept that it was begun with wants it, or once it is gone (one newer
    /// than every file in `files`, begun since they were listed, is kept); the parts of files done
    /// with go, and the routings of none. Whether anything changed.
    fn follow(&mut self, archives: &[Archive], files: &[(u64, FileKey)]) -> bool {
        let mut changed = false;
        let now_at = |file: &FileKey, seq: u64| files.iter().find(|(s, f)| f == file && *s != seq).map(|(s, _)| *s);
        for place in self.done.values_mut().flatten() {
            if let Some(seq) = now_at(&place.file, place.seq) {
                place.seq = seq;
                changed = true;
            }
        }
        for b in &mut self.begun {
            if let Some(seq) = now_at(&b.file, b.seq) {
                b.seq = seq;
                changed = true;
            }
        }
        let kept = |id: &str| archives.iter().any(|a| a.id == id);
        let newest = files.last().map_or(0, |(seq, _)| *seq);
        let before = (self.done.len(), self.begun.len(), self.written.len(), self.routings.len());
        self.done.retain(|id, _| kept(id));
        let mut begun = std::mem::take(&mut self.begun);
        begun.retain(|b| {
            let held = files.iter().any(|(_, f)| *f == b.file);
            if !held {
                return b.seq > newest;
            }
            self.routings.get(b.routing).is_some_and(|r| r.iter().any(|route| kept(&route.id) && self.wants(&route.id, b.seq)))
        });
        self.begun = begun;
        let begun: BTreeSet<FileKey> = self.begun.iter().map(|b| b.file).collect();
        self.written.retain(|w| begun.contains(&w.file) && kept(&w.archive));
        // Routings no file is begun with go, the rest numbered anew.
        let used: BTreeSet<usize> = self.begun.iter().map(|b| b.routing).collect();
        if used.len() != self.routings.len() {
            let mut renumber = BTreeMap::new();
            let mut routings = Vec::with_capacity(used.len());
            for (i, r) in std::mem::take(&mut self.routings).into_iter().enumerate() {
                if used.contains(&i) {
                    renumber.insert(i, routings.len());
                    routings.push(r);
                }
            }
            self.routings = routings;
            for b in &mut self.begun {
                b.routing = renumber.get(&b.routing).copied().unwrap_or(0);
            }
        }
        changed || before != (self.done.len(), self.begun.len(), self.written.len(), self.routings.len())
    }

    /// The archives changing from `before` to `after` (in order), the files held `files`: each file
    /// held that an archive of the old routing wants, not yet begun, is begun with the old routing
    /// (a change applies to the files cut after it); an archive new begins after the newest file.
    fn change(&mut self, before: &[Archive], after: &[Archive], files: &[(u64, FileKey)]) {
        let old = routes_of(before);
        if old != routes_of(after) {
            for (seq, file) in files {
                if self.routing(file).is_none() && old.iter().any(|r| self.wants(&r.id, *seq)) {
                    self.pin(*file, *seq, &old);
                }
            }
        }
        let newest = files.last().map(|(seq, file)| Place { seq: *seq, file: *file });
        for a in after {
            if !before.iter().any(|b| b.id == a.id) {
                self.done.insert(a.id.clone(), newest);
            }
        }
    }
}

/// What a file is archived with: its routes, and which of them want it.
#[derive(Debug, Clone)]
struct Work {
    routes: Vec<Route>,
    want: Vec<bool>,
}

/// What a route wrote of a file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Counts {
    events: u64,
    objects: u64,
    bytes: u64,
}

// ---------------------------------------------------------------------------------- archives

struct Inner {
    archives: Vec<Archive>,
    state: State,
}

/// What writes an archive's objects: its cloud integration's settings, and its credentials as they
/// were last got (kept until they end).
enum Writer {
    Aws { cfg: aws::Config, creds: aws::Creds },
    Gcp { cfg: gcp::Config, bearer: gcp::Bearer },
    Azure { cfg: azure::Config, token: Option<azure::Token> },
}

impl Writer {
    fn of(cloud: Cloud) -> Self {
        match cloud {
            Cloud::Aws(cfg) => Self::Aws { cfg, creds: aws::Creds::default() },
            Cloud::Gcp(cfg) => Self::Gcp { bearer: gcp::Bearer::new(&cfg, GCS_SCOPE), cfg },
            Cloud::Azure(cfg) => Self::Azure { cfg, token: None },
        }
    }

    fn is(&self, cloud: &Cloud) -> bool {
        match (self, cloud) {
            (Self::Aws { cfg, .. }, Cloud::Aws(c)) => cfg == c,
            (Self::Gcp { cfg, .. }, Cloud::Gcp(c)) => cfg == c,
            (Self::Azure { cfg, .. }, Cloud::Azure(c)) => cfg == c,
            _ => false,
        }
    }
}

/// The archives and their archiver.
pub struct Archives {
    path: PathBuf,
    state_path: PathBuf,
    inner: Mutex<Inner>,
    status: Mutex<BTreeMap<String, Status>>,
    clouds: Arc<Clouds>,
    http: Arc<Client>,
    writers: Mutex<HashMap<String, Writer>>,
}

#[derive(Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    archives: Vec<Archive>,
}

/// `path` read as JSON; one that does not read is moved aside (`<name>.broken-<ms>`) and said, so
/// what is saved next does not write over it.
fn read_or_aside<T: serde::de::DeserializeOwned + Default>(path: &Path) -> T {
    let Ok(bytes) = std::fs::read(path) else { return T::default() };
    match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            let aside = path.with_extension(format!("json.broken-{}", crate::util::now_ms()));
            let moved = std::fs::rename(path, &aside);
            tracing::error!(path = %path.display(), aside = %aside.display(), error = %e, moved = moved.is_ok(), "a file of observe's archives does not read: it is kept aside");
            T::default()
        }
    }
}

/// A refusal as the API answers it: its status and what it says.
pub type Refusal = (u16, String);

impl Archives {
    pub fn open(dir: &Path, clouds: Arc<Clouds>, http: Arc<Client>) -> Arc<Self> {
        let path = dir.join("archives.json");
        let state_path = dir.join("archives.state.json");
        let saved: Saved = read_or_aside(&path);
        let state: State = read_or_aside(&state_path);
        Arc::new(Self { path, state_path, inner: Mutex::new(Inner { archives: saved.archives, state }), status: Mutex::new(BTreeMap::new()), clouds, http, writers: Mutex::new(HashMap::new()) })
    }

    fn save_archives(&self, archives: &[Archive]) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(&serde_json::json!({ "archives": archives })).map_err(|e| e.to_string())?;
        crate::sync::group::save_atomic(&self.path, &bytes).map_err(|e| format!("{}: {e}", self.path.display()))
    }

    fn save_state(&self, state: &State) -> Result<(), String> {
        let bytes = serde_json::to_vec(state).map_err(|e| e.to_string())?;
        crate::sync::group::save_atomic(&self.state_path, &bytes).map_err(|e| format!("{}: {e}", self.state_path.display()))
    }

    fn saved_state(&self, state: &State) {
        if let Err(e) = self.save_state(state) {
            tracing::warn!(error = %e, "observe's archives could not save where they are");
        }
    }

    /// The archives, in their order, each its status.
    pub fn list(&self) -> Vec<(Archive, Status)> {
        let archives = lock(&self.inner).archives.clone();
        let status = lock(&self.status);
        archives
            .into_iter()
            .map(|a| {
                let s = status.get(&a.id).cloned().unwrap_or_default();
                (a, s)
            })
            .collect()
    }

    pub fn get(&self, id: &str) -> Option<(Archive, Status)> {
        let a = lock(&self.inner).archives.iter().find(|a| a.id == id).cloned()?;
        let s = lock(&self.status).get(id).cloned().unwrap_or_default();
        Some((a, s))
    }

    /// The archives' ids, in their order.
    pub fn order(&self) -> Vec<String> {
        lock(&self.inner).archives.iter().map(|a| a.id.clone()).collect()
    }

    /// The cloud integration `a` writes with, which must be of the kind its destination needs.
    fn cloud_of(&self, a: &Archive) -> Result<Cloud, String> {
        let name = &a.destination.integration().cloud;
        let c = self.clouds.get(name).ok_or_else(|| format!("integration.cloud: no cloud integration {name:?}"))?;
        let want = a.destination.cloud_kind();
        if c.kind() != want {
            return Err(format!("integration.cloud: {name} is a {} integration; this destination is written with an {want} one", c.kind()));
        }
        Ok(c)
    }

    /// The archives changed by `f`, the log files held `files` (`Store::log_files`): saved, the
    /// state first (an archive's place before the archive); what `f` gave.
    fn change<T>(&self, files: &[(u64, Arc<LogFile>)], f: impl FnOnce(&mut Vec<Archive>) -> Result<T, Refusal>) -> Result<T, Refusal> {
        let keys: Vec<(u64, FileKey)> = files.iter().map(|(seq, f)| (*seq, FileKey::of(f))).collect();
        let mut inner = lock(&self.inner);
        let mut after = inner.archives.clone();
        let out = f(&mut after)?;
        let Inner { archives, state } = &mut *inner;
        state.change(archives, &after, &keys);
        state.follow(&after, &keys);
        self.save_state(state).map_err(|e| (500, e))?;
        self.save_archives(&after).map_err(|e| (500, e))?;
        *archives = after;
        Ok(out)
    }

    /// A new archive, last in order, begun after the newest file held.
    pub fn create(&self, mut a: Archive, files: &[(u64, Arc<LogFile>)]) -> Result<Archive, Refusal> {
        a.check().map_err(|e| (400, e))?;
        self.cloud_of(&a).map_err(|e| (400, e))?;
        self.change(files, |archives| {
            if archives.len() >= ARCHIVES {
                return Err((400, format!("{ARCHIVES} archives at the most")));
            }
            if archives.iter().any(|x| x.name == a.name) {
                return Err((409, format!("an archive named {:?} already", a.name)));
            }
            let now = crate::util::now_ms();
            a.id = super::fresh_id("a", |id| archives.iter().any(|x| x.id == id));
            (a.created, a.modified) = (now, now);
            archives.push(a.clone());
            Ok(a)
        })
    }

    /// Archive `id` changed, its place in the order kept: a change of its search or tags applies to
    /// the files cut after it, of where it writes from the next part written.
    pub fn put(&self, id: &str, mut a: Archive, files: &[(u64, Arc<LogFile>)]) -> Result<Archive, Refusal> {
        a.check().map_err(|e| (400, e))?;
        self.cloud_of(&a).map_err(|e| (400, e))?;
        let changed = self.change(files, |archives| {
            if archives.iter().any(|x| x.name == a.name && x.id != id) {
                return Err((409, format!("an archive named {:?} already", a.name)));
            }
            let Some(old) = archives.iter_mut().find(|x| x.id == id) else { return Err((404, "no such archive".into())) };
            (a.id, a.created, a.modified) = (old.id.clone(), old.created, crate::util::now_ms());
            *old = a.clone();
            Ok(a)
        })?;
        lock(&self.writers).remove(id);
        Ok(changed)
    }

    pub fn delete(&self, id: &str, files: &[(u64, Arc<LogFile>)]) -> Result<(), Refusal> {
        self.change(files, |archives| {
            let before = archives.len();
            archives.retain(|x| x.id != id);
            if archives.len() == before { Err((404, "no such archive".into())) } else { Ok(()) }
        })?;
        lock(&self.writers).remove(id);
        lock(&self.status).remove(id);
        Ok(())
    }

    /// The archives put in an order: every one's id, each once.
    pub fn set_order(&self, ids: &[String], files: &[(u64, Arc<LogFile>)]) -> Result<Vec<String>, Refusal> {
        self.change(files, |archives| {
            let mut have: Vec<&str> = archives.iter().map(|a| a.id.as_str()).collect();
            let mut asked: Vec<&str> = ids.iter().map(String::as_str).collect();
            have.sort_unstable();
            asked.sort_unstable();
            if have != asked {
                return Err((422, "archive_ids: every archive's id, each once".into()));
            }
            let mut by_id: BTreeMap<String, Archive> = archives.drain(..).map(|a| (a.id.clone(), a)).collect();
            *archives = ids.iter().filter_map(|id| by_id.remove(id)).collect();
            Ok(ids.to_vec())
        })
    }

    // ------------------------------------------------------------------------------- archiver

    /// Archives what is due every [`every`] on the server while it leads, until it stops.
    pub async fn run(self: Arc<Self>, shutdown: crate::rt::Shutdown) {
        while shutdown.sleep(every()).await {
            let Some(o) = super::api::get() else { continue };
            let v = o.view();
            if v.leader == 0 || v.leader != v.member {
                continue;
            }
            let Some(store) = o.store() else { continue };
            let files = store.read().unwrap_or_else(PoisonError::into_inner).log_files();
            self.round(&files, &|series| o.write(series), &|record| o.log(vec![record])).await;
        }
    }

    /// One round: every file held (`files`, oldest first) that an archive wants, [`IN_FLIGHT`] at
    /// once, each archive's objects of it written; then, file by file in order, each archive done
    /// with it that wrote it all (or that its routing does not name), and none past a file it
    /// failed: an archive that fails is left for the round (its parts written since are kept, and
    /// not written again) and tried again after its backoff. kiln's counts of it written with
    /// `write`, its events (failing, recovered) with `event`.
    async fn round(&self, files: &[(u64, Arc<LogFile>)], write: Counted<'_>, event: Said<'_>) {
        let keys: Vec<(u64, FileKey)> = files.iter().map(|(seq, f)| (*seq, FileKey::of(f))).collect();
        let now = Instant::now();
        let stopped: Mutex<BTreeSet<String>> = Mutex::new(lock(&self.status).iter().filter(|(_, s)| s.retry_at.is_some_and(|t| t > now)).map(|(id, _)| id.clone()).collect());
        {
            let mut inner = lock(&self.inner);
            let Inner { archives, state } = &mut *inner;
            if archives.is_empty() && state.begun.is_empty() {
                return;
            }
            if state.follow(archives, &keys) {
                self.saved_state(state);
            }
        }
        let in_flight = std::thread::available_parallelism().map_or(1, |n| n.get().saturating_sub(1)).clamp(1, IN_FLIGHT);
        let stopped = &stopped;
        // Each file owned by its future (borrowed items would make the round's future less general
        // than a task needs), all polled on this task (hyper-rt's I/O asks it), handed over in order.
        let items: Vec<(u64, Arc<LogFile>, FileKey)> = files.iter().zip(&keys).map(|((seq, f), (_, key))| (*seq, Arc::clone(f), *key)).collect();
        crate::rt::each_in_order(
            items,
            in_flight,
            |(seq, file, key)| async move {
                let snapshot = lock(stopped).clone();
                let work = self.begin(seq, key, &snapshot);
                let results = if work.want.contains(&true) { self.archive_file(key, &file, &work).await } else { Vec::new() };
                (seq, key, work, results)
            },
            |(seq, key, work, results)| {
                let mut inner = lock(&self.inner);
                let Inner { archives, state } = &mut *inner;
                let mut stop = lock(stopped);
                // The archives kept that its routing does not name (made after it was begun) are
                // done with it, in their turn.
                for a in archives.iter() {
                    if !work.routes.iter().any(|r| r.id == a.id) && !stop.contains(&a.id) && state.wants(&a.id, seq) {
                        state.done.insert(a.id.clone(), Some(Place { seq, file: key }));
                    }
                }
                for ((route, want), result) in work.routes.iter().zip(&work.want).zip(results) {
                    if !*want {
                        continue;
                    }
                    let Some(name) = archives.iter().find(|a| a.id == route.id).map(|a| a.name.clone()) else { continue };
                    match result {
                        Ok(counts) => {
                            self.counted(&route.id, &name, counts, write);
                            // Past a file it failed this round, an archive stays where it is.
                            if !stop.contains(&route.id) {
                                state.done.insert(route.id.clone(), Some(Place { seq, file: key }));
                                self.worked(&route.id, &name, event);
                            }
                        }
                        Err(e) => {
                            if stop.insert(route.id.clone()) {
                                self.failed(&route.id, &name, &e, write, event);
                            }
                        }
                    }
                }
                drop(stop);
                state.follow(archives, &keys);
                self.saved_state(state);
            },
        )
        .await;
        // What each archive has yet to write.
        let inner = lock(&self.inner);
        let mut status = lock(&self.status);
        let ms = super::clock_ms();
        let mut series = Vec::new();
        for a in &inner.archives {
            let pending = keys.iter().filter(|(seq, _)| inner.state.wants(&a.id, *seq)).count() as u64;
            status.entry(a.id.clone()).or_default().pending = pending;
            if let Ok(k) = SeriesKey::new("kiln.archives.pending", [("archive", a.name.as_str())], Kind::Gauge) {
                series.push((k, vec![(ms, pending as f64)]));
            }
        }
        drop((status, inner));
        write(series);
    }

    /// File `seq` (`file`) as this round finds it: its routing (the one it was begun with, or the
    /// archives as they are), and which of them want it (kept, not stopped, yet to write it): begun
    /// now, with that routing, if one does.
    fn begin(&self, seq: u64, file: FileKey, stopped: &BTreeSet<String>) -> Work {
        let mut inner = lock(&self.inner);
        let Inner { archives, state } = &mut *inner;
        let pinned = state.routing(&file).cloned();
        let routes = pinned.clone().unwrap_or_else(|| routes_of(archives));
        let want: Vec<bool> = routes.iter().map(|r| archives.iter().any(|a| a.id == r.id) && !stopped.contains(&r.id) && state.wants(&r.id, seq)).collect();
        if pinned.is_none() && want.contains(&true) {
            state.pin(file, seq, &routes);
            self.saved_state(state);
        }
        Work { routes, want }
    }

    /// One file archived: its records encoded on the blocking pool, each part written as it closes
    /// (the encoder waits while one is being written). Each route's outcome: what it wrote, or why
    /// it could not.
    async fn archive_file(&self, key: FileKey, file: &Arc<LogFile>, work: &Work) -> Vec<Result<Counts, String>> {
        let (tx, rx) = async_channel::bounded::<Part>(1);
        let (f, routes, want) = (Arc::clone(file), work.routes.clone(), work.want.clone());
        let encoding = crate::rt::blocking(move || encode(&f, &routes, &want, key, &tx));
        // The receiver moves in: once this ends, the encoder's next send fails and it stops.
        let uploading = async move {
            let mut out: Vec<Result<Counts, String>> = vec![Ok(Counts::default()); work.routes.len()];
            while let Ok(part) = rx.recv().await {
                let route = part.route;
                let Some(id) = work.routes.get(route).map(|r| r.id.clone()) else { continue };
                if !matches!(out.get(route), Some(Ok(_))) {
                    continue;
                }
                let (events, bytes) = (part.events, part.body.len() as u64);
                let written = self.write_part(&id, key, part).await;
                if let Some(slot) = out.get_mut(route) {
                    match (written, slot.as_mut()) {
                        (Ok(true), Ok(c)) => {
                            c.events += events;
                            c.objects += 1;
                            c.bytes += bytes;
                        }
                        (Ok(_), _) => {}
                        (Err(e), _) => *slot = Err(e),
                    }
                }
                if out.iter().zip(&work.want).all(|(r, w)| !*w || r.is_err()) {
                    break;
                }
            }
            out
        };
        let (encoded, mut out) = futures::future::join(encoding, uploading).await;
        let failed = match encoded {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(e),
            Err(e) => Some(format!("{e:#}")),
        };
        if let Some(e) = failed {
            for (slot, want) in out.iter_mut().zip(&work.want) {
                if *want && slot.is_ok() {
                    *slot = Err(format!("its file could not be read: {e}"));
                }
            }
        }
        out
    }

    /// A part written to archive `id`'s destination as it is now, unless written there already:
    /// whether it was written now.
    async fn write_part(&self, id: &str, file: FileKey, part: Part) -> Result<bool, String> {
        let (a, w, done) = {
            let inner = lock(&self.inner);
            let a = inner.archives.iter().find(|a| a.id == id).cloned().ok_or_else(|| "removed as it was written".to_owned())?;
            let w = Written { archive: id.to_owned(), file, hour: part.hour, part: part.number, place: a.destination.place() };
            let done = inner.state.written.contains(&w);
            (a, w, done)
        };
        if done {
            return Ok(false);
        }
        let object = archive::key(a.destination.path(), part.newest_ms, &archive::uuid(file.seed(id), part.hour, part.number));
        self.upload(&a, &object, Bytes::from(part.body)).await.map_err(|e| format!("{e} (writing {object})"))?;
        let mut inner = lock(&self.inner);
        if inner.state.begun.iter().any(|b| b.file == file) {
            inner.state.written.insert(w);
            self.save_state(&inner.state)?;
        }
        Ok(true)
    }

    /// One object written to `a`'s destination with its cloud integration's credentials (kept from
    /// one object to the next while the integration is the same).
    async fn upload(&self, a: &Archive, key: &str, body: Bytes) -> Result<(), String> {
        let cloud = self.cloud_of(a)?;
        let held = lock(&self.writers).remove(&a.id);
        let mut writer = match held {
            Some(w) if w.is(&cloud) => w,
            _ => Writer::of(cloud),
        };
        let api = Api::new(&self.http);
        let result = put_object(&api, &a.destination, &mut writer, key, body, crate::util::now_ms()).await;
        lock(&self.writers).insert(a.id.clone(), writer);
        result
    }

    /// What an archive wrote of a file, as it wrote it: its status's counts, and kiln's
    /// (`kiln.archives.*`).
    fn counted(&self, id: &str, name: &str, c: Counts, write: Counted<'_>) {
        if c.objects == 0 {
            return;
        }
        let ms = super::clock_ms();
        {
            let mut st = lock(&self.status);
            let s = st.entry(id.to_owned()).or_default();
            (s.events, s.objects, s.bytes, s.last_written) = (s.events + c.events, s.objects + c.objects, s.bytes + c.bytes, Some(ms));
        }
        let mut series = Vec::new();
        for (metric, v) in [("kiln.archives.events", c.events), ("kiln.archives.objects", c.objects), ("kiln.archives.bytes", c.bytes)] {
            if let Ok(k) = SeriesKey::new(metric, [("archive", name)], Kind::Count) {
                series.push((k, vec![(ms, v as f64)]));
            }
        }
        write(series);
    }

    /// An archive done with a file: working (again: said once).
    fn worked(&self, id: &str, name: &str, event: Said<'_>) {
        let was_failing = {
            let mut st = lock(&self.status);
            let s = st.entry(id.to_owned()).or_default();
            let was = s.state == "FAILING";
            (s.state, s.error, s.failures, s.retry_at) = ("WORKING", None, 0, None);
            was
        };
        if was_failing {
            event(archive_event(name, 9, "recovered", format!("archive {name} recovered: writing again")));
        }
    }

    fn failed(&self, id: &str, name: &str, why: &str, write: Counted<'_>, event: Said<'_>) {
        let ms = super::clock_ms();
        let first = {
            let mut st = lock(&self.status);
            let s = st.entry(id.to_owned()).or_default();
            let first = s.state != "FAILING";
            let wait = every().saturating_mul(2u32 << s.failures.min(4)).min(BACKOFF_MAX);
            (s.state, s.error, s.failures, s.retry_at) = ("FAILING", Some(why.to_owned()), s.failures.saturating_add(1), Some(Instant::now() + wait));
            first
        };
        tracing::warn!(archive = name, error = why, "an archive of observe's could not write; it is tried again");
        if first {
            event(archive_event(name, 17, "failing", format!("archive {name} is failing: {why}")));
        }
        if let Ok(k) = SeriesKey::new("kiln.archives.failures", [("archive", name)], Kind::Count) {
            write(vec![(k, vec![(ms, 1.0)])]);
        }
    }
}

/// An event of an archive's (`source:kiln.event`, `event.type:archive.write`).
fn archive_event(name: &str, severity: u8, result: &str, message: String) -> Record {
    Record {
        ts: super::clock_ms().saturating_mul(1_000_000),
        severity,
        service: "kiln.archives".into(),
        source: kiln_obs::monitor::EVENTS.into(),
        message,
        attrs: vec![("event.type".into(), "archive.write".into()), ("archive".into(), name.into()), ("result".into(), result.into())],
        ..Record::default()
    }
}

// ------------------------------------------------------------------------------------ writing

/// An object written: S3's PUT (signed, its payload's hash with it), Cloud Storage's media upload,
/// Azure's block blob; an answer not 2xx an error saying it.
async fn put_object(api: &Api<'_>, dest: &Destination, writer: &mut Writer, key: &str, body: Bytes, now: i64) -> Result<(), String> {
    let answer = match (dest, writer) {
        (Destination::S3 { bucket, region, storage_class, encryption, endpoint, .. }, Writer::Aws { cfg, creds }) => {
            let region = region
                .clone()
                .or_else(|| cfg.regions.first().cloned())
                .or_else(|| super::cloud::env("AWS_REGION"))
                .or_else(|| super::cloud::env("AWS_DEFAULT_REGION"))
                .unwrap_or_else(|| "us-east-1".into());
            let keys = creds.get(api, cfg, &region, now).await.map_err(|e| format!("its credentials: {e:#}"))?;
            // The path signed is the path sent, encoded once (S3's way).
            let (url, path) = match endpoint {
                Some(e) => {
                    let path = format!("/{bucket}/{key}");
                    (format!("{}{}", url_of(e)?, sigv4::uri_encode(&path, true)), path)
                }
                None => {
                    let domain = if region.starts_with("cn-") { "amazonaws.com.cn" } else { "amazonaws.com" };
                    // A bucket named with dots is no host TLS names: its objects by path.
                    if bucket.contains('.') {
                        let path = format!("/{bucket}/{key}");
                        (format!("https://s3.{region}.{domain}{}", sigv4::uri_encode(&path, true)), path)
                    } else {
                        let path = format!("/{key}");
                        (format!("https://{bucket}.s3.{region}.{domain}{}", sigv4::uri_encode(&path, true)), path)
                    }
                }
            };
            let (authority, _) = super::cloud::authority_and_path(&url).map_err(|e| format!("{e:#}"))?;
            let mut headers: Vec<(String, String)> = vec![("host".into(), authority), ("content-type".into(), GZIP.into())];
            if let Some(c) = storage_class {
                headers.push(("x-amz-storage-class".into(), c.clone()));
            }
            match encryption.as_ref().map(|e| (e.kind.as_str(), e.key.as_deref())) {
                Some(("SSE_S3", _)) => headers.push(("x-amz-server-side-encryption".into(), "AES256".into())),
                Some(("SSE_KMS", key)) => {
                    headers.push(("x-amz-server-side-encryption".into(), "aws:kms".into()));
                    headers.push(("x-amz-server-side-encryption-aws-kms-key-id".into(), key.unwrap_or_default().trim().to_owned()));
                }
                _ => {}
            }
            let signed_with: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            let at = time::OffsetDateTime::from_unix_timestamp(now.div_euclid(1_000)).unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
            let signed = sigv4::sign(&keys, &region, "s3", &sigv4::Request { method: "PUT", path: &path, query: &[], headers: &signed_with, body: &body }, at);
            // `host` is the URL's to send, and `content-type` the call's own.
            let mut call = Call::put(&url, GZIP, body).timeout(UPLOAD_TIMEOUT);
            for (k, v) in headers.iter().skip(2).chain(signed.iter()) {
                call = call.header(k, v);
            }
            api.call(&call, |_| false).await.map_err(|e| format!("{e:#}"))?
        }
        (Destination::Gcs { bucket, endpoint, .. }, Writer::Gcp { bearer, .. }) => {
            let token = bearer.get(api, now).await.map_err(|e| format!("its credentials: {e:#}"))?;
            let base = endpoint.as_deref().map_or(Ok("https://storage.googleapis.com"), url_of)?;
            let url = format!("{base}/upload/storage/v1/b/{}/o?uploadType=media&name={}", super::cloud::encode(bucket), super::cloud::encode(key));
            api.call(&Call::post(&url, GZIP, body).bearer(&token).timeout(UPLOAD_TIMEOUT), |_| false).await.map_err(|e| format!("{e:#}"))?
        }
        (Destination::Azure { storage_account, container, endpoint, .. }, Writer::Azure { cfg, token }) => {
            let value = match token {
                Some(t) if t.until.saturating_sub(now) > super::cloud::REFRESH_MS => t.value.clone(),
                _ => {
                    let t = azure::token(api, cfg, AZURE_STORAGE, now).await.map_err(|e| format!("its credentials: {e:#}"))?;
                    let v = t.value.clone();
                    *token = Some(t);
                    v
                }
            };
            let base = match endpoint {
                Some(e) => url_of(e)?.to_owned(),
                None => format!("https://{storage_account}.blob.core.windows.net"),
            };
            let url = format!("{base}/{container}{}", sigv4::uri_encode(&format!("/{key}"), true));
            let date = httpdate(now);
            let call = Call::put(&url, GZIP, body).bearer(&value).header("x-ms-version", AZURE_VERSION).header("x-ms-blob-type", "BlockBlob").header("x-ms-date", &date).timeout(UPLOAD_TIMEOUT);
            api.call(&call, |_| false).await.map_err(|e| format!("{e:#}"))?
        }
        _ => return Err("its destination and its cloud integration are of different clouds".into()),
    };
    if answer.ok() { Ok(()) } else { Err(format!("{} {}", answer.status, answer.text())) }
}

/// RFC 7231's IMF-fixdate of `ms` (Azure's `x-ms-date`).
fn httpdate(ms: i64) -> String {
    let at = time::OffsetDateTime::from_unix_timestamp(ms.div_euclid(1_000)).unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    let day = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"].get(usize::from(at.weekday().number_days_from_monday())).copied().unwrap_or("Mon");
    let month = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"].get(usize::from(u8::from(at.month())).saturating_sub(1)).copied().unwrap_or("Jan");
    format!("{day}, {:02} {month} {:04} {:02}:{:02}:{:02} GMT", at.day(), at.year(), at.hour(), at.minute(), at.second())
}

// ----------------------------------------------------------------------------------- encoding

/// A part closed: its route, the hour its events are of, its number in that hour, its events, its
/// newest event's time (ms) and its gzip'd body.
struct Part {
    route: usize,
    hour: i64,
    number: u32,
    events: u64,
    newest_ms: i64,
    body: Vec<u8>,
}

/// A part being written.
struct Open {
    route: usize,
    hour: i64,
    number: u32,
    gz: flate2::write::GzEncoder<Vec<u8>>,
    raw: usize,
    events: u64,
    newest_ms: i64,
    /// The record last written to it (its place in the file).
    used: u32,
}

impl Open {
    fn close(self) -> std::io::Result<Part> {
        Ok(Part { route: self.route, hour: self.hour, number: self.number, events: self.events, newest_ms: self.newest_ms, body: self.gz.finish()? })
    }
}

/// A file's records, each to the first route whose search it matches (as a log monitor's,
/// `kiln_obs::monitor::logs_only`), each wanted route's as parts of an hour's events, sent on `tx`
/// as they close. A route's parts are of its records alone, whichever routes are wanted: the same
/// file and routes split alike every time. A route whose search does not parse takes nothing. Ends
/// early, quietly, once `tx`'s receiver is gone.
fn encode(file: &LogFile, routes: &[Route], want: &[bool], key: FileKey, tx: &async_channel::Sender<Part>) -> Result<(), String> {
    let searches: Vec<Option<kiln_obs::logs::search::Search>> = routes
        .iter()
        .map(|r| match kiln_obs::logs::search::parse(&kiln_obs::monitor::logs_only(&r.query)) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!(archive = %r.id, error = %e, "an archive's search does not parse: it takes no records");
                None
            }
        })
        .collect();
    let seeds: Vec<[u8; 8]> = routes.iter().map(|r| key.seed(&r.id)).collect();
    let mut open: Vec<Open> = Vec::new();
    let mut next: BTreeMap<(usize, i64), u32> = BTreeMap::new();
    let (mut text, mut raw, mut line, mut id) = (Vec::new(), Vec::new(), Vec::new(), String::new());
    let mut scratch = archive::Scratch::default();
    let mut rs = kiln_obs::logs::search::RowScratch::default();
    let send = |p: Part| tx.send_blocking(p).is_ok();
    let mut n: u32 = 0;
    for i in 0..file.blocks.len() {
        let b = file.read_text(i, &mut text).map_err(|e| e.to_string())?;
        b.decompress_text(&text, &mut raw).map_err(|e| e.to_string())?;
        let mut rows = b.rows(&raw);
        while let Some(r) = rows.next_row().map_err(|e| e.to_string())? {
            let at = n;
            n = n.wrapping_add(1);
            let Some(route) = searches.iter().position(|s| s.as_ref().is_some_and(|s| s.row(&r, &mut rs))) else { continue };
            if !want.get(route).copied().unwrap_or(false) {
                continue;
            }
            let (Some(seed), Some(tags)) = (seeds.get(route), routes.get(route).map(|r| r.tags)) else { continue };
            let ms = r.ts.div_euclid(1_000_000);
            let hour = archive::hour_of(r.ts);
            id.clear();
            archive::id(ms, *seed, at, &mut id);
            line.clear();
            archive::event(&r, &id, tags, &mut line, &mut scratch);
            // Its part: a full one closed first (an event larger than a part is a part of its own).
            let mut slot = open.iter().position(|o| o.route == route && o.hour == hour);
            if let Some(p) = slot
                && open.get(p).is_some_and(|o| o.raw > 0 && o.raw + line.len() > archive::PART_BYTES)
            {
                if !send(open.swap_remove(p).close().map_err(|e| e.to_string())?) {
                    return Ok(());
                }
                slot = None;
            }
            let slot = match slot {
                Some(p) => p,
                None => {
                    // The route's parts open at their bound: the one written to longest ago closed.
                    if open.iter().filter(|o| o.route == route).count() >= OPEN_PARTS
                        && let Some(p) = open.iter().enumerate().filter(|(_, o)| o.route == route).min_by_key(|(_, o)| o.used).map(|(p, _)| p)
                        && !send(open.swap_remove(p).close().map_err(|e| e.to_string())?)
                    {
                        return Ok(());
                    }
                    let number = next.entry((route, hour)).or_insert(0);
                    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(GZIP_LEVEL));
                    open.push(Open { route, hour, number: *number, gz, raw: 0, events: 0, newest_ms: ms, used: at });
                    *number += 1;
                    open.len() - 1
                }
            };
            let Some(o) = open.get_mut(slot) else { continue };
            o.gz.write_all(&line).map_err(|e| e.to_string())?;
            o.raw += line.len();
            o.events += 1;
            o.newest_ms = o.newest_ms.max(ms);
            o.used = at;
        }
    }
    open.sort_by_key(|o| (o.route, o.hour, o.number));
    for o in open {
        if !send(o.close().map_err(|e| e.to_string())?) {
            return Ok(());
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn integration(cloud: &str) -> Integration {
        Integration { cloud: cloud.into(), ..Integration::default() }
    }

    fn s3_at(bucket: &str, path: &str, cloud: &str, storage_class: Option<&str>, encryption: Option<Encryption>) -> Destination {
        Destination::S3 { bucket: bucket.into(), path: path.into(), integration: integration(cloud), region: None, storage_class: storage_class.map(str::to_owned), encryption, endpoint: None }
    }

    fn archive(id: &str, name: &str, query: &str) -> Archive {
        Archive {
            id: id.into(),
            name: name.into(),
            query: query.into(),
            destination: s3_at("logs", "/kiln/", "aws", None, None),
            include_tags: false,
            rehydration_tags: Vec::new(),
            rehydration_max_scan_size_in_gb: None,
            created: 0,
            modified: 0,
        }
    }

    /// Datadog's archive JSON is taken (each cloud's destination, its own integration's fields kept
    /// as given), kiln's `integration.cloud` asked for; one wrong is refused, saying what.
    #[test]
    fn archives_are_datadog_s_shape_and_are_checked() {
        let a: Archive = serde_json::from_value(serde_json::json!({
            "name": "all", "query": "service:web",
            "destination": { "type": "s3", "bucket": "my-bucket", "path": "/logs", "integration": { "cloud": "prod", "account_id": "123", "role_name": "dd" },
                "encryption": { "type": "SSE_KMS", "key": "arn:aws:kms:us-east-1:123:key/x" }, "storage_class": "STANDARD_IA" },
            "include_tags": true, "rehydration_tags": ["team:a"]
        }))
        .unwrap();
        assert!(a.check().is_ok(), "{:?}", a.check());
        let back = serde_json::to_value(&a).unwrap();
        assert_eq!(back["destination"]["integration"], serde_json::json!({ "cloud": "prod", "account_id": "123", "role_name": "dd" }));
        assert!(matches!(&a.destination, Destination::S3 { encryption: Some(e), .. } if e.kind == "SSE_KMS"));
        let gcs: Destination = serde_json::from_value(serde_json::json!({ "type": "gcs", "bucket": "b", "integration": { "cloud": "g" } })).unwrap();
        assert!(matches!(gcs, Destination::Gcs { .. }));
        let az: Destination = serde_json::from_value(serde_json::json!({ "type": "azure", "storage_account": "acct", "container": "c", "integration": { "cloud": "z" } })).unwrap();
        assert!(matches!(az, Destination::Azure { .. }));
        let with = |destination: Destination| Archive { destination, ..archive("", "x", "*") };
        let kms = |key: Option<&str>| Some(Encryption { kind: "SSE_KMS".into(), key: key.map(str::to_owned) });
        for (bad, says) in [
            (archive("", " ", "*"), "a name"),
            (archive("", "x", "(unclosed"), "its query"),
            (with(s3_at("logs", "", "", None, None)), "integration.cloud"),
            (with(s3_at("a b", "", "c", None, None)), "its bucket"),
            (with(s3_at("b", "/a/../b", "c", None, None)), "'..'"),
            (with(s3_at("b", "a\nb", "c", None, None)), "control"),
            (with(s3_at("b", "", "c", None, kms(None))), "SSE_KMS"),
            (with(s3_at("b", "", "c", None, kms(Some(" ")))), "SSE_KMS"),
            (with(s3_at("b", "", "c", None, Some(Encryption { kind: "AES".into(), key: None }))), "NO_OVERRIDE"),
            (with(Destination::Gcs { bucket: "b".into(), path: String::new(), integration: integration("c"), endpoint: Some("ftp://x".into()) }), "endpoint"),
        ] {
            let e = bad.check().unwrap_err();
            assert!(e.contains(says), "{e} (wanted {says})");
        }
    }

    /// Where a destination writes names it (its bucket, path and endpoint), not its credentials or
    /// storage class.
    #[test]
    fn a_destination_s_place_is_where_it_writes() {
        let base = s3_at("logs", "/kiln", "aws", None, None);
        assert_eq!(base.place(), s3_at("logs", "/kiln", "other", Some("GLACIER_IR"), None).place());
        assert_ne!(base.place(), s3_at("elsewhere", "/kiln", "aws", None, None).place());
        assert_ne!(base.place(), s3_at("logs", "/kiln/2", "aws", None, None).place());
    }

    fn rec(i: i64, service: &str, severity: u8) -> Record {
        Record {
            ts: 1_800_000_000_000_000_000 + i * 60_000_000_000,
            severity,
            service: service.into(),
            host: "h".into(),
            source: if i == 7 { kiln_obs::monitor::EVENTS.into() } else { "app".into() },
            message: format!("record {i}"),
            ..Record::default()
        }
    }

    /// A file of `blocks` (each sealed: its records sorted by time).
    fn file_of(dir: &Path, blocks: &[Vec<Record>]) -> LogFile {
        let sealed: Vec<kiln_obs::logs::block::LogBlock> = blocks.iter().map(|b| kiln_obs::logs::block::LogBlock::seal_owned(b)).collect();
        let refs: Vec<&kiln_obs::logs::block::LogBlock> = sealed.iter().collect();
        kiln_obs::logs::file::write_refs(&dir.join("logs-1.logs"), &refs).unwrap()
    }

    fn lines_of(gz: &[u8]) -> Vec<Value> {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut flate2::read::GzDecoder::new(gz), &mut text).unwrap();
        text.lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    /// The parts `encode` sends of `file`.
    fn parts(file: &LogFile, routes: &[Route], want: &[bool]) -> Vec<Part> {
        let (tx, rx) = async_channel::unbounded();
        encode(file, routes, want, FileKey::of(file), &tx).unwrap();
        drop(tx);
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    fn message_number(v: &Value) -> i64 {
        v["message"].as_str().unwrap().trim_start_matches("record ").parse().unwrap()
    }

    /// A file's records each go to the first archive whose search they match (none of kiln's own
    /// events, which no search names), each once, in parts of an hour's events lying in that hour;
    /// encoded again with fewer routes wanted, each wanted route's parts are the same to the byte.
    #[test]
    fn records_go_each_to_its_first_archive_once_split_alike_whatever_is_wanted() {
        let dir = tempfile::tempdir().unwrap();
        // 200 records a minute apart: three hours and more of them.
        let records: Vec<Record> = (0..200).map(|i| rec(i, ["web", "api", "db"][(i % 3) as usize], if i % 4 == 0 { 17 } else { 9 })).collect();
        let file = file_of(dir.path(), std::slice::from_ref(&records));
        let routes = routes_of(&[archive("a1", "errors", "status:error"), archive("a2", "web", "service:web"), archive("a3", "rest", "*")]);
        let all = parts(&file, &routes, &[true, true, true]);
        let numbers = |route: usize| -> Vec<i64> {
            let mut n: Vec<i64> = all.iter().filter(|p| p.route == route).flat_map(|p| lines_of(&p.body)).map(|v| message_number(&v)).collect();
            n.sort_unstable();
            n
        };
        let want = |keep: &dyn Fn(&Record) -> bool| (0..200).filter(|&i| records[i as usize].source != kiln_obs::monitor::EVENTS && keep(&records[i as usize])).collect::<Vec<i64>>();
        assert_eq!(numbers(0), want(&|r| r.severity >= 17));
        assert_eq!(numbers(1), want(&|r| r.severity < 17 && r.service == "web"));
        assert_eq!(numbers(2), want(&|r| r.severity < 17 && r.service != "web"));
        // Each part's events are of its hour, its newest event's time in it.
        for p in &all {
            let lines = lines_of(&p.body);
            assert_eq!(lines.len() as u64, p.events);
            for v in &lines {
                assert_eq!(archive::hour_of(records[message_number(v) as usize].ts), p.hour);
            }
            assert_eq!(p.newest_ms.div_euclid(3_600_000), p.hour);
        }
        assert!(all.iter().map(|p| p.hour).collect::<BTreeSet<_>>().len() >= 3, "three hours of records, three hours of parts");
        let ids: BTreeSet<String> = all.iter().flat_map(|p| lines_of(&p.body)).map(|v| v["_id"].as_str().unwrap().to_owned()).collect();
        assert_eq!(ids.len(), 199, "every record but kiln's own event, each its own _id");
        // Fewer wanted: the wanted route's parts are what they were, to the byte.
        let fewer = parts(&file, &routes, &[false, true, false]);
        assert!(fewer.iter().all(|p| p.route == 1));
        let of = |ps: &[Part]| ps.iter().filter(|p| p.route == 1).map(|p| (p.hour, p.number, p.events, p.newest_ms, p.body.clone())).collect::<Vec<_>>();
        assert_eq!(of(&fewer), of(&all));
    }

    /// Records across more hours than a route keeps parts open for (blocks overlapping in time): the
    /// one written to longest ago closes, and an hour come again is a part of its own (its next
    /// number), every record in one part of its hour.
    #[test]
    fn late_records_across_many_hours_close_parts_and_lose_none() {
        let dir = tempfile::tempdir().unwrap();
        // Ten blocks, each a record of every one of twelve hours.
        let blocks: Vec<Vec<Record>> = (0..10)
            .map(|b| {
                (0..12)
                    .map(|h| Record { ts: 1_800_000_000_000_000_000 + h * 3_600_000_000_000 + b * 1_000_000, message: format!("record {}", b * 12 + h), source: "app".into(), ..Record::default() })
                    .collect()
            })
            .collect();
        let file = file_of(dir.path(), &blocks);
        let routes = routes_of(&[archive("a", "all", "*")]);
        let got = parts(&file, &routes, &[true]);
        let mut seen: Vec<i64> = got.iter().flat_map(|p| lines_of(&p.body)).map(|v| message_number(&v)).collect();
        seen.sort_unstable();
        assert_eq!(seen, (0..120).collect::<Vec<i64>>());
        for p in &got {
            for v in lines_of(&p.body) {
                assert_eq!(archive::hour_of(blocks[(message_number(&v) / 12) as usize][(message_number(&v) % 12) as usize].ts), p.hour);
            }
        }
        let names: BTreeSet<(i64, u32)> = got.iter().map(|p| (p.hour, p.number)).collect();
        assert_eq!(names.len(), got.len(), "each part its own name");
        assert!(got.len() > 12, "parts closed for room and opened again: {}", got.len());
        assert_eq!(
            parts(&file, &routes, &[true]).iter().map(|p| (p.hour, p.number, p.body.clone())).collect::<Vec<_>>(),
            got.iter().map(|p| (p.hour, p.number, p.body.clone())).collect::<Vec<_>>(),
            "and alike again"
        );
    }

    fn key(n: u32) -> FileKey {
        FileKey { crc: n, bytes: u64::from(n) * 10, min_ts: i64::from(n), max_ts: i64::from(n) + 1 }
    }

    /// The archiver's state: a new archive begins after the newest file; a change of routing pins
    /// the files held that the old routing's archives want; a file's routing is kept while one of
    /// its archives wants it, and while it is newer than a listing that misses it; places follow
    /// their files through an install's numbering; parts and routings of files done with go.
    #[test]
    fn the_state_keeps_each_file_to_its_routing_and_each_place_to_its_file() {
        let mut st = State::default();
        let files: Vec<(u64, FileKey)> = (1..=3).map(|n| (u64::from(n), key(n))).collect();
        let a = archive("a", "A", "*");
        st.change(&[], std::slice::from_ref(&a), &files);
        assert_eq!(st.done.get("a"), Some(&Some(Place { seq: 3, file: key(3) })), "begins after the newest");
        assert!(!st.wants("a", 3) && st.wants("a", 4));
        // Files 4 and 5 come; B is made first in order: 4 and 5 keep A's routing.
        let files: Vec<(u64, FileKey)> = (1..=5).map(|n| (u64::from(n), key(n))).collect();
        let b = archive("b", "B", "service:web");
        st.change(std::slice::from_ref(&a), &[b.clone(), a.clone()], &files);
        let only_a = routes_of(std::slice::from_ref(&a));
        assert_eq!(st.routing(&key(4)), Some(&only_a));
        assert_eq!(st.routing(&key(5)), Some(&only_a));
        assert_eq!(st.routing(&key(3)), None, "A had it already");
        assert_eq!(st.done.get("b"), Some(&Some(Place { seq: 5, file: key(5) })));
        let both = [b.clone(), a.clone()];
        // A listing that misses 5 (taken before it was cut) keeps its pin.
        assert!(!st.follow(&both, &files[..4]));
        assert!(st.routing(&key(5)).is_some());
        // A writes 4: its pin goes, 5's stays.
        st.written.insert(Written { archive: "a".into(), file: key(4), hour: 0, part: 0, place: 1 });
        st.done.insert("a".into(), Some(Place { seq: 4, file: key(4) }));
        assert!(st.follow(&both, &files));
        assert_eq!(st.routing(&key(4)), None);
        assert!(st.routing(&key(5)).is_some());
        assert!(st.written.is_empty(), "parts of files done with go");
        // An install numbers them anew (from 10): places and pins follow.
        let renumbered: Vec<(u64, FileKey)> = (1..=5).map(|n| (u64::from(n) + 9, key(n))).collect();
        assert!(st.follow(&both, &renumbered));
        assert_eq!(st.done.get("a"), Some(&Some(Place { seq: 13, file: key(4) })));
        assert_eq!(st.done.get("b"), Some(&Some(Place { seq: 14, file: key(5) })));
        assert!(st.wants("a", 14) && !st.wants("b", 14));
        assert_eq!(st.begun.iter().find(|x| x.file == key(5)).map(|x| x.seq), Some(14));
        // A removed: its place goes, and 5's pin with it (no archive kept wants it).
        st.follow(std::slice::from_ref(&b), &renumbered);
        assert!(!st.done.contains_key("a"));
        assert_eq!(st.routing(&key(5)), None);
        assert!(st.routings.is_empty());
        // A file expired: its pin goes though an archive wanted it.
        st.pin(key(1), 10, &routes_of(std::slice::from_ref(&b)));
        st.done.insert("b".into(), Some(Place { seq: 9, file: key(9) }));
        assert!(st.follow(std::slice::from_ref(&b), &renumbered[1..]));
        assert_eq!(st.routing(&key(1)), None);
        // A place lost: every file held is wanted.
        st.done.clear();
        assert!(st.wants("b", 10));
    }

    /// Azure's date as RFC 7231 writes one.
    #[test]
    fn azure_s_date_is_rfc_7231_s() {
        assert_eq!(httpdate(1_526_394_676_003), "Tue, 15 May 2018 14:31:16 GMT");
    }
}
