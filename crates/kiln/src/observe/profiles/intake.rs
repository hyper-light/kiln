//! A profiler's upload (`docs/observe.md` §8.11), as Datadog's tracers send it: `multipart/form-data`
//! of an `event` part (`event.json`) and a part a profile (pprof, gzip'd, or zstd'd: told by its
//! first bytes, nothing in the request says which). Read and checked before anything is kept: its
//! times, its profiler's tags, each profile inflated (bounded) and read as pprof.

use serde::Deserialize;

use super::multipart::{self, MultipartError};

/// A profile inflated, at the most (a minute of a busy service's CPU profile is some hundreds of
/// KiB; a heap profile of a large one a few MiB).
pub const MAX_PROFILE: usize = 64 << 20;
/// Attachments an upload may hold, at the most (Go's profiler sends seven types at the most).
pub const MAX_ATTACHMENTS: usize = 16;

#[derive(Debug, thiserror::Error)]
pub enum IntakeError {
    #[error("not multipart/form-data with a boundary ({0:?})")]
    ContentType(String),
    #[error(transparent)]
    Multipart(#[from] MultipartError),
    #[error("no `event` part (event.json)")]
    NoEvent,
    #[error("event.json: {0}")]
    Event(String),
    #[error("event.json's {0}: not an RFC 3339 time")]
    Time(&'static str),
    #[error("an upload's end before its start")]
    Times,
    #[error("{0}: {1}")]
    Profile(String, String),
    #[error("more than {MAX_ATTACHMENTS} attachments")]
    Attachments,
}

#[derive(Deserialize)]
struct Event {
    start: String,
    end: String,
    #[serde(default)]
    attachments: Vec<String>,
    #[serde(default)]
    tags_profiler: String,
    #[serde(default)]
    family: String,
    #[serde(default)]
    endpoint_counts: std::collections::BTreeMap<String, u64>,
}

/// An upload read: what Datadog calls a profile, its types collected about the same time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upload {
    pub start_ns: i64,
    pub end_ns: i64,
    /// Its profiler's language (`go`, `python`, `java`, …).
    pub family: String,
    /// Its profiler's tags, each split at its first colon, in their order (`service`, `env`,
    /// `version`, `host`, `runtime`, `profile_seq`, …); a tag without a colon, its value empty.
    pub tags: Vec<(String, String)>,
    /// Requests by endpoint while it was collected.
    pub endpoint_counts: Vec<(String, u64)>,
    /// Each pprof attachment: its name and its bytes inflated (each read whole once to check it).
    pub profiles: Vec<(String, Vec<u8>)>,
    /// Attachments not pprof (Go's `metrics.json`, an execution trace): their names, not kept.
    pub others: Vec<String>,
}

impl Upload {
    /// Its tag `key`'s value, the first if it has several.
    pub fn tag(&self, key: &str) -> Option<&str> {
        self.tags.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}

/// `body` (of type `content_type`) read and checked.
pub fn read(content_type: &str, body: &[u8]) -> Result<Upload, IntakeError> {
    let boundary = multipart::boundary(content_type).ok_or_else(|| IntakeError::ContentType(content_type.chars().take(80).collect()))?;
    let parts = multipart::parts(body, boundary)?;
    let event = parts.iter().find(|p| p.name == "event").ok_or(IntakeError::NoEvent)?;
    let ev: Event = serde_json::from_slice(event.body).map_err(|e| IntakeError::Event(e.to_string()))?;
    let time = |s: &str, what: &'static str| -> Result<i64, IntakeError> {
        let t = time::OffsetDateTime::parse(s.trim(), &time::format_description::well_known::Rfc3339).map_err(|_| IntakeError::Time(what))?;
        i64::try_from(t.unix_timestamp_nanos()).map_err(|_| IntakeError::Time(what))
    };
    let (start_ns, end_ns) = (time(&ev.start, "start")?, time(&ev.end, "end")?);
    if end_ns < start_ns {
        return Err(IntakeError::Times);
    }
    let tags = ev
        .tags_profiler
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(|t| match t.split_once(':') {
            Some((k, v)) => (k.trim().to_owned(), v.trim().to_owned()),
            None => (t.to_owned(), String::new()),
        })
        .collect();
    let mut up = Upload { start_ns, end_ns, family: ev.family, tags, endpoint_counts: ev.endpoint_counts.into_iter().collect(), profiles: Vec::new(), others: Vec::new() };
    // Each attachment the event names, in its order; a part the event does not name, passed over
    // (an older profiler's: `event` names them all).
    if ev.attachments.len() > MAX_ATTACHMENTS {
        return Err(IntakeError::Attachments);
    }
    for name in &ev.attachments {
        let Some(part) = parts.iter().find(|p| p.name == name.as_str() && p.name != "event") else { continue };
        if !name.ends_with(".pprof") {
            up.others.push(name.clone());
            continue;
        }
        let plain = inflate(part.body).map_err(|e| IntakeError::Profile(name.clone(), e))?;
        kiln_obs::profile::pprof::read(&plain).map_err(|e| IntakeError::Profile(name.clone(), e.to_string()))?;
        up.profiles.push((name.clone(), plain));
    }
    Ok(up)
}

/// A profile's bytes inflated as their first bytes say: gzip (`1f 8b`), zstd (`28 b5 2f fd`), else
/// as they are; never past [`MAX_PROFILE`].
pub fn inflate(b: &[u8]) -> Result<Vec<u8>, String> {
    use std::io::Read as _;
    if b.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(b).take(MAX_PROFILE as u64 + 1).read_to_end(&mut out).map_err(|e| format!("not gzip: {e}"))?;
        if out.len() > MAX_PROFILE {
            return Err(format!("past {MAX_PROFILE} bytes inflated"));
        }
        return Ok(out);
    }
    if b.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        let mut out = Vec::new();
        kiln_obs::zstd::decompress(b, &mut out, MAX_PROFILE).map_err(|e| format!("not zstd: {e}"))?;
        return Ok(out);
    }
    if b.len() > MAX_PROFILE {
        return Err(format!("past {MAX_PROFILE} bytes"));
    }
    Ok(b.to_vec())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// An upload as dd-trace-go writes one (`profiler/upload.go`): each profile a part named as its
    /// file, then `event` (its `start`, `end`, `attachments`, `tags_profiler`, `family`, `version`
    /// 4, `endpoint_counts`); the profiles Go's own (`kiln_obs`'s fixtures), gzip'd as sent.
    pub(super) fn upload(boundary: &str) -> Vec<u8> {
        let cpu = include_bytes!("../../../../kiln-obs/src/fixtures/pprof/go-cpu.pprof");
        let heap = include_bytes!("../../../../kiln-obs/src/fixtures/pprof/go-heap.pprof");
        let mut body = Vec::new();
        let mut part = |name: &str, ct: &str, bytes: &[u8]| {
            body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"{name}\"\r\nContent-Type: {ct}\r\n\r\n").as_bytes());
            body.extend_from_slice(bytes);
            body.extend_from_slice(b"\r\n");
        };
        part("cpu.pprof", "application/octet-stream", cpu);
        part("delta-heap.pprof", "application/octet-stream", heap);
        part("metrics.json", "application/octet-stream", b"[]");
        let event = r#"{"start":"2026-10-23T17:39:00.9Z","end":"2026-10-23T17:40:00.9Z","attachments":["cpu.pprof","delta-heap.pprof","metrics.json"],"tags_profiler":"env:prod,service:checkout,profile_seq:7,runtime:go,version:1.4.2,host:web-1","family":"go","version":"4","endpoint_counts":{"GET /cart":120}}"#;
        part("event", "application/json", event.as_bytes());
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        body
    }

    /// Its times, tags, endpoints; each pprof attachment inflated and kept, `metrics.json` named
    /// and not kept.
    #[test]
    fn a_go_profiler_s_upload_is_read() {
        let up = read("multipart/form-data; boundary=b0und", &upload("b0und")).unwrap();
        assert_eq!((up.start_ns, up.end_ns - up.start_ns), (1_792_777_140_900_000_000, 60_000_000_000));
        assert_eq!((up.family.as_str(), up.tag("service"), up.tag("env"), up.tag("profile_seq")), ("go", Some("checkout"), Some("prod"), Some("7")));
        assert_eq!(up.endpoint_counts, [("GET /cart".to_owned(), 120)]);
        assert_eq!(up.profiles.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), ["cpu.pprof", "delta-heap.pprof"]);
        assert_eq!(up.others, ["metrics.json"]);
        let cpu = kiln_obs::profile::pprof::read(&up.profiles[0].1).unwrap();
        assert_eq!(cpu.samples.iter().map(|s| cpu.values_of(s)[1]).sum::<i64>(), 1_500_000_000);
    }

    /// Refused, saying why: another type; no event; a time that is not one; an end before its
    /// start; a profile that is not pprof.
    #[test]
    fn a_malformed_upload_is_refused() {
        assert!(matches!(read("application/json", b"{}"), Err(IntakeError::ContentType(_))));
        let no_event = b"--b\r\nContent-Disposition: form-data; name=\"cpu.pprof\"\r\n\r\nx\r\n--b--";
        assert!(matches!(read("multipart/form-data; boundary=b", no_event), Err(IntakeError::NoEvent)));
        let event = |json: &str, extra: &[u8]| {
            let mut b = extra.to_vec();
            b.extend_from_slice(format!("--b\r\nContent-Disposition: form-data; name=\"event\"\r\n\r\n{json}\r\n--b--").as_bytes());
            b
        };
        assert!(matches!(read("multipart/form-data; boundary=b", &event(r#"{"start":"yesterday","end":"2026-10-23T17:40:00Z"}"#, b"")), Err(IntakeError::Time("start"))));
        assert!(matches!(read("multipart/form-data; boundary=b", &event(r#"{"start":"2026-10-23T17:40:00Z","end":"2026-10-23T17:39:00Z"}"#, b"")), Err(IntakeError::Times)));
        let bad = event(
            r#"{"start":"2026-10-23T17:39:00Z","end":"2026-10-23T17:40:00Z","attachments":["cpu.pprof"]}"#,
            b"--b\r\nContent-Disposition: form-data; name=\"cpu.pprof\"\r\n\r\n\x1f\x8bnot gzip\r\n",
        );
        assert!(matches!(read("multipart/form-data; boundary=b", &bad), Err(IntakeError::Profile(n, _)) if n == "cpu.pprof"));
    }
}
