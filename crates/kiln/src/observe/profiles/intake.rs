//! A profiler's upload (`docs/observe.md` §8.11), as Datadog's tracers send it: `multipart/form-data`
//! of an `event` part (`event.json`) and a part a profile (pprof, gzip'd, zstd'd or in LZ4's frames:
//! told by its first bytes, nothing in the request says which). Read and checked before anything is
//! kept: its times, its profiler's tags, each profile inflated (bounded), read as pprof and its
//! values summed; its bytes kept as they came, in the body the upload holds.

use std::ops::Range;

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

/// An upload's `event.json`. libdatadog's exporter (under Python's, Ruby's, PHP's and .NET's
/// tracers) says `null` of what it has none of (`endpoint_counts`): each optional field is read
/// as none either way.
#[derive(Deserialize)]
struct Event {
    start: String,
    end: String,
    #[serde(default)]
    attachments: Option<Vec<String>>,
    #[serde(default)]
    tags_profiler: Option<String>,
    #[serde(default)]
    family: Option<String>,
    #[serde(default)]
    endpoint_counts: Option<std::collections::BTreeMap<String, u64>>,
}

/// How an attachment's bytes came, told by their first bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Raw,
    Gzip,
    Zstd,
    /// LZ4's frame format.
    Lz4,
}

impl Encoding {
    pub fn of(b: &[u8]) -> Self {
        if b.starts_with(&[0x1f, 0x8b]) {
            Self::Gzip
        } else if b.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
            Self::Zstd
        } else if b.starts_with(&kiln_obs::lz4::FRAME_MAGIC.to_le_bytes()) {
            Self::Lz4
        } else {
            Self::Raw
        }
    }
}

/// A pprof attachment, read: its name; its sample types (type, unit) and each one's values summed
/// over its samples; how its bytes came, where they are in the upload's body, and their length
/// inflated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub name: String,
    pub types: Vec<(String, String)>,
    pub totals: Vec<i64>,
    pub encoding: Encoding,
    pub sent: Range<usize>,
    pub plain_len: usize,
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
    /// Each pprof attachment.
    pub profiles: Vec<Attachment>,
    /// Attachments not pprof (Go's `metrics.json`, an execution trace, Java's JFR): their names,
    /// not kept.
    pub others: Vec<String>,
    /// The request's body, every attachment's bytes in it.
    pub body: Vec<u8>,
}

impl Upload {
    /// Its tag `key`'s value, the first if it has several.
    pub fn tag(&self, key: &str) -> Option<&str> {
        self.tags.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// Attachment `a`'s bytes as they came.
    pub fn sent(&self, a: &Attachment) -> &[u8] {
        self.body.get(a.sent.clone()).unwrap_or_default()
    }

    /// Tags another said of it (an Agent's `X-Datadog-Additional-Tags`, a runner's: `host:…`,
    /// `container_id:…`), comma-separated: each whose key it has not added.
    pub fn add_tags(&mut self, extra: &str) {
        for (k, v) in split_tags(extra) {
            if self.tag(&k).is_none() {
                self.tags.push((k, v));
            }
        }
    }
}

/// Tags as Datadog's profilers write them (`k:v,k:v`), each split at its first colon.
fn split_tags(s: &str) -> impl Iterator<Item = (String, String)> + '_ {
    s.split(',').map(str::trim).filter(|t| !t.is_empty()).map(|t| match t.split_once(':') {
        Some((k, v)) => (k.trim().to_owned(), v.trim().to_owned()),
        None => (t.to_owned(), String::new()),
    })
}

/// `body` (of type `content_type`) read and checked.
pub fn read(content_type: &str, body: Vec<u8>) -> Result<Upload, IntakeError> {
    let boundary = multipart::boundary(content_type).ok_or_else(|| IntakeError::ContentType(content_type.chars().take(80).collect()))?;
    // The parts read in place: the event, and where each attachment it names is.
    let (ev, found) = {
        let parts = multipart::parts(&body, boundary)?;
        let event = parts.iter().find(|p| p.name == "event").ok_or(IntakeError::NoEvent)?;
        let ev: Event = serde_json::from_slice(event.body).map_err(|e| IntakeError::Event(e.to_string()))?;
        let attachments = ev.attachments.as_deref().unwrap_or_default();
        if attachments.len() > MAX_ATTACHMENTS {
            return Err(IntakeError::Attachments);
        }
        // Each attachment the event names, once, in its order; a part the event does not name,
        // passed over (an older profiler's: `event` names them all).
        let mut found: Vec<(String, Range<usize>)> = Vec::with_capacity(attachments.len());
        for name in attachments {
            if name == "event" || found.iter().any(|(n, _)| n == name) {
                continue;
            }
            if let Some(p) = parts.iter().find(|p| p.name == name.as_str()) {
                found.push((name.clone(), p.at..p.at + p.body.len()));
            }
        }
        (ev, found)
    };
    let time = |s: &str, what: &'static str| -> Result<i64, IntakeError> {
        let t = time::OffsetDateTime::parse(s.trim(), &time::format_description::well_known::Rfc3339).map_err(|_| IntakeError::Time(what))?;
        i64::try_from(t.unix_timestamp_nanos()).map_err(|_| IntakeError::Time(what))
    };
    let (start_ns, end_ns) = (time(&ev.start, "start")?, time(&ev.end, "end")?);
    if end_ns < start_ns {
        return Err(IntakeError::Times);
    }
    let mut up = Upload {
        start_ns,
        end_ns,
        family: ev.family.unwrap_or_default(),
        tags: split_tags(ev.tags_profiler.as_deref().unwrap_or_default()).collect(),
        endpoint_counts: ev.endpoint_counts.unwrap_or_default().into_iter().collect(),
        profiles: Vec::with_capacity(found.len()),
        others: Vec::new(),
        body: Vec::new(),
    };
    // One buffer inflates every attachment in turn.
    let mut plain = Vec::new();
    for (name, sent) in found {
        // What is plainly not pprof (Go's metrics.json, an execution trace, Java's JFR) is named and
        // not kept; any other attachment is read as pprof, whatever its name (libdatadog's are
        // not all `.pprof`), and one named `.pprof` that does not read refuses the upload.
        if [".json", ".jfr", ".trace", ".txt"].iter().any(|x| name.ends_with(x)) {
            up.others.push(name);
            continue;
        }
        let bytes = body.get(sent.clone()).unwrap_or_default();
        let encoding = Encoding::of(bytes);
        let fail = |e: String| IntakeError::Profile(name.clone(), e);
        let read = match encoding {
            Encoding::Raw if bytes.len() > MAX_PROFILE => Err(format!("past {MAX_PROFILE} bytes")),
            Encoding::Raw => kiln_obs::profile::pprof::read(bytes).map_err(|e| e.to_string()),
            _ => inflate(bytes, &mut plain).and_then(|()| kiln_obs::profile::pprof::read(&plain).map_err(|e| e.to_string())),
        };
        let p = match read {
            Ok(p) => p,
            Err(e) if name.ends_with(".pprof") => return Err(fail(e)),
            Err(_) => {
                up.others.push(name);
                continue;
            }
        };
        let mut totals = vec![0i64; p.sample_types.len()];
        for s in &p.samples {
            for (t, v) in totals.iter_mut().zip(p.values_of(s)) {
                *t = t.saturating_add(*v);
            }
        }
        let types = p.sample_types.iter().map(|t| (t.kind.to_owned(), t.unit.to_owned())).collect();
        let plain_len = if encoding == Encoding::Raw { bytes.len() } else { plain.len() };
        up.profiles.push(Attachment { name, types, totals, encoding, sent, plain_len });
    }
    up.body = body;
    Ok(up)
}

/// A profile's bytes inflated into `out` (cleared first) as their first bytes say: gzip, zstd or
/// LZ4's frames; else as they are; never past [`MAX_PROFILE`].
pub fn inflate(b: &[u8], out: &mut Vec<u8>) -> Result<(), String> {
    use std::io::Read as _;
    out.clear();
    match Encoding::of(b) {
        Encoding::Gzip => {
            flate2::read::GzDecoder::new(b).take(MAX_PROFILE as u64 + 1).read_to_end(out).map_err(|e| format!("not gzip: {e}"))?;
            if out.len() > MAX_PROFILE {
                return Err(format!("past {MAX_PROFILE} bytes inflated"));
            }
        }
        Encoding::Zstd => kiln_obs::zstd::decompress(b, out, MAX_PROFILE).map_err(|e| format!("not zstd: {e}"))?,
        Encoding::Lz4 => kiln_obs::lz4::decompress_frames(b, out, MAX_PROFILE).map_err(|e| format!("not LZ4's frames: {e}"))?,
        Encoding::Raw if b.len() > MAX_PROFILE => return Err(format!("past {MAX_PROFILE} bytes")),
        Encoding::Raw => out.extend_from_slice(b),
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
pub(super) mod tests {
    use super::*;

    pub(in crate::observe::profiles) const CPU: &[u8] = include_bytes!("../../../../kiln-obs/src/fixtures/pprof/go-cpu.pprof");
    pub(in crate::observe::profiles) const HEAP: &[u8] = include_bytes!("../../../../kiln-obs/src/fixtures/pprof/go-heap.pprof");

    /// An upload as dd-trace-go writes one (`profiler/upload.go`): each profile a part named as its
    /// file, then `event` (its `start`, `end`, `attachments`, `tags_profiler`, `family`, `version`
    /// 4, `endpoint_counts`); the profiles Go's own (`kiln_obs`'s fixtures), gzip'd as sent.
    pub(in crate::observe::profiles) fn upload(boundary: &str) -> Vec<u8> {
        upload_of(
            boundary,
            &[("cpu.pprof", CPU), ("delta-heap.pprof", HEAP), ("metrics.json", b"[]")],
            "2026-10-23T17:39:00.9Z",
            "env:prod,service:checkout,profile_seq:7,runtime:go,version:1.4.2,host:web-1",
        )
    }

    /// An upload of `files`, its event's start `start` (its end a minute on) and its tags `tags`.
    pub(in crate::observe::profiles) fn upload_of(boundary: &str, files: &[(&str, &[u8])], start: &str, tags: &str) -> Vec<u8> {
        let mut body = Vec::new();
        let mut part = |name: &str, ct: &str, bytes: &[u8]| {
            body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"{name}\"\r\nContent-Type: {ct}\r\n\r\n").as_bytes());
            body.extend_from_slice(bytes);
            body.extend_from_slice(b"\r\n");
        };
        for (name, bytes) in files {
            part(name, "application/octet-stream", bytes);
        }
        let t = time::OffsetDateTime::parse(start, &time::format_description::well_known::Rfc3339).unwrap();
        let end = (t + time::Duration::minutes(1)).format(&time::format_description::well_known::Rfc3339).unwrap();
        let names: Vec<String> = files.iter().map(|(n, _)| format!("{n:?}")).collect();
        let event = format!(r#"{{"start":"{start}","end":"{end}","attachments":[{}],"tags_profiler":"{tags}","family":"go","version":"4","endpoint_counts":{{"GET /cart":120}}}}"#, names.join(","));
        part("event", "application/json", event.as_bytes());
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        body
    }

    /// Its times, tags, endpoints; each pprof attachment read (its types, its values summed) and
    /// found in the body as it came; `metrics.json` named and not kept.
    #[test]
    fn a_go_profiler_s_upload_is_read() {
        let up = read("multipart/form-data; boundary=b0und", upload("b0und")).unwrap();
        assert_eq!((up.start_ns, up.end_ns - up.start_ns), (1_792_777_140_900_000_000, 60_000_000_000));
        assert_eq!((up.family.as_str(), up.tag("service"), up.tag("env"), up.tag("profile_seq")), ("go", Some("checkout"), Some("prod"), Some("7")));
        assert_eq!(up.endpoint_counts, [("GET /cart".to_owned(), 120)]);
        assert_eq!(up.profiles.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), ["cpu.pprof", "delta-heap.pprof"]);
        assert_eq!(up.others, ["metrics.json"]);
        let (cpu, heap) = (&up.profiles[0], &up.profiles[1]);
        assert_eq!(cpu.types, [("samples".to_owned(), "count".to_owned()), ("cpu".to_owned(), "nanoseconds".to_owned())]);
        assert_eq!(cpu.totals, [150, 1_500_000_000]);
        assert_eq!((cpu.encoding, up.sent(cpu)), (Encoding::Gzip, CPU));
        assert_eq!(heap.types.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>(), ["alloc_objects", "alloc_space", "inuse_objects", "inuse_space"]);
        assert_eq!(heap.totals, [10_503, 13_105_703, 9_786, 10_278_807]);
        assert_eq!(heap.plain_len, 7_494);
    }

    /// The same profile zstd'd, in LZ4's frames, or as it is, read alike.
    #[test]
    fn a_profile_however_it_came_is_read_alike() {
        let mut plain = Vec::new();
        inflate(HEAP, &mut plain).unwrap();
        let zstd = include_bytes!("../../../../kiln-obs/src/fixtures/pprof/go-heap.pprof.zst");
        let lz4 = include_bytes!("../../../../kiln-obs/src/fixtures/pprof/go-heap.pprof.lz4");
        for (bytes, encoding) in [(&zstd[..], Encoding::Zstd), (&lz4[..], Encoding::Lz4), (&plain[..], Encoding::Raw)] {
            let body = upload_of("b", &[("heap.pprof", bytes)], "2026-10-23T17:39:00Z", "service:a");
            let up = read("multipart/form-data; boundary=b", body).unwrap();
            let a = &up.profiles[0];
            assert_eq!((a.encoding, a.totals.as_slice(), a.plain_len, up.sent(a)), (encoding, &[10_503, 13_105_703, 9_786, 10_278_807][..], 7_494, bytes));
        }
    }

    /// An attachment whatever its name that reads as pprof is kept (libdatadog's need not end in
    /// `.pprof`); one that does not, of another name, is named and passed over.
    #[test]
    fn a_profile_of_another_name_is_read_and_anything_else_passed_over() {
        let body =
            upload_of("b", &[("auto.pprof.zst", include_bytes!("../../../../kiln-obs/src/fixtures/pprof/go-heap.pprof.zst")), ("notes.bin", b"not a profile")], "2026-10-23T17:39:00Z", "service:a");
        let up = read("multipart/form-data; boundary=b", body).unwrap();
        assert_eq!((up.profiles.len(), up.profiles[0].name.as_str(), up.others.as_slice()), (1, "auto.pprof.zst", &["notes.bin".to_owned()][..]));
    }

    /// An upload as ddtrace 4.16 (Python's; libdatadog 1.0.0's exporter) sent it, captured on the
    /// loopback (`fixtures/ddtrace-py-4.16-profile.multipart`): its `endpoint_counts` null, its
    /// profile zstd'd, of 19 sample types, `code-provenance.json` beside it. Read: its times to
    /// the nanosecond, its tags, its profile's types; its CPU time summed, the app's own hashing
    /// loop most of it.
    #[test]
    fn a_python_profiler_s_upload_through_libdatadog_is_read() {
        let body = include_bytes!("../fixtures/ddtrace-py-4.16-profile.multipart").to_vec();
        let up = read("multipart/form-data; boundary=d0ef18efbee87c53-3e8c5bb69c00847f-ca79f11e12eaf4a3-7d6369855f018300", body).unwrap();
        assert_eq!((up.family.as_str(), up.tag("service"), up.tag("language"), up.endpoint_counts.len()), ("python", Some("kiln-pycheck"), Some("python"), 0));
        assert_eq!(up.end_ns - up.start_ns, 5_099_889_063, "2026-10-23T19:18:30.604819706Z to 19:18:35.704708769Z");
        assert_eq!(up.others, ["code-provenance.json"]);
        let a = &up.profiles[0];
        assert_eq!((a.name.as_str(), a.encoding, a.types.len(), a.types[0].0.as_str(), a.types[0].1.as_str()), ("profile.pprof", Encoding::Zstd, 19, "cpu-time", "nanoseconds"));
        let mut plain = Vec::new();
        inflate(up.sent(a), &mut plain).unwrap();
        let p = kiln_obs::profile::pprof::read(&plain).unwrap();
        let mut g = kiln_obs::profile::flame::Flame::new();
        g.add(&p, 0);
        let (whole, by) = (g.nodes[0].total, g.by_function());
        let burn = by.iter().find(|(f, _, _)| g.name(*f).0 == "burn").map_or(0, |b| b.2);
        assert!(whole > 0 && burn * 2 > whole, "burn {burn} of {whole}");
        assert_eq!(a.totals[0], whole, "its CPU time summed at intake as in the graph");
    }

    /// Another's tags added where the upload has none of their keys: its own `host` kept.
    #[test]
    fn an_agent_s_tags_add_what_the_profiler_did_not_say() {
        let mut up = read("multipart/form-data; boundary=b", upload("b")).unwrap();
        up.add_tags("host:runner-3, container_id:abc123,default_env:none");
        assert_eq!((up.tag("host"), up.tag("container_id"), up.tag("default_env")), (Some("web-1"), Some("abc123"), Some("none")));
    }

    /// Refused, saying why: another type; no event; a time that is not one; an end before its
    /// start; a profile that is not pprof.
    #[test]
    fn a_malformed_upload_is_refused() {
        assert!(matches!(read("application/json", b"{}".to_vec()), Err(IntakeError::ContentType(_))));
        let no_event = b"--b\r\nContent-Disposition: form-data; name=\"cpu.pprof\"\r\n\r\nx\r\n--b--".to_vec();
        assert!(matches!(read("multipart/form-data; boundary=b", no_event), Err(IntakeError::NoEvent)));
        let event = |json: &str, extra: &[u8]| {
            let mut b = extra.to_vec();
            b.extend_from_slice(format!("--b\r\nContent-Disposition: form-data; name=\"event\"\r\n\r\n{json}\r\n--b--").as_bytes());
            b
        };
        assert!(matches!(read("multipart/form-data; boundary=b", event(r#"{"start":"yesterday","end":"2026-10-23T17:40:00Z"}"#, b"")), Err(IntakeError::Time("start"))));
        assert!(matches!(read("multipart/form-data; boundary=b", event(r#"{"start":"2026-10-23T17:40:00Z","end":"2026-10-23T17:39:00Z"}"#, b"")), Err(IntakeError::Times)));
        let bad = event(
            r#"{"start":"2026-10-23T17:39:00Z","end":"2026-10-23T17:40:00Z","attachments":["cpu.pprof"]}"#,
            b"--b\r\nContent-Disposition: form-data; name=\"cpu.pprof\"\r\n\r\n\x1f\x8bnot gzip\r\n",
        );
        assert!(matches!(read("multipart/form-data; boundary=b", bad), Err(IntakeError::Profile(n, _)) if n == "cpu.pprof"));
    }
}
