//! The Docker Engine API (`docs/observe.md` §10.10), as Docker and Podman serve it on a node: each
//! running container's name, image, labels and start (`GET /containers/json`), and, where the node's
//! cgroups are not seen (Docker Desktop's VM, on macOS and Windows), its stats
//! (`GET /containers/{id}/stats?stream=false&one-shot=true`). HTTP/1.0 over the engine's Unix socket
//! (or Windows' named pipe), with std alone: to a 1.0 request the engine answers whole, not chunked,
//! and closes.

use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use super::cgroup::{Counters, Stats};

/// A request's wait at the most, for its connection and each read: the engine answers in
/// milliseconds, and one that hangs holds only the check's own thread.
pub const TIMEOUT: Duration = Duration::from_secs(2);
/// An answer's bytes at the most (a list of a thousand containers is some 4 MiB).
pub const ANSWER: u64 = 16 << 20;

/// Where the engine answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    Unix(PathBuf),
    Pipe(String),
}

impl Endpoint {
    /// The engine on this node: `DOCKER_HOST`'s (`unix://`, `npipe://`), else Docker's, Podman's
    /// (rootful, then this user's), Docker Desktop's on macOS, Windows' pipe; the first that is there.
    pub fn find() -> Option<Self> {
        if let Ok(h) = std::env::var("DOCKER_HOST") {
            if let Some(p) = h.strip_prefix("unix://") {
                return Some(Self::Unix(PathBuf::from(p)));
            }
            if let Some(p) = h.strip_prefix("npipe://") {
                return Some(Self::Pipe(p.replace('/', "\\")));
            }
        }
        if cfg!(windows) {
            return Some(Self::Pipe(r"\\.\pipe\docker_engine".to_owned()));
        }
        let mut places = vec![PathBuf::from("/var/run/docker.sock"), PathBuf::from("/run/podman/podman.sock")];
        if let Ok(run) = std::env::var("XDG_RUNTIME_DIR") {
            places.push(PathBuf::from(run).join("podman/podman.sock"));
        }
        if let Some(home) = std::env::var_os("HOME") {
            places.push(PathBuf::from(home).join(".docker/run/docker.sock"));
        }
        places.into_iter().find(|p| p.exists()).map(Self::Unix)
    }
}

#[cfg(unix)]
fn exchange(e: &Endpoint, request: &[u8]) -> std::io::Result<Vec<u8>> {
    let Endpoint::Unix(path) = e else { return Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "a named pipe is Windows'")) };
    let mut s = std::os::unix::net::UnixStream::connect(path)?;
    s.set_read_timeout(Some(TIMEOUT))?;
    s.set_write_timeout(Some(TIMEOUT))?;
    s.write_all(request)?;
    let mut out = Vec::new();
    s.take(ANSWER).read_to_end(&mut out)?;
    Ok(out)
}

#[cfg(windows)]
fn exchange(e: &Endpoint, request: &[u8]) -> std::io::Result<Vec<u8>> {
    let Endpoint::Pipe(name) = e else { return Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "a Unix socket is not Windows'")) };
    let mut f = std::fs::OpenOptions::new().read(true).write(true).open(name)?;
    f.write_all(request)?;
    let mut out = Vec::new();
    f.take(ANSWER).read_to_end(&mut out)?;
    Ok(out)
}

#[cfg(not(any(unix, windows)))]
fn exchange(_: &Endpoint, _: &[u8]) -> std::io::Result<Vec<u8>> {
    Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "no engine endpoint on this OS"))
}

/// An answer's body, when its status is 200.
pub fn body_of(answer: &[u8]) -> std::io::Result<&[u8]> {
    let split = memchr::memmem::find(answer, b"\r\n\r\n").ok_or_else(|| std::io::Error::other("the engine's answer has no head"))?;
    let (head, rest) = answer.split_at(split);
    let status = head.split(|&b| b == b' ').nth(1).unwrap_or_default();
    if status != b"200" {
        return Err(std::io::Error::other(format!("the engine answered {}", String::from_utf8_lossy(head.split(|&b| b == b'\r').next().unwrap_or_default()))));
    }
    Ok(rest.get(4..).unwrap_or_default())
}

/// `GET path`'s body.
pub fn get(e: &Endpoint, path: &str) -> std::io::Result<Vec<u8>> {
    let answer = exchange(e, format!("GET {path} HTTP/1.0\r\nHost: docker\r\nUser-Agent: kiln\r\n\r\n").as_bytes())?;
    body_of(&answer).map(<[u8]>::to_vec)
}

/// What the engine says of a running container.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Named {
    pub name: String,
    pub image: String,
    pub labels: HashMap<String, String>,
    /// When it was made, seconds since the epoch.
    pub created: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Listed {
    id: String,
    #[serde(default)]
    names: Vec<String>,
    #[serde(default)]
    image: String,
    #[serde(default)]
    labels: Option<HashMap<String, String>>,
    #[serde(default)]
    created: i64,
}

/// The engine's list read: each container by its id.
pub fn parse_list(body: &[u8]) -> Option<HashMap<String, Named>> {
    let listed: Vec<Listed> = serde_json::from_slice(body).ok()?;
    Some(
        listed
            .into_iter()
            .map(|l| {
                let name = l.names.first().map(|n| n.trim_start_matches('/').to_owned()).unwrap_or_default();
                (l.id, Named { name, image: l.image, labels: l.labels.unwrap_or_default(), created: l.created })
            })
            .collect(),
    )
}

/// The running containers, by id.
pub fn list(e: &Endpoint) -> std::io::Result<HashMap<String, Named>> {
    parse_list(&get(e, "/containers/json")?).ok_or_else(|| std::io::Error::other("the engine's list does not read"))
}

fn at(v: &Value, path: &str) -> Option<u64> {
    v.pointer(path).and_then(Value::as_u64)
}

/// A container's stats as the engine gives them, as the cgroups' are read: its CPU in ns, its
/// memory (its working set the usage less `inactive_file`, cgroup v2's name or v1's), its block I/O
/// summed by direction, its processes.
pub fn parse_stats(body: &[u8]) -> Option<Stats> {
    let v: Value = serde_json::from_slice(body).ok()?;
    let mut c = Counters { cpu_ns: at(&v, "/cpu_stats/cpu_usage/total_usage")?, ..Counters::default() };
    c.user_ns = at(&v, "/cpu_stats/cpu_usage/usage_in_usermode").unwrap_or(0);
    c.system_ns = at(&v, "/cpu_stats/cpu_usage/usage_in_kernelmode").unwrap_or(0);
    c.throttled_ns = at(&v, "/cpu_stats/throttling_data/throttled_time").unwrap_or(0);
    c.throttled_periods = at(&v, "/cpu_stats/throttling_data/throttled_periods").unwrap_or(0);
    for io in v.pointer("/blkio_stats/io_service_bytes_recursive").and_then(Value::as_array).into_iter().flatten() {
        let n = io.get("value").and_then(Value::as_u64).unwrap_or(0);
        match io.get("op").and_then(Value::as_str).map(str::to_ascii_lowercase).as_deref() {
            Some("read") => c.read_bytes = c.read_bytes.saturating_add(n),
            Some("write") => c.write_bytes = c.write_bytes.saturating_add(n),
            _ => {}
        }
    }
    let usage = at(&v, "/memory_stats/usage");
    let stat = |k: &str| at(&v, &format!("/memory_stats/stats/{k}"));
    let inactive = stat("inactive_file").or_else(|| stat("total_inactive_file")).unwrap_or(0);
    Some(Stats {
        counters: c,
        memory_usage: usage,
        memory_working_set: usage.map(|u| u.saturating_sub(inactive)),
        memory_rss: stat("anon").or_else(|| stat("rss")),
        memory_cache: stat("file").or_else(|| stat("cache")),
        memory_limit: at(&v, "/memory_stats/limit"),
        cpu_limit: None,
        pids: at(&v, "/pids_stats/current"),
    })
}

/// A container's stats, once (not streamed).
pub fn stats(e: &Endpoint, id: &str) -> std::io::Result<Stats> {
    parse_stats(&get(e, &format!("/containers/{id}/stats?stream=false&one-shot=true"))?).ok_or_else(|| std::io::Error::other("the engine's stats do not read"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// The engine's answers read as an engine of 2026 gives them (a list of two, one's stats);
    /// what is not 200 refused, saying so.
    #[test]
    fn the_engine_s_answers_read() {
        let list = br#"[{"Id":"aa11","Names":["/checkout-api"],"Image":"registry.example.com/shop/checkout:2.41.0","Labels":{"com.docker.compose.service":"checkout","com.docker.compose.project":"shop"},"Created":1792670400,"State":"running"},{"Id":"bb22","Names":[],"Image":"busybox","Labels":null,"Created":1792670500}]"#;
        let named = parse_list(list).unwrap();
        assert_eq!(named["aa11"].name, "checkout-api");
        assert_eq!(named["aa11"].labels.get("com.docker.compose.service").map(String::as_str), Some("checkout"));
        assert_eq!((named["bb22"].name.as_str(), named["bb22"].labels.len()), ("", 0));
        let stats = br#"{"cpu_stats":{"cpu_usage":{"total_usage":76560938000,"usage_in_kernelmode":16000000000,"usage_in_usermode":60000000000},"system_cpu_usage":1,"online_cpus":32,"throttling_data":{"periods":10,"throttled_periods":2,"throttled_time":3000}},"memory_stats":{"usage":5083136,"stats":{"anon":1048576,"file":2097152,"inactive_file":1048576},"limit":67108864},"pids_stats":{"current":4},"blkio_stats":{"io_service_bytes_recursive":[{"major":8,"minor":0,"op":"read","value":4096},{"major":8,"minor":0,"op":"write","value":1024},{"major":8,"minor":16,"op":"Read","value":10}]}}"#;
        let s = parse_stats(stats).unwrap();
        assert_eq!((s.counters.cpu_ns, s.counters.user_ns, s.counters.system_ns, s.counters.throttled_periods), (76_560_938_000, 60_000_000_000, 16_000_000_000, 2));
        assert_eq!((s.counters.read_bytes, s.counters.write_bytes), (4106, 1024));
        assert_eq!(
            (s.memory_usage, s.memory_working_set, s.memory_rss, s.memory_cache, s.memory_limit, s.pids),
            (Some(5_083_136), Some(4_034_560), Some(1_048_576), Some(2_097_152), Some(67_108_864), Some(4))
        );
        assert_eq!(body_of(b"HTTP/1.0 200 OK\r\nContent-Type: application/json\r\n\r\n[]").unwrap(), b"[]");
        assert!(body_of(b"HTTP/1.0 404 Not Found\r\n\r\n{}").unwrap_err().to_string().contains("404"));
        assert!(body_of(b"garbage").is_err());
    }
}
