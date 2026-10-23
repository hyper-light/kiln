//! Continuous profiles (`docs/observe.md` §8.11), Datadog's Continuous Profiler: what tracers'
//! profilers upload (`intake`: `multipart/form-data`, read by `multipart`), kept by the server
//! (`store`), its pprof read and summed by `kiln_obs::profile`.
//!
//! A tracer uploads to its node's Agent (`POST /profiling/v1/input` on the loopback,
//! `super::ddtrace`), which reads it (a tracer hears what is wrong with it), adds the tags an Agent
//! adds (its host, the tracer's container) and sends it on to the server over the observe lane
//! (`super::wire::Msg::Profile`), only while that lane has room: profiles yield to the group's own
//! messages. An Agent of Datadog's, or a tracer without one, uploads to the server's own intake
//! (`POST /api/v2/profile`, its key observe's ingest token). The server reads each again and keeps
//! it on a writer thread of its own, uploads taken in batches (one sync a batch), a bounded queue
//! before it: past it, an upload is refused (503 to whoever asked) and counted.

pub mod http;
pub mod intake;
pub mod multipart;
pub mod query;
pub mod store;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

// `::http`, the crate: `http` here is the API's module.
use ::http::{Method, StatusCode, header};
use serde_json::json;

use crate::http::server as srv;

/// An upload's bytes at the most as sent (a minute of a busy service's profiles is a few MiB), with
/// room under the observe lane's payload for what goes with it.
pub const MAX_UPLOAD: usize = super::wire::MAX_PAYLOAD - (64 << 10);
/// Bytes waiting for the writer at the most, and uploads.
const QUEUE_BYTES: u64 = 256 << 20;
const QUEUE_JOBS: usize = 1024;
/// A batch's uploads and bytes at the most.
const BATCH: usize = 32;
const BATCH_BYTES: u64 = 64 << 20;
/// How often the writer lets go of what the store's bounds say.
const RETAIN_EVERY: Duration = Duration::from_secs(60);
/// Room the store leaves on its disk: under it, its oldest segments are let go, and uploads
/// refused while that frees too little.
pub const RESERVE: u64 = 1 << 30;
/// How often a kind of trouble is said, at the most.
const SAY_EVERY: Duration = Duration::from_secs(60);

/// An upload to keep: as a runner sent it on (read again here), or read.
pub enum Job {
    Sent { content_type: String, body: Vec<u8>, tags: String, from: String },
    Read(intake::Upload),
}

impl Job {
    fn bytes(&self) -> u64 {
        match self {
            Self::Sent { body, .. } => body.len() as u64,
            Self::Read(u) => u.body.len() as u64,
        }
    }
}

/// What became of uploads since the server started.
#[derive(Debug, Default)]
pub struct Counts {
    pub kept: AtomicU64,
    /// Refused: the queue full, the disk short.
    pub busy: AtomicU64,
    pub disk: AtomicU64,
    /// Read again here and found wrong (a runner's), or past the store's bound.
    pub malformed: AtomicU64,
    /// Holding no pprof profile (Java's JFR recordings): not kept.
    pub not_pprof: AtomicU64,
    /// Lost to a write that failed.
    pub failed: AtomicU64,
}

/// The server's profiles: its store, its writer's queue.
pub struct Profiles {
    store: Arc<store::Store>,
    tx: SyncSender<Job>,
    queued: Arc<AtomicU64>,
    counts: Arc<Counts>,
}

static PROFILES: OnceLock<Profiles> = OnceLock::new();

/// The server's profiles, once they opened.
pub fn get() -> Option<&'static Profiles> {
    PROFILES.get()
}

/// Opens the server's store in `dir` on its writer's thread (its header files read), its uploads
/// kept `retention_ms` and its bytes at the most `max_bytes` (none: a quarter of its disk's); its
/// uploads taken once it has.
pub fn start(dir: PathBuf, retention_ms: i64, max_bytes: Option<u64>) {
    let spawned = std::thread::Builder::new().name("kiln-observe-profiles".into()).spawn(move || {
        let t = Instant::now();
        let store = match store::Store::open(&dir) {
            Ok(s) => Arc::new(s),
            Err(e) => {
                tracing::warn!(dir = %dir.display(), error = %e, "profiles did not open: uploads are refused");
                return;
            }
        };
        store.set_bounds(retention_ms, max_bytes.or_else(|| super::disk_total(&dir).map(|t| t / 4)).unwrap_or(u64::MAX));
        let st = store.stats();
        tracing::info!(uploads = st.uploads, segments = st.segments, bytes = st.bytes, ms = t.elapsed().as_millis() as u64, "profiles opened");
        let (tx, rx) = std::sync::mpsc::sync_channel(QUEUE_JOBS);
        let (queued, counts) = (Arc::new(AtomicU64::new(0)), Arc::new(Counts::default()));
        if PROFILES.set(Profiles { store: store.clone(), tx, queued: queued.clone(), counts: counts.clone() }).is_err() {
            return;
        }
        write_loop(&store, &rx, &queued, &counts);
    });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "profiles did not start: no thread");
    }
}

impl Profiles {
    /// `job` queued for the writer; false where the queue is full (refused, counted).
    pub fn offer(&self, job: Job) -> bool {
        let n = job.bytes();
        if self.queued.fetch_add(n, Ordering::AcqRel).saturating_add(n) > QUEUE_BYTES || self.tx.try_send(job).is_err() {
            self.queued.fetch_sub(n, Ordering::AcqRel);
            self.counts.busy.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        true
    }

    pub fn store(&self) -> &Arc<store::Store> {
        &self.store
    }

    pub fn counts(&self) -> &Counts {
        &self.counts
    }
}

/// Says a kind of trouble at most every [`SAY_EVERY`].
#[derive(Default)]
struct Said(Option<Instant>);

impl Said {
    fn due(&mut self) -> bool {
        if self.0.is_some_and(|t| t.elapsed() < SAY_EVERY) {
            return false;
        }
        self.0 = Some(Instant::now());
        true
    }
}

/// The writer: each batch read, checked against the disk's room, kept; the store's bounds applied
/// every [`RETAIN_EVERY`].
fn write_loop(store: &store::Store, rx: &Receiver<Job>, queued: &AtomicU64, counts: &Counts) {
    let mut retain_at = Instant::now();
    let (mut said_bad, mut said_jfr, mut said_disk, mut said_failed) = (Said::default(), Said::default(), Said::default(), Said::default());
    let mut batch: Vec<intake::Upload> = Vec::with_capacity(BATCH);
    loop {
        let first = match rx.recv_timeout(retain_at.saturating_duration_since(Instant::now())) {
            Ok(j) => Some(j),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        if Instant::now() >= retain_at {
            retain(store, 0);
            retain_at = Instant::now() + RETAIN_EVERY;
        }
        let Some(first) = first else { continue };
        let mut bytes = first.bytes();
        let mut jobs = vec![first];
        while jobs.len() < BATCH && bytes < BATCH_BYTES {
            let Ok(j) = rx.try_recv() else { break };
            bytes += j.bytes();
            jobs.push(j);
        }
        for job in jobs {
            let up = match job {
                Job::Read(up) => up,
                Job::Sent { content_type, body, tags, from } => match intake::read(&content_type, body) {
                    Ok(mut up) => {
                        up.add_tags(&tags);
                        up
                    }
                    Err(e) => {
                        counts.malformed.fetch_add(1, Ordering::Relaxed);
                        if said_bad.due() {
                            tracing::warn!(from = %from, error = %e, "a profile upload sent on does not read: not kept");
                        }
                        continue;
                    }
                },
            };
            if up.profiles.is_empty() {
                counts.not_pprof.fetch_add(1, Ordering::Relaxed);
                if said_jfr.due() {
                    tracing::warn!(family = %up.family, attachments = ?up.others, "a profile upload holds no pprof profile (Java's JFR recordings are not read): not kept");
                }
                continue;
            }
            batch.push(up);
        }
        let need: u64 = batch.iter().map(|u| u.body.len() as u64).sum();
        if !batch.is_empty() && super::disk_free(store.dir()).is_some_and(|f| f < RESERVE.saturating_add(need)) {
            retain(store, need);
            if super::disk_free(store.dir()).is_some_and(|f| f < RESERVE.saturating_add(need)) {
                counts.disk.fetch_add(batch.len() as u64, Ordering::Relaxed);
                if said_disk.due() {
                    tracing::warn!(uploads = batch.len(), "profile uploads refused: the disk has less than {} GiB free, and letting go of profiles freed too little", RESERVE >> 30);
                }
                batch.clear();
            }
        }
        if !batch.is_empty() {
            match store.put(&batch, super::clock_ms()) {
                Ok(p) => {
                    counts.kept.fetch_add(p.kept as u64, Ordering::Relaxed);
                    counts.malformed.fetch_add(p.refused as u64, Ordering::Relaxed);
                }
                Err(e) => {
                    counts.failed.fetch_add(batch.len() as u64, Ordering::Relaxed);
                    if said_failed.due() {
                        tracing::warn!(error = %e, uploads = batch.len(), "profile uploads not kept: the write failed");
                    }
                }
            }
            batch.clear();
        }
        queued.fetch_sub(bytes, Ordering::AcqRel);
    }
}

/// The store's bounds applied, and room made for `need` bytes more where the disk is short of its
/// reserve.
fn retain(store: &store::Store, need: u64) {
    let short = super::disk_free(store.dir()).map_or(0, |f| RESERVE.saturating_add(need).saturating_sub(f));
    match store.retain(super::clock_ms(), short) {
        Ok(r) if r.segments > 0 => tracing::info!(segments = r.segments, uploads = r.uploads, bytes = r.bytes, "profiles let go"),
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "profiles could not be let go"),
    }
}

/// An upload sent on, as the observe lane carries it: its body, then its content type and the tags
/// its Agent adds, then their lengths (so the body is taken back without a copy).
pub fn pack(mut body: Vec<u8>, content_type: &str, tags: &str) -> Vec<u8> {
    let (ct, tags) = (store::clip(content_type, 1024), store::clip(tags, 8192));
    body.reserve(ct.len() + tags.len() + 4);
    body.extend_from_slice(ct.as_bytes());
    body.extend_from_slice(tags.as_bytes());
    body.extend_from_slice(&u16::try_from(ct.len()).unwrap_or(0).to_le_bytes());
    body.extend_from_slice(&u16::try_from(tags.len()).unwrap_or(0).to_le_bytes());
    body
}

/// [`pack`] undone: the body, its content type and its tags.
pub fn unpack(mut payload: Vec<u8>) -> Option<(Vec<u8>, String, String)> {
    let n = payload.len();
    let lens = payload.get(n.checked_sub(4)?..)?;
    let (ct, tags) = (usize::from(u16::from_le_bytes([*lens.first()?, *lens.get(1)?])), usize::from(u16::from_le_bytes([*lens.get(2)?, *lens.get(3)?])));
    let body_len = n.checked_sub(4 + ct + tags)?;
    let content_type = std::str::from_utf8(payload.get(body_len..body_len + ct)?).ok()?.to_owned();
    let tags = std::str::from_utf8(payload.get(body_len + ct..body_len + ct + tags)?).ok()?.to_owned();
    payload.truncate(body_len);
    Some((payload, content_type, tags))
}

/// An upload a runner sent on, for the writer.
pub fn from_runner(machine: &str, payload: Vec<u8>) {
    let Some(p) = get() else { return };
    let Some((body, content_type, tags)) = unpack(payload) else {
        p.counts.malformed.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let _ = p.offer(Job::Sent { content_type, body, tags, from: machine.to_owned() });
}

fn answer(code: StatusCode, error: Option<&str>) -> srv::Response {
    let r = match error {
        Some(e) => srv::json(code, &json!({ "errors": [e] })),
        None => srv::json(code, &json!({})),
    };
    if code == StatusCode::SERVICE_UNAVAILABLE { srv::with_header(r, header::RETRY_AFTER, "1") } else { r }
}

/// `POST /api/v2/profile`, the server's intake (its key found good): an upload from an Agent of
/// Datadog's (the tags it adds, `X-Datadog-Additional-Tags`, added) or a tracer without one.
pub async fn datadog_intake(req: srv::Request) -> srv::Response {
    if req.method() != Method::POST {
        return answer(StatusCode::METHOD_NOT_ALLOWED, Some("Datadog's profile intake takes POST"));
    }
    let Some(p) = get() else { return answer(StatusCode::SERVICE_UNAVAILABLE, Some("profiles are kept by the server, and it is starting")) };
    let content_type = srv::header_str(&req, &header::CONTENT_TYPE).unwrap_or("").to_owned();
    let tags = req.headers().get("x-datadog-additional-tags").and_then(|v| v.to_str().ok()).unwrap_or("").to_owned();
    let Ok(body) = srv::body_bytes(req.into_body(), MAX_UPLOAD).await else {
        return answer(StatusCode::PAYLOAD_TOO_LARGE, Some(&format!("an upload is taken up to {} MiB", MAX_UPLOAD >> 20)));
    };
    let done = crate::rt::blocking(move || match intake::read(&content_type, Vec::from(body)) {
        Ok(mut up) => {
            up.add_tags(&tags);
            if p.offer(Job::Read(up)) { answer(StatusCode::ACCEPTED, None) } else { answer(StatusCode::SERVICE_UNAVAILABLE, Some("behind on writes: nothing was taken; try again")) }
        }
        Err(e) => answer(StatusCode::BAD_REQUEST, Some(&e.to_string())),
    })
    .await;
    done.unwrap_or_else(|e| answer(StatusCode::INTERNAL_SERVER_ERROR, Some(&format!("{e:#}"))))
}

/// `POST /profiling/v1/input`, a node's Agent's (no key asked, on the loopback): an upload read here
/// (a tracer hears what is wrong with it), the tags an Agent adds added (this node, as its host; the
/// tracer's container), and kept here where this is the server, else sent on to it.
pub async fn from_tracer(o: &super::Observe, host: &str, req: srv::Request) -> srv::Response {
    if req.method() != Method::POST {
        return answer(StatusCode::METHOD_NOT_ALLOWED, Some("the profile intake takes POST"));
    }
    let content_type = srv::header_str(&req, &header::CONTENT_TYPE).unwrap_or("").to_owned();
    // The tracer's container: dd-trace-go's `Datadog-Container-ID`, or libdatadog's
    // `Datadog-Entity-ID` of a container id (`ci-<id>`; one of a cgroup's inode, `in-<inode>`, names
    // none this reads).
    let header = |name: &str| req.headers().get(name).and_then(|v| v.to_str().ok()).map(str::trim).filter(|v| !v.is_empty());
    let container = header("datadog-container-id").or_else(|| header("datadog-entity-id").and_then(|e| e.strip_prefix("ci-"))).unwrap_or("").to_owned();
    let mut tags = format!("host:{host}");
    if !container.is_empty() && container.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.') {
        tags.push_str(",container_id:");
        tags.push_str(&container);
    }
    let Ok(body) = srv::body_bytes(req.into_body(), MAX_UPLOAD).await else {
        return answer(StatusCode::PAYLOAD_TOO_LARGE, Some(&format!("an upload is taken up to {} MiB", MAX_UPLOAD >> 20)));
    };
    let o = o.clone();
    let done = crate::rt::blocking(move || {
        let mut up = match intake::read(&content_type, Vec::from(body)) {
            Ok(up) => up,
            Err(e) => return answer(StatusCode::BAD_REQUEST, Some(&e.to_string())),
        };
        if up.profiles.is_empty() {
            // Nothing kiln reads (Java's JFR): taken, as an Agent would, and not sent on.
            return answer(StatusCode::ACCEPTED, None);
        }
        let taken = match get() {
            Some(p) => {
                up.add_tags(&tags);
                p.offer(Job::Read(up))
            }
            None => o.offer_profile(pack(std::mem::take(&mut up.body), &content_type, &tags)),
        };
        if taken { answer(StatusCode::ACCEPTED, None) } else { answer(StatusCode::SERVICE_UNAVAILABLE, Some("behind on sending profiles: nothing was taken")) }
    })
    .await;
    done.unwrap_or_else(|e| answer(StatusCode::INTERNAL_SERVER_ERROR, Some(&format!("{e:#}"))))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// An upload sent on comes back as it went: its body, its type, its tags.
    #[test]
    fn an_upload_sent_on_is_unpacked_as_it_was_packed() {
        let body = intake::tests::upload("b");
        let packed = pack(body.clone(), "multipart/form-data; boundary=b", "host:runner-1,container_id:abc");
        let (b, ct, tags) = unpack(packed).unwrap();
        assert_eq!((b, ct.as_str(), tags.as_str()), (body, "multipart/form-data; boundary=b", "host:runner-1,container_id:abc"));
        assert_eq!(unpack(vec![1, 0]), None);
        assert_eq!(unpack(vec![9, 9, 200, 0, 0, 0]), None, "lengths past the payload");
    }
}
