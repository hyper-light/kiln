//! Prometheus's scrapes (`docs/observe.md` §8): an exporter's `/metrics` read on a schedule, as a
//! Prometheus `scrape_config` reads a target, by the server or a runner, its samples written as any
//! series is.
//!
//! - A target is a URL, how often it is read (`every_s`, 15 unless said: Datadog's agent's check
//!   interval; Prometheus's default `scrape_interval` is a minute) and how long a scrape may take
//!   (`timeout_s`, 10 unless said: Prometheus's `scrape_timeout`; never past the interval), and the
//!   machine that reads it: the server, a runner by its name, or every machine (`*`), each its own
//!   (a node exporter on every host, `http://127.0.0.1:9100/metrics`).
//! - Its samples are tagged `job` (the target's name) and `instance` (the URL's host and port) as
//!   Prometheus tags them, over a sample's own labels of those keys (kept as `exported_job`, ...:
//!   Prometheus's `honor_labels: false`), with `host` (the machine) when every machine reads its
//!   own, and the target's own tags. Each scrape writes Prometheus's own series of it: `up` (1, or
//!   0 when it failed), `scrape_duration_seconds` and `scrape_samples_scraped`.
//! - A scrape asks for OpenMetrics or the text format, as Prometheus's `Accept` does, gzip allowed,
//!   reads what the answer says it is, [`BODY_MAX`] bytes at the most; scrapes are spread through
//!   their interval by a hash of their target (Prometheus's offset), so a hundred targets are not
//!   read at once, and kept to their schedule (a slow scrape does not push the next ones later).
//! - A target that starts failing, or recovers, is said as an event once. The server keeps the
//!   targets (`<observe>/scrapes.json`, a bearer token sealed under the master key, never shown
//!   again) and tells each runner its own; each machine reports how its scrapes go in its status.

use std::collections::{BTreeMap, HashMap};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use base64::Engine as _;
use kiln_obs::batch::{Kind, SeriesKey};
use kiln_obs::logs::record::Record;
use kiln_obs::prom::Format;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::http::client::Client;

/// What a scrape asks for, Prometheus's own (`scrape.go`, `acceptHeader`): OpenMetrics first, then
/// the text format.
const ACCEPT: &str = "application/openmetrics-text;version=1.0.0;q=0.5,application/openmetrics-text;version=0.0.1;q=0.4,text/plain;version=0.0.4;q=0.3,*/*;q=0.1";
/// The most bytes a scrape reads, unzipped: Prometheus has no bound unless given one; the
/// biggest exporters (kube-state-metrics on large clusters) serve some tens of MiB.
pub const BODY_MAX: usize = 64 << 20;
/// Datadog's agent's check interval, and Prometheus's `scrape_timeout`.
const EVERY_S: u64 = 15;
const TIMEOUT_S: u64 = 10;
/// The longest interval: a day.
const EVERY_MAX_S: u64 = 86_400;
/// Points a write holds at the most: a big scrape is written in parts.
const WRITE_POINTS: usize = 100_000;
/// What binds a sealed token to its target.
const SEALED: &str = "sealed:v1:";
/// The machine that is the server, as targets name it.
pub const SERVER: &str = "server";
/// Every machine, each its own.
pub const EVERY: &str = "*";

fn every_default() -> u64 {
    EVERY_S
}

fn timeout_default() -> u64 {
    TIMEOUT_S
}

fn on_default() -> String {
    SERVER.to_owned()
}

/// A target: what to read, how often, who reads it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    pub url: String,
    #[serde(default = "every_default")]
    pub every_s: u64,
    #[serde(default = "timeout_default")]
    pub timeout_s: u64,
    /// `server`, a runner's name, or `*` (every machine, each its own).
    #[serde(default = "on_default")]
    pub on: String,
    /// `key:value` tags on every sample.
    #[serde(default)]
    pub tags: Vec<String>,
    /// A bearer token the target asks for: sealed at rest, never shown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer: Option<String>,
}

impl Target {
    pub fn check(&self) -> Result<(), String> {
        let uri: http::Uri = self.url.parse().map_err(|_| format!("a target's URL: {}", self.url))?;
        if !matches!(uri.scheme_str(), Some("http" | "https")) || uri.host().is_none_or(str::is_empty) {
            return Err(format!("a target's URL is http:// or https:// with a host: {}", self.url));
        }
        if !(1..=EVERY_MAX_S).contains(&self.every_s) {
            return Err(format!("every_s: 1 to {EVERY_MAX_S}"));
        }
        if self.timeout_s == 0 || self.timeout_s > self.every_s {
            return Err("timeout_s: 1, and no longer than every_s".into());
        }
        if self.on.is_empty() || self.on.len() > 255 {
            return Err("on: `server`, a runner's name, or `*`".into());
        }
        for t in &self.tags {
            if t.split_once(':').is_none_or(|(k, v)| k.is_empty() || v.is_empty()) {
                return Err(format!("a tag is key:value: {t}"));
            }
        }
        Ok(())
    }

    /// Whether `machine` reads it (`server` for the server).
    pub fn read_by(&self, machine: &str) -> bool {
        self.on == EVERY || self.on == machine
    }

    /// As the API shows it: its token left out.
    pub fn redacted(&self) -> Value {
        json!({ "url": self.url, "every_s": self.every_s, "timeout_s": self.timeout_s, "on": self.on, "tags": self.tags, "bearer": self.bearer.is_some() })
    }

    /// The URL's host and port: Prometheus's `instance`.
    fn instance(&self) -> String {
        let Ok(uri) = self.url.parse::<http::Uri>() else { return self.url.clone() };
        let port = uri.port_u16().unwrap_or(if uri.scheme_str() == Some("https") { 443 } else { 80 });
        format!("{}:{port}", uri.host().unwrap_or(""))
    }
}

/// How a machine's scrapes of a target go, as its status says.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// The last scrape's: none before the first.
    pub up: Option<bool>,
    /// Unix ms.
    pub last_ms: Option<i64>,
    pub took_ms: u64,
    pub samples: u64,
    /// Samples the last scrape could not read (a line that does not parse, a series that cannot be
    /// made), and those not written (observe busy).
    pub skipped: u64,
    pub deferred: u64,
    pub error: Option<String>,
    pub scrapes: u64,
    pub failures: u64,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

// --------------------------------------------------------------------------------------- store

/// The server's targets by name, kept in `<observe>/scrapes.json` (a token sealed under the master
/// key, the file the owner's alone).
pub struct Targets {
    path: PathBuf,
    master: PathBuf,
    all: Mutex<BTreeMap<String, Target>>,
}

fn aad(name: &str) -> Vec<u8> {
    format!("kiln observe scrape\0{name}\0bearer").into_bytes()
}

impl Targets {
    pub fn open(dir: &Path, master: &Path) -> Self {
        let path = dir.join("scrapes.json");
        let mut all: BTreeMap<String, Target> = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        for (name, t) in &mut all {
            if let Some(s) = t.bearer.as_deref().and_then(|s| s.strip_prefix(SEALED)) {
                t.bearer = match unseal(master, name, s) {
                    Ok(plain) => Some(plain),
                    Err(e) => {
                        tracing::warn!(target = %name, error = %format!("{e:#}"), "a scrape target's token does not open: give it again");
                        None
                    }
                };
            }
        }
        Self { path, master: master.to_owned(), all: Mutex::new(all) }
    }

    pub fn list(&self) -> Vec<(String, Target)> {
        lock(&self.all).iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    pub fn get(&self, name: &str) -> Option<Target> {
        lock(&self.all).get(name).cloned()
    }

    /// Keeps `t` as `name` (a token left out is the old one's).
    pub fn put(&self, name: &str, mut t: Target) -> Result<(), String> {
        if name.is_empty() || name.len() > 64 || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)) {
            return Err("a target's name is 1 to 64 letters, digits, '-', '_' and '.'".into());
        }
        t.check()?;
        let mut all = lock(&self.all);
        if t.bearer.is_none() {
            t.bearer = all.get(name).and_then(|o| o.bearer.clone());
        }
        all.insert(name.to_owned(), t);
        self.save(&all).map_err(|e| format!("{e:#}"))
    }

    pub fn delete(&self, name: &str) -> anyhow::Result<bool> {
        let mut all = lock(&self.all);
        let had = all.remove(name).is_some();
        self.save(&all)?;
        Ok(had)
    }

    fn save(&self, all: &BTreeMap<String, Target>) -> anyhow::Result<()> {
        let mut sealed = all.clone();
        for (name, t) in &mut sealed {
            if let Some(plain) = t.bearer.as_deref() {
                let (ct, nonce) = crate::ci::secrets::seal_with(&self.master, plain.as_bytes(), &aad(name))?;
                let b64 = base64::engine::general_purpose::STANDARD_NO_PAD;
                t.bearer = Some(format!("{SEALED}{}:{}", b64.encode(nonce), b64.encode(ct)));
            }
        }
        crate::util::write_secret(&self.path, &serde_json::to_vec_pretty(&sealed)?, 0o600)?;
        Ok(())
    }
}

fn unseal(master: &Path, name: &str, s: &str) -> anyhow::Result<String> {
    let b64 = base64::engine::general_purpose::STANDARD_NO_PAD;
    let (nonce, ct) = s.split_once(':').ok_or_else(|| anyhow::anyhow!("not a sealed token"))?;
    let plain = crate::ci::secrets::open_with(master, &b64.decode(ct)?, &b64.decode(nonce)?, &aad(name))?;
    Ok(String::from_utf8(plain)?)
}

// ------------------------------------------------------------------------------------- scraper

/// A machine's scrapes: each target it reads, its own task.
pub struct Scraper {
    machine: String,
    http: Arc<Client>,
    running: Mutex<HashMap<String, Running>>,
    stop: crate::rt::Shutdown,
}

struct Running {
    target: Target,
    stop: crate::rt::Shutdown,
    state: Arc<Mutex<State>>,
}

static SCRAPER: OnceLock<Arc<Scraper>> = OnceLock::new();

/// This process's scraper, started once: `machine` the name targets give it (`server` for the
/// server), `host` the name its samples of every machine's targets are tagged with.
pub fn start(machine: &str, host: &str, http: Arc<Client>, stop: crate::rt::Shutdown) -> &'static Arc<Scraper> {
    SCRAPER.get_or_init(|| Arc::new(Scraper { machine: format!("{machine}\0{host}"), http, running: Mutex::new(HashMap::new()), stop }))
}

pub fn get() -> Option<&'static Arc<Scraper>> {
    SCRAPER.get()
}

impl Scraper {
    fn names(&self) -> (&str, &str) {
        self.machine.split_once('\0').unwrap_or((&self.machine, &self.machine))
    }

    /// Reads `targets` (of every machine: those this one reads are kept) from now: one unchanged
    /// goes on as it was, a changed one starts again, one gone stops.
    pub fn set(&self, targets: &[(String, Target)]) {
        let (machine, _) = self.names();
        let mine: HashMap<&str, &Target> = targets.iter().filter(|(_, t)| t.read_by(machine)).map(|(n, t)| (n.as_str(), t)).collect();
        let mut running = lock(&self.running);
        running.retain(|name, r| {
            let keep = mine.get(name.as_str()).is_some_and(|t| **t == r.target);
            if !keep {
                r.stop.cancel();
            }
            keep
        });
        for (name, t) in mine {
            if running.contains_key(name) {
                continue;
            }
            let r = Running { target: t.clone(), stop: crate::rt::Shutdown::new(), state: Arc::new(Mutex::new(State::default())) };
            let (http, stop, server, state, target, name_owned, host) = (self.http.clone(), r.stop.clone(), self.stop.clone(), r.state.clone(), t.clone(), name.to_owned(), self.names().1.to_owned());
            crate::rt::spawn(async move { scrape_loop(http, name_owned, host, target, stop, server, state).await });
            running.insert(name.to_owned(), r);
        }
    }

    /// How each of this machine's targets goes.
    pub fn states(&self) -> Vec<(String, State)> {
        let mut out: Vec<(String, State)> = lock(&self.running).iter().map(|(n, r)| (n.clone(), lock(&r.state).clone())).collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}

/// The target's offset into its interval: Prometheus's (a hash of the target, so each keeps its
/// place across restarts and targets spread evenly).
fn offset(name: &str, t: &Target, every: Duration) -> Duration {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.bytes().chain([0]).chain(t.url.bytes()) {
        h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
    }
    let ms = u64::try_from(every.as_millis()).unwrap_or(u64::MAX).max(1);
    Duration::from_millis(h % ms)
}

/// One scrape: the exposition's text and its format.
pub async fn scrape_once(http: &Client, t: &Target) -> anyhow::Result<(String, Format)> {
    let timeout = Duration::from_secs(t.timeout_s);
    let fetch = async {
        let mut req = http.get(&t.url).header("Accept", ACCEPT).header("Accept-Encoding", "gzip").header("X-Prometheus-Scrape-Timeout-Seconds", &t.timeout_s.to_string()).timeout(timeout);
        if let Some(b) = &t.bearer {
            req = req.bearer(b);
        }
        let r = req.send().await?;
        let status = r.status();
        let format = Format::of(r.header("content-type").unwrap_or(""));
        let gzip = r.header("content-encoding").is_some_and(|e| e.trim().eq_ignore_ascii_case("gzip"));
        let body = r.bytes(BODY_MAX).await?;
        anyhow::ensure!((200..300).contains(&status), "{} answered {status}", t.url);
        let body = if gzip {
            let mut plain = Vec::new();
            flate2::read::GzDecoder::new(&body[..]).take(BODY_MAX as u64 + 1).read_to_end(&mut plain)?;
            anyhow::ensure!(plain.len() <= BODY_MAX, "{} unzips past {BODY_MAX} bytes", t.url);
            plain
        } else {
            body.to_vec()
        };
        Ok((String::from_utf8_lossy(&body).into_owned(), format))
    };
    match crate::rt::timeout(timeout, fetch).await {
        Ok(r) => r,
        Err(_) => anyhow::bail!("{} took more than {} s", t.url, t.timeout_s),
    }
}

/// A target's task: a scrape at each of its times until it is changed, gone, or the process stops.
async fn scrape_loop(http: Arc<Client>, name: String, host: String, t: Target, stop: crate::rt::Shutdown, server: crate::rt::Shutdown, state: Arc<Mutex<State>>) {
    let every = Duration::from_secs(t.every_s);
    if !stop.sleep(offset(&name, &t, every)).await || server.is_cancelled() {
        return;
    }
    let instance = t.instance();
    let mut tags: Vec<(String, String)> = vec![("job".into(), name.clone()), ("instance".into(), instance)];
    if t.on == EVERY {
        tags.push(("host".into(), host));
    }
    for tag in &t.tags {
        if let Some((k, v)) = tag.split_once(':')
            && !tags.iter().any(|(tk, _)| tk == k)
        {
            tags.push((k.to_owned(), v.to_owned()));
        }
    }
    let start = Instant::now();
    let mut k: u32 = 0;
    let mut failing = false;
    loop {
        if stop.is_cancelled() || server.is_cancelled() {
            return;
        }
        let t0 = Instant::now();
        let at = super::clock_ms();
        let r = scrape_once(&http, &t).await;
        if stop.is_cancelled() {
            return;
        }
        let took = t0.elapsed();
        let borrowed: Vec<(&str, &str)> = tags.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let (up, samples, skipped, deferred, error) = match r {
            Ok((text, format)) => {
                let scraped = kiln_obs::prom::parse_target(&text, at, &borrowed, format);
                let (samples, skipped) = (scraped.samples, scraped.skipped);
                let (_, deferred) = write(scraped.series).await;
                (true, samples, skipped, deferred, None)
            }
            Err(e) => (false, 0, 0, 0, Some(format!("{e:#}"))),
        };
        own(&borrowed, at, up, took, samples);
        {
            let mut s = lock(&state);
            s.up = Some(up);
            s.last_ms = Some(at);
            s.took_ms = u64::try_from(took.as_millis()).unwrap_or(u64::MAX);
            s.samples = samples;
            s.skipped = skipped;
            s.deferred = deferred;
            s.error.clone_from(&error);
            s.scrapes += 1;
            s.failures += u64::from(!up);
        }
        if up == failing {
            failing = !up;
            let message = match &error {
                Some(why) => format!("scrape target {name} is down: {why}"),
                None => format!("scrape target {name} is up again: {samples} samples in {} ms", took.as_millis()),
            };
            event(&name, &t.url, if up { 9 } else { 17 }, if up { "recovered" } else { "failing" }, message);
        }
        // Kept to its schedule: the next of `start + n × every` still ahead.
        k = k.saturating_add(1);
        let mut due = every.saturating_mul(k);
        let ran = start.elapsed();
        if due <= ran {
            k = u32::try_from(ran.as_millis() / every.as_millis().max(1)).unwrap_or(u32::MAX).saturating_add(1);
            due = every.saturating_mul(k);
        }
        let slept = std::pin::pin!(stop.sleep(due.saturating_sub(start.elapsed())));
        let ended = std::pin::pin!(server.cancelled());
        match futures::future::select(slept, ended).await {
            futures::future::Either::Left((true, _)) => {}
            _ => return,
        }
    }
}

/// Writes a scrape's samples in parts as observe has room: those written and those not.
async fn write(series: Vec<(SeriesKey, Vec<(i64, f64)>)>) -> (u64, u64) {
    let Some(o) = super::api::get() else {
        return (0, series.iter().map(|(_, p)| p.len() as u64).sum());
    };
    let (mut written, mut deferred) = (0u64, 0u64);
    let mut part: Vec<(SeriesKey, Vec<(i64, f64)>)> = Vec::new();
    let mut n = 0usize;
    let mut rest = series.into_iter().peekable();
    while let Some(s) = rest.next() {
        n += s.1.len();
        part.push(s);
        if n >= WRITE_POINTS || rest.peek().is_none() {
            let points = n as u64;
            if o.offer(std::mem::take(&mut part)) {
                written += points;
            } else {
                deferred += points;
            }
            n = 0;
        }
    }
    (written, deferred)
}

/// Prometheus's own series of a scrape (`up`, `scrape_duration_seconds`, `scrape_samples_scraped`),
/// tagged as the target's samples are.
fn own(tags: &[(&str, &str)], at: i64, up: bool, took: Duration, samples: u64) {
    let Some(o) = super::api::get() else { return };
    let mut out = Vec::with_capacity(3);
    for (metric, v) in [("up", if up { 1.0 } else { 0.0 }), ("scrape_duration_seconds", took.as_secs_f64()), ("scrape_samples_scraped", samples as f64)] {
        if let Ok(key) = SeriesKey::new(metric, tags.iter().copied(), Kind::Gauge) {
            out.push((key, vec![(at, v)]));
        }
    }
    o.write(out);
}

fn event(name: &str, url: &str, severity: u8, result: &str, message: String) {
    let Some(o) = super::api::get() else { return };
    o.log(vec![Record {
        ts: super::clock_ms().saturating_mul(1_000_000),
        severity,
        service: "kiln.scrape".into(),
        host: String::new(),
        source: "kiln.event".into(),
        message,
        attrs: vec![("event.type".into(), "scrape".into()), ("target".into(), name.into()), ("url".into(), url.into()), ("result".into(), result.into())],
        trace_id: None,
        span_id: None,
    }]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(url: &str) -> Target {
        Target { url: url.into(), every_s: 15, timeout_s: 10, on: SERVER.into(), tags: vec!["env:prod".into()], bearer: None }
    }

    #[test]
    fn a_target_says_what_is_wrong_with_it() {
        assert!(target("http://127.0.0.1:9100/metrics").check().is_ok());
        for bad in ["ftp://x/metrics", "http:///metrics", "not a url"] {
            assert!(target(bad).check().is_err(), "{bad}");
        }
        assert!(Target { timeout_s: 20, ..target("http://h/m") }.check().is_err(), "a timeout past the interval");
        assert!(Target { every_s: 0, ..target("http://h/m") }.check().is_err());
        assert!(Target { tags: vec!["nocolon".into()], ..target("http://h/m") }.check().is_err());
        assert_eq!(target("http://10.0.0.5:9100/metrics").instance(), "10.0.0.5:9100");
        assert_eq!(target("https://exporter.example/metrics").instance(), "exporter.example:443");
        let every = Target { on: EVERY.into(), ..target("http://h/m") };
        assert!(every.read_by("server") && every.read_by("r1") && !target("http://h/m").read_by("r1"));
    }

    /// Targets spread through their interval, each at its own place every time.
    #[test]
    fn offsets_spread_targets_and_stay_put() {
        let every = Duration::from_secs(15);
        let offsets: Vec<Duration> = (0..200).map(|i| offset(&format!("t{i}"), &target("http://h/m"), every)).collect();
        assert!(offsets.iter().all(|o| *o < every));
        assert_eq!(Some(offset("t1", &target("http://h/m"), every)), offsets.get(1).copied());
        // A uniform spread: each third of the interval holds a third of them, give or take.
        for third in 0..3u32 {
            let n = offsets.iter().filter(|o| (o.as_millis() * 3 / every.as_millis()) as u32 == third).count();
            assert!((40..=95).contains(&n), "{third}: {n}");
        }
    }

    /// The store keeps a token sealed at rest, gives it back opened, and never shows it.
    #[test]
    fn a_token_is_sealed_at_rest_and_never_shown() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let master = dir.path().join("master.key");
        std::fs::write(&master, [7u8; 32])?;
        let ts = Targets::open(dir.path(), &master);
        ts.put("node", Target { bearer: Some("s3cret".into()), ..target("http://h:9100/metrics") }).map_err(anyhow::Error::msg)?;
        let raw = std::fs::read_to_string(dir.path().join("scrapes.json"))?;
        assert!(!raw.contains("s3cret") && raw.contains(SEALED), "{raw}");
        assert_eq!(Targets::open(dir.path(), &master).get("node").and_then(|t| t.bearer), Some("s3cret".into()));
        // Given again without it, the token stays.
        ts.put("node", target("http://h:9100/metrics")).map_err(anyhow::Error::msg)?;
        assert_eq!(ts.get("node").and_then(|t| t.bearer), Some("s3cret".into()));
        assert_eq!(ts.get("node").and_then(|t| t.redacted().get("bearer").cloned()), Some(Value::Bool(true)));
        assert!(ts.put("bad name", target("http://h/m")).is_err());
        assert!(ts.delete("node")?);
        Ok(())
    }
}
