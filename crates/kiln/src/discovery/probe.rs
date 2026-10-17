//! Probes: finding kiln instances by address, for networks multicast does not cross (other
//! subnets, VPNs, Wi-Fi that isolates its clients, firewalls that drop multicast).
//!
//! * **The probe.** A one-shot ("legacy") multicast DNS query for `_kiln._tcp.local` sent straight
//!   to `<address>:5353` from an ephemeral port. kiln's responder answers such a query by unicast
//!   (RFC 6762 §6.7), and does so for off-subnet sources that `[discovery] answer_from` allows
//!   (private ranges by default), so the answer crosses routers. Answers count only from the
//!   addresses probed, to the query's id.
//! * **HTTP.** A single probe (`POST /api/discovery/probe`), seeds and Tailscale peers also try
//!   `GET http://<address>:<port or 7070>/api/me`: a kiln server says who it is there even when
//!   UDP 5353 is blocked. (A server older than discovery v2 gives no instance id; it is listed under
//!   a made-up one until it is heard properly.)
//! * **Scans** (`POST /api/discovery/scan`): every address of up to 4096 (each range at most a
//!   /22), about 200 probes a second; progress in `scan` and `discovery` events (at most two a
//!   second).
//! * **Sources** (every minute): `[discovery] seeds`; every five minutes: the peers `tailscale
//!   status --json` lists; and every address a scan or probe found an instance at, until it has not
//!   answered three times in a row. Instances found this way are listed for 150 s past the last
//!   answer.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::FutureExt as _;
use parking_lot::Mutex;
use serde::Serialize;

use crate::dns::{self, Message, Question};
use crate::rt::Shutdown;

use super::{Discovery, HTTP_PORT, Instance, MDNS_PORT, TXT_KEYS, Via};

/// How long an instance found by a probe stays listed past its last answer.
pub const HOLD: Duration = Duration::from_secs(150);
pub const REPROBE_EVERY: Duration = Duration::from_secs(60);
pub const TAILSCALE_EVERY: Duration = Duration::from_secs(300);
/// Probes a second.
pub const RATE: u64 = 200;
/// How long a sweep waits for answers after its last probe.
const LINGER: Duration = Duration::from_secs(2);
/// Most addresses one scan covers.
pub const MAX_SCAN: usize = 4096;
/// Most addresses kept for re-probing.
const MAX_TARGETS: usize = 256;
/// Re-probes in a row without an answer before an address is dropped.
const MAX_MISSES: u32 = 3;
const HTTP_TIMEOUT: Duration = Duration::from_millis(2500);

/// `scan` of `GET /api/discovery`: the latest scan.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ScanState {
    pub running: bool,
    pub cidrs: Vec<String>,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    /// Addresses probed so far.
    pub probed: usize,
    pub total: usize,
    /// Instances this scan found that were not listed before it.
    pub found: usize,
}

/// An address a probe found an instance at.
#[derive(Clone, Debug)]
struct Target {
    via: Via,
    /// Its kiln HTTP port, when it answered there.
    http: Option<u16>,
    misses: u32,
}

/// The latest scan and the addresses being re-probed.
#[derive(Default)]
pub struct Probes {
    scan: Mutex<Option<ScanState>>,
    targets: Mutex<HashMap<IpAddr, Target>>,
}

impl Probes {
    pub fn scan(&self) -> Option<ScanState> {
        self.scan.lock().clone()
    }

    /// How many addresses are re-probed.
    pub fn targets(&self) -> usize {
        self.targets.lock().len()
    }

    fn remember(&self, ip: IpAddr, via: Via, http: Option<u16>) {
        let mut g = self.targets.lock();
        if let Some(t) = g.get_mut(&ip) {
            t.misses = 0;
            t.http = http.or(t.http);
            return;
        }
        if g.len() < MAX_TARGETS {
            g.insert(ip, Target { via, http, misses: 0 });
        }
    }
}

/// The addresses of scan ranges: IPv4 CIDRs or addresses, each at most a /22, at most
/// [`MAX_SCAN`] in all; network and broadcast addresses and this machine's own are left out.
pub fn scan_addresses(cidrs: &[String], own: &[IpAddr]) -> Result<Vec<Ipv4Addr>, String> {
    let mut out: Vec<Ipv4Addr> = Vec::new();
    let mut seen: HashSet<Ipv4Addr> = HashSet::new();
    for c in cidrs {
        let Some((ip, len)) = super::parse_cidr(c) else { return Err(format!("{c:?} is not an address or CIDR")) };
        let IpAddr::V4(ip) = ip else { return Err(format!("{c}: only IPv4 ranges can be scanned")) };
        if len < super::MAX_SCAN_PREFIX {
            return Err(format!("{c} is too large to scan: at most a /{} (1024 addresses) each", super::MAX_SCAN_PREFIX));
        }
        let mask = u32::from(super::mask_of(len));
        let net = u32::from(ip) & mask;
        let size = 1u64 << (32 - u32::from(len));
        for k in 0..size {
            let Ok(k) = u32::try_from(k) else { break };
            let a = net.wrapping_add(k);
            // A /31 or /32 has no network or broadcast address to skip.
            if len <= 30 && (k == 0 || u64::from(k) == size - 1) {
                continue;
            }
            let addr = Ipv4Addr::from(a);
            if own.contains(&IpAddr::V4(addr)) || !seen.insert(addr) {
                continue;
            }
            out.push(addr);
            if out.len() > MAX_SCAN {
                return Err(format!("at most {MAX_SCAN} addresses per scan"));
            }
        }
    }
    Ok(out)
}

fn query_bytes(id: u16) -> Result<Vec<u8>, String> {
    Message {
        id,
        questions: vec![Question { name: super::service(), qtype: dns::TYPE_PTR, qclass: dns::CLASS_IN, unicast: false }],
        ..Message::default()
    }
    .encode()
    .map_err(|e| e.to_string())
}

/// Send a one-shot query to `<address>:5353` for each of `addrs`, about [`RATE`] a second, and
/// collect the instances that answer until [`LINGER`] after the last one. `progress` hears how many
/// were probed and each instance as it answers.
pub async fn sweep(addrs: &[IpAddr], via: Via, stop: &Shutdown, progress: &mut (dyn FnMut(usize, Vec<Instance>) + Send)) -> Result<Vec<Instance>, String> {
    sweep_port(addrs, MDNS_PORT, via, stop, progress).await
}

/// [`sweep`] to another port than 5353 (tests).
async fn sweep_port(
    addrs: &[IpAddr],
    port: u16,
    via: Via,
    stop: &Shutdown,
    progress: &mut (dyn FnMut(usize, Vec<Instance>) + Send),
) -> Result<Vec<Instance>, String> {
    let v4 = hyper_rt::udp::UdpSocket::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))).map_err(|e| format!("UDP socket: {e}"))?;
    let v6 = if addrs.iter().any(IpAddr::is_ipv6) {
        hyper_rt::udp::UdpSocket::bind(SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0))).ok()
    } else {
        None
    };
    let id: u16 = rand::random();
    let query = query_bytes(id)?;
    let probed: HashSet<IpAddr> = addrs.iter().copied().collect();
    let interval = Duration::from_micros(1_000_000 / RATE);
    let mut found: Vec<Instance> = Vec::new();
    let mut sent = 0usize;
    let mut next_send = Instant::now();
    let mut done_at: Option<Instant> = None;
    let mut buf4 = vec![0u8; dns::MAX_MESSAGE];
    let mut buf6 = vec![0u8; dns::MAX_MESSAGE];
    enum Ev {
        Stop,
        Packet(Result<(usize, SocketAddr), hyper_rt::RtError>, bool),
        Tick,
    }
    loop {
        let now = Instant::now();
        let before = sent;
        while sent < addrs.len() && next_send <= now {
            if let Some(ip) = addrs.get(sent) {
                let to = SocketAddr::new(*ip, port);
                let sock = if ip.is_ipv4() { Some(&v4) } else { v6.as_ref() };
                // A full send buffer drops this probe: a scan is best effort, re-probes follow.
                if let Some(s) = sock {
                    let _ = s.try_send_to(&query, to);
                }
            }
            sent += 1;
            next_send += interval;
        }
        if sent != before {
            progress(sent, Vec::new());
        }
        if sent >= addrs.len() && done_at.is_none() {
            done_at = Some(now + LINGER);
        }
        if done_at.is_some_and(|t| t <= now) {
            return Ok(found);
        }
        let deadline = done_at.unwrap_or(next_send).max(now);
        let ev = {
            let r4 = v4.recv_from(&mut buf4).fuse();
            let r6 = async {
                match &v6 {
                    Some(s) => s.recv_from(&mut buf6).await,
                    None => futures::future::pending().await,
                }
            }
            .fuse();
            let timer = crate::rt::sleep_until(deadline).fuse();
            let stopped = stop.cancelled().fuse();
            futures::pin_mut!(r4, r6, timer, stopped);
            futures::select_biased! {
                () = stopped => Ev::Stop,
                r = r4 => Ev::Packet(r, false),
                r = r6 => Ev::Packet(r, true),
                () = timer => Ev::Tick,
            }
        };
        match ev {
            Ev::Stop => return Ok(found),
            Ev::Tick | Ev::Packet(Err(_), _) => {}
            Ev::Packet(Ok((n, from)), is_v6) => {
                let data = if is_v6 { buf6.get(..n) } else { buf4.get(..n) }.unwrap_or_default();
                if from.port() != port || !probed.contains(&from.ip()) {
                    continue;
                }
                let Ok(msg) = Message::decode(data) else { continue };
                if !msg.is_response() || msg.id != id {
                    continue;
                }
                let now = Instant::now();
                let mut fresh = Vec::new();
                for mut i in super::heard_via(&msg, from, now, via).0 {
                    // A legacy answer's TTL is ten seconds; the probes keep the entry alive instead.
                    i.set_sources(vec![(via, now + HOLD)]);
                    i.address_expiry = i.addresses.iter().map(|a| (*a, now + HOLD)).collect();
                    i.expires = now + HOLD;
                    if !found.iter().any(|f| f.id == i.id) {
                        found.push(i.clone());
                        fresh.push(i);
                    }
                }
                if !fresh.is_empty() {
                    progress(sent, fresh);
                }
            }
        }
    }
}

/// `http://<ip>:<port>`.
fn base_url(ip: IpAddr, port: u16) -> String {
    match ip {
        IpAddr::V4(v4) => format!("http://{v4}:{port}"),
        IpAddr::V6(v6) => format!("http://[{v6}]:{port}"),
    }
}

/// Who answers `GET /api/me` at `ip:port`, when it is a kiln server.
pub async fn identify_http(client: &crate::http::client::Client, ip: IpAddr, port: u16, via: Via) -> Option<Instance> {
    let resp = client.get(&format!("{}/api/me", base_url(ip, port))).timeout(HTTP_TIMEOUT).send().await.ok()?;
    if !resp.is_success() {
        return None;
    }
    let v: serde_json::Value = crate::rt::timeout(HTTP_TIMEOUT, resp.json(64 << 10)).await.ok()?.ok()?;
    let version = v.get("version")?.as_str()?;
    let hostname = v.get("hostname")?.as_str()?;
    let now = Instant::now();
    if let Some(adv) = v.get("discovery").filter(|a| a.is_object()) {
        // A newer server says what it announces.
        let pairs: Vec<(String, String)> = TXT_KEYS
            .iter()
            .filter_map(|k| {
                let value = adv.get(*k)?;
                let text = value.as_str().map(str::to_owned).or_else(|| value.as_u64().map(|n| n.to_string()))?;
                Some(((*k).to_owned(), text))
            })
            .collect();
        let label = adv.get("instance").and_then(|l| l.as_str()).unwrap_or(hostname);
        let port = adv.get("port").and_then(serde_json::Value::as_u64).and_then(|p| u16::try_from(p).ok()).unwrap_or(port);
        return super::instance_from(label, port, &pairs, vec![ip], via, now, HOLD);
    }
    // An older server: who it is, but no instance id of its own.
    let id: String = crate::util::sha256_hex(format!("kiln-http:{ip}:{port}").as_bytes()).chars().take(32).collect();
    let pairs = vec![
        ("id".to_owned(), id),
        ("name".to_owned(), hostname.to_owned()),
        ("role".to_owned(), "server".to_owned()),
        ("version".to_owned(), version.to_owned()),
    ];
    let label = format!("kiln server at {}", base_url(ip, port).trim_start_matches("http://"));
    let mut i = super::instance_from(&label, port, &pairs, vec![ip], via, now, HOLD)?;
    i.synthetic = true;
    Some(i)
}

/// Probe `addrs` both ways (multicast DNS by unicast, and `/api/me` on `http_port` when given):
/// the instances found, and which addresses answered over HTTP.
async fn probe_addresses(
    d: &Discovery,
    addrs: &[IpAddr],
    http_port: Option<u16>,
    via: Via,
    stop: &Shutdown,
    on_change: &(dyn Fn() + Send + Sync),
) -> (Vec<Instance>, HashSet<IpAddr>) {
    let mut found: Vec<Instance> = Vec::new();
    let mut by_http: HashSet<IpAddr> = HashSet::new();
    let mdns = async {
        let mut ignore = |_: usize, _: Vec<Instance>| {};
        sweep(addrs, via, stop, &mut ignore).await.unwrap_or_default()
    };
    let http = async {
        let Some(port) = http_port else { return Vec::new() };
        let Ok(client) = crate::http::client::Client::public() else { return Vec::new() };
        let checks = addrs.iter().take(64).map(|ip| {
            let client = &client;
            async move { (*ip, identify_http(client, *ip, port, via).await) }
        });
        futures::future::join_all(checks).await
    };
    let (heard, answered) = futures::join!(mdns, http);
    found.extend(heard);
    for (ip, i) in answered {
        if let Some(i) = i {
            by_http.insert(ip);
            if !found.iter().any(|f| f.id == i.id) {
                found.push(i);
            }
        }
    }
    d.found(found.clone(), via, on_change);
    (found, by_http)
}

/// `host`, `host:port`, `ip`, `[v6]:port` or an `http://` URL: the host and the HTTP port given.
pub fn split_address(s: &str) -> Option<(String, Option<u16>)> {
    let s = s.trim();
    let s = s.strip_prefix("http://").or_else(|| s.strip_prefix("https://")).unwrap_or(s);
    let s = s.split('/').next().unwrap_or(s);
    if s.is_empty() {
        return None;
    }
    if let Ok(a) = s.parse::<SocketAddr>() {
        return Some((a.ip().to_string(), Some(a.port())));
    }
    let bare = s.trim_start_matches('[').trim_end_matches(']');
    if bare.parse::<IpAddr>().is_ok() {
        return Some((bare.to_owned(), None));
    }
    match s.rsplit_once(':') {
        Some((h, p)) => Some((h.to_owned(), Some(p.parse().ok()?))),
        None => Some((s.to_owned(), None)),
    }
}

/// `POST /api/discovery/probe`: one address or host name, both ways, within about three seconds.
pub async fn probe(d: &Arc<Discovery>, address: &str, on_change: &(dyn Fn() + Send + Sync)) -> Result<Vec<Instance>, String> {
    let (host, port) = split_address(address).ok_or_else(|| format!("{address:?} is not an address or host name"))?;
    let http = port.unwrap_or(HTTP_PORT);
    let resolved = crate::rt::timeout(Duration::from_secs(2), crate::rt::resolve_host(&host, http))
        .await
        .map_err(|_| format!("cannot resolve {host} (timed out)"))?
        .map_err(|e| format!("cannot resolve {host}: {e:#}"))?;
    let mut addrs: Vec<IpAddr> = Vec::new();
    for a in resolved {
        if !addrs.contains(&a.ip()) && addrs.len() < 4 {
            addrs.push(a.ip());
        }
    }
    let stop = crate::SHUTDOWN.get().cloned().unwrap_or_default();
    let (found, by_http) = probe_addresses(d, &addrs, Some(http), Via::Probe, &stop, on_change).await;
    for i in &found {
        for ip in i.addresses.iter().filter(|a| addrs.contains(a)) {
            d.probes.remember(*ip, Via::Probe, by_http.contains(ip).then_some(http));
        }
    }
    Ok(found.iter().filter_map(|i| d.table.get(&i.id)).collect())
}

/// At most one call every half second (the last one always goes through on `flush`).
struct Throttle {
    last: Option<Instant>,
    pending: bool,
}

impl Throttle {
    fn tick(&mut self, emit: &(dyn Fn() + Send + Sync)) {
        if self.last.is_some_and(|t| t.elapsed() < Duration::from_millis(500)) {
            self.pending = true;
            return;
        }
        self.last = Some(Instant::now());
        self.pending = false;
        emit();
    }

    fn flush(&mut self, emit: &(dyn Fn() + Send + Sync)) {
        if self.pending {
            self.pending = false;
            emit();
        }
    }
}

/// This machine's addresses (left out of scans).
fn own_addresses(d: &Discovery) -> Vec<IpAddr> {
    super::interfaces(&d.settings.interfaces)
        .into_iter()
        .flat_map(|i| i.v4.into_iter().map(|(a, _)| IpAddr::V4(a)).chain(i.v6.into_iter().map(IpAddr::V6)).collect::<Vec<_>>())
        .collect()
}

/// `POST /api/discovery/scan`: start a scan of `cidrs` (default: the LAN subnets) in the
/// background. While one runs, that one is returned.
pub fn start_scan(d: &Arc<Discovery>, cidrs: Option<Vec<String>>, on_change: Arc<dyn Fn() + Send + Sync>) -> Result<ScanState, String> {
    if let Some(s) = d.probes.scan().filter(|s| s.running) {
        return Ok(s);
    }
    let cidrs = cidrs.filter(|c| !c.is_empty()).unwrap_or_else(|| super::subnets(&d.settings.interfaces));
    if cidrs.is_empty() {
        return Err("no LAN subnet found here; give the ranges to scan (cidrs)".to_owned());
    }
    if cidrs.len() > 16 {
        return Err("at most 16 ranges per scan".to_owned());
    }
    let addrs: Vec<IpAddr> = scan_addresses(&cidrs, &own_addresses(d))?.into_iter().map(IpAddr::V4).collect();
    let state = ScanState {
        running: true,
        cidrs,
        started_at: crate::util::now_unix(),
        finished_at: None,
        probed: 0,
        total: addrs.len(),
        found: 0,
    };
    *d.probes.scan.lock() = Some(state.clone());
    let known: HashSet<String> = d.table.snapshot().into_iter().map(|i| i.id).collect();
    let d = Arc::clone(d);
    crate::rt::spawn(async move {
        let stop = crate::SHUTDOWN.get().cloned().unwrap_or_default();
        let throttle = Mutex::new(Throttle { last: None, pending: false });
        let emit = |throttle: &Mutex<Throttle>| throttle.lock().tick(&*on_change);
        let mut new_ids: HashSet<String> = HashSet::new();
        let mut progress = |probed: usize, fresh: Vec<Instance>| {
            for i in &fresh {
                if !known.contains(&i.id) {
                    new_ids.insert(i.id.clone());
                }
                for ip in &i.addresses {
                    d.probes.remember(*ip, Via::Scan, None);
                }
            }
            d.found(fresh, Via::Scan, &|| {});
            if let Some(s) = d.probes.scan.lock().as_mut() {
                s.probed = probed;
                s.found = new_ids.len();
            }
            emit(&throttle);
        };
        let result = sweep(&addrs, Via::Scan, &stop, &mut progress).await;
        if let Err(e) = &result {
            tracing::warn!(error = %e, "discovery: the scan failed");
        }
        if let Some(s) = d.probes.scan.lock().as_mut() {
            s.running = false;
            s.finished_at = Some(crate::util::now_unix());
            s.found = new_ids.len();
        }
        throttle.lock().flush(&*on_change);
        on_change();
        tracing::info!(found = new_ids.len(), "discovery: scan finished");
    });
    Ok(state)
}

// ------------------------------------------------------------------------------ sources

/// The `tailscale` command, where it usually is.
fn tailscale_command() -> Option<std::path::PathBuf> {
    if let Some(p) = crate::os::which("tailscale") {
        return Some(p);
    }
    let candidates: &[&str] = if cfg!(target_os = "macos") {
        &["/Applications/Tailscale.app/Contents/MacOS/Tailscale"]
    } else if cfg!(windows) {
        &[r"C:\Program Files\Tailscale\tailscale.exe"]
    } else {
        &[]
    };
    candidates.iter().map(std::path::PathBuf::from).find(|p| p.is_file())
}

/// The online peers' first IPv4 Tailscale address (else their first one) in `tailscale status
/// --json` output.
pub fn tailscale_peers(status: &serde_json::Value) -> Vec<IpAddr> {
    let Some(peers) = status.get("Peer").and_then(serde_json::Value::as_object) else { return Vec::new() };
    let mut out = Vec::new();
    for p in peers.values().take(256) {
        if p.get("Online").and_then(serde_json::Value::as_bool) == Some(false) {
            continue;
        }
        let ips: Vec<IpAddr> = p
            .get("TailscaleIPs")
            .and_then(serde_json::Value::as_array)
            .map(|a| a.iter().filter_map(|v| v.as_str()?.parse().ok()).collect())
            .unwrap_or_default();
        if let Some(ip) = ips.iter().find(|a| a.is_ipv4()).or_else(|| ips.first())
            && !out.contains(ip)
        {
            out.push(*ip);
        }
    }
    out
}

/// Ask `tailscale status --json` for the peers.
async fn ask_tailscale() -> Result<Vec<IpAddr>, String> {
    let exe = tailscale_command().ok_or_else(|| "Tailscale is not installed".to_owned())?;
    let run = crate::rt::blocking(move || std::process::Command::new(exe).args(["status", "--json"]).stdin(std::process::Stdio::null()).output());
    let output = crate::rt::timeout(Duration::from_secs(10), run)
        .await
        .map_err(|_| "tailscale status did not answer".to_owned())?
        .map_err(|e| format!("{e:#}"))?
        .map_err(|e| format!("cannot run tailscale: {e}"))?;
    if !output.status.success() {
        let why = String::from_utf8_lossy(&output.stderr).lines().next().unwrap_or("not running or logged out").trim().to_owned();
        return Err(format!("tailscale status failed: {why}"));
    }
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).map_err(|e| format!("tailscale status --json: {e}"))?;
    if v.get("BackendState").and_then(serde_json::Value::as_str).is_some_and(|s| s != "Running") {
        return Err(format!("Tailscale is {}", v.get("BackendState").and_then(serde_json::Value::as_str).unwrap_or("not running")));
    }
    Ok(tailscale_peers(&v))
}

async fn probe_tailscale(d: &Discovery, stop: &Shutdown, on_change: &(dyn Fn() + Send + Sync)) {
    match ask_tailscale().await {
        Ok(peers) => {
            d.health.lock().tailscale = super::status::TailscaleState::Active { peers: peers.len() };
            if !peers.is_empty() {
                probe_addresses(d, &peers, Some(HTTP_PORT), Via::Tailscale, stop, on_change).await;
            }
        }
        Err(e) => {
            let not_there = e.contains("not installed");
            if !not_there {
                tracing::debug!(error = %e, "discovery: no Tailscale peers");
            }
            d.health.lock().tailscale = super::status::TailscaleState::Unavailable(e);
        }
    }
}

async fn probe_seeds(d: &Discovery, stop: &Shutdown, on_change: &(dyn Fn() + Send + Sync)) {
    let mut unresolved = Vec::new();
    // Seeds by HTTP port: most give none (7070).
    let mut groups: HashMap<u16, Vec<IpAddr>> = HashMap::new();
    for seed in d.settings.seeds.iter().take(64) {
        let Some((host, port)) = split_address(seed) else {
            unresolved.push(seed.clone());
            continue;
        };
        let port = port.unwrap_or(HTTP_PORT);
        match crate::rt::timeout(Duration::from_secs(3), crate::rt::resolve_host(&host, port)).await {
            Ok(Ok(addrs)) if !addrs.is_empty() => {
                let g = groups.entry(port).or_default();
                for a in addrs.iter().take(4) {
                    if !g.contains(&a.ip()) {
                        g.push(a.ip());
                    }
                }
            }
            _ => unresolved.push(seed.clone()),
        }
    }
    {
        let mut h = d.health.lock();
        h.seeds.count = d.settings.seeds.len();
        h.seeds.unresolved = unresolved;
    }
    for (port, addrs) in groups {
        probe_addresses(d, &addrs, Some(port), Via::Seed, stop, on_change).await;
    }
}

/// Probe again every address scans and probes found instances at; drop those that stopped
/// answering.
async fn reprobe(d: &Discovery, stop: &Shutdown, on_change: &(dyn Fn() + Send + Sync)) {
    let targets: Vec<(IpAddr, Target)> = d.probes.targets.lock().iter().map(|(ip, t)| (*ip, t.clone())).collect();
    for via in [Via::Scan, Via::Probe] {
        let mut by_port: HashMap<Option<u16>, Vec<IpAddr>> = HashMap::new();
        for (ip, t) in targets.iter().filter(|(_, t)| t.via == via) {
            by_port.entry(t.http).or_default().push(*ip);
        }
        for (port, addrs) in by_port {
            let (found, _) = probe_addresses(d, &addrs, port, via, stop, on_change).await;
            let answered: HashSet<IpAddr> = found.iter().flat_map(|i| i.addresses.clone()).collect();
            let mut g = d.probes.targets.lock();
            for ip in addrs {
                if answered.contains(&ip) {
                    if let Some(t) = g.get_mut(&ip) {
                        t.misses = 0;
                    }
                } else if let Some(t) = g.get_mut(&ip) {
                    t.misses += 1;
                    if t.misses >= MAX_MISSES {
                        g.remove(&ip);
                    }
                }
            }
        }
    }
}

/// Probe seeds (every minute), Tailscale peers (every five minutes) and what scans and probes
/// found (every minute) until `shutdown`.
pub async fn run(d: &Arc<Discovery>, shutdown: &Shutdown, on_change: &Arc<dyn Fn() + Send + Sync>) {
    if !d.browsing() {
        return;
    }
    let start = Instant::now();
    let mut next_seeds = (!d.settings.seeds.is_empty()).then_some(start);
    let mut next_tailscale = d.settings.tailscale.then_some(start);
    let mut next_reprobe = start + REPROBE_EVERY;
    loop {
        let now = Instant::now();
        let round = async {
            if next_seeds.is_some_and(|t| t <= now) {
                probe_seeds(d, shutdown, &**on_change).await;
                next_seeds = Some(now + REPROBE_EVERY);
            }
            if next_tailscale.is_some_and(|t| t <= now) {
                probe_tailscale(d, shutdown, &**on_change).await;
                next_tailscale = Some(now + TAILSCALE_EVERY);
            }
            if next_reprobe <= now {
                reprobe(d, shutdown, &**on_change).await;
                next_reprobe = now + REPROBE_EVERY;
            }
        };
        if !super::until_stopped(shutdown, round).await {
            return;
        }
        let next = [next_seeds, next_tailscale, Some(next_reprobe)].into_iter().flatten().min().unwrap_or(next_reprobe);
        if !shutdown.sleep(next.saturating_duration_since(Instant::now())).await {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_ranges_are_checked() {
        let own = [IpAddr::V4(Ipv4Addr::new(192, 168, 1, 7))];
        let a = scan_addresses(&["192.168.1.0/24".into()], &own).unwrap_or_default();
        // Network, broadcast and our own address are left out.
        assert_eq!(a.len(), 253);
        assert_eq!(a.first(), Some(&Ipv4Addr::new(192, 168, 1, 1)));
        assert_eq!(a.last(), Some(&Ipv4Addr::new(192, 168, 1, 254)));
        assert_eq!(scan_addresses(&["10.0.0.0/22".into()], &[]).map(|a| a.len()), Ok(1022));
        assert!(scan_addresses(&["10.0.0.0/21".into()], &[]).is_err_and(|e| e.contains("too large")));
        assert!(scan_addresses(&["fd00::/120".into()], &[]).is_err_and(|e| e.contains("IPv4")));
        assert!(scan_addresses(&["nope".into()], &[]).is_err());
        assert_eq!(scan_addresses(&["10.1.1.5".into(), "10.1.1.4/31".into()], &[]).map(|a| a.len()), Ok(2));
        let five: Vec<String> = (0..5).map(|n| format!("10.{n}.0.0/22")).collect();
        assert!(scan_addresses(&five, &[]).is_err_and(|e| e.contains("at most")));
    }

    #[test]
    fn addresses_to_probe() {
        assert_eq!(split_address("192.168.1.9"), Some(("192.168.1.9".into(), None)));
        assert_eq!(split_address("192.168.1.9:8080"), Some(("192.168.1.9".into(), Some(8080))));
        assert_eq!(split_address("[fd00::9]:7070"), Some(("fd00::9".into(), Some(7070))));
        assert_eq!(split_address("fd00::9"), Some(("fd00::9".into(), None)));
        assert_eq!(split_address("build.lan"), Some(("build.lan".into(), None)));
        assert_eq!(split_address("http://build.lan:7070/"), Some(("build.lan".into(), Some(7070))));
        assert_eq!(split_address("build.lan:http"), None);
        assert_eq!(split_address(" "), None);
    }

    #[test]
    fn tailscale_status() {
        let v = serde_json::json!({
            "BackendState": "Running",
            "Self": { "TailscaleIPs": ["100.64.0.1"] },
            "Peer": {
                "nodekey:a": { "HostName": "a", "Online": true, "TailscaleIPs": ["fd7a:115c:a1e0::2", "100.64.0.2"] },
                "nodekey:b": { "HostName": "b", "Online": false, "TailscaleIPs": ["100.64.0.3"] },
                "nodekey:c": { "HostName": "c", "TailscaleIPs": ["fd7a:115c:a1e0::4"] }
            }
        });
        let mut peers = tailscale_peers(&v);
        peers.sort();
        assert_eq!(peers, vec![IpAddr::V4(Ipv4Addr::new(100, 64, 0, 2)), "fd7a:115c:a1e0::4".parse().unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED))]);
        assert!(tailscale_peers(&serde_json::json!({})).is_empty());
    }

    /// A probe of a real engine over loopback: it answers the one-shot query, and the instance is
    /// held for [`HOLD`] rather than the ten seconds of a legacy answer.
    #[test]
    fn sweeps_find_a_responder() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let mut a = super::super::Advert::new("abcdefabcdefabcdefabcdefabcdef12".into(), super::super::Role::Server, 7070, 7071);
            a.name = "probe-me".into();
            let opts = super::super::Options {
                advert: Some(Arc::new(Mutex::new(a.clone()))),
                port: 0,
                multicast: false,
                ..super::super::Options::browse_only()
            };
            let engine = super::super::Engine::open(opts, Arc::new(super::super::Table::default()))?;
            let port = engine.local_addr().map(|s| s.port()).ok_or_else(|| anyhow::anyhow!("no port"))?;
            let stop = Shutdown::new();
            let served = {
                let stop = stop.clone();
                crate::rt::spawn(async move { engine.serve(stop, &|| {}).await })
            };
            let mut seen = Vec::new();
            let mut progress = |_: usize, fresh: Vec<Instance>| seen.extend(fresh);
            // Two addresses: one answers, one (TEST-NET, never routed) does not.
            let addrs = [IpAddr::V4(Ipv4Addr::LOCALHOST), IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1))];
            let started = Instant::now();
            let found = sweep_port(&addrs, port, Via::Scan, &Shutdown::new(), &mut progress).await.map_err(anyhow::Error::msg)?;
            stop.cancel();
            let _ = crate::rt::timeout(Duration::from_secs(5), served).await;
            let i = found.iter().find(|i| i.id == a.id).ok_or_else(|| anyhow::anyhow!("not found"))?;
            assert_eq!(found.len(), 1);
            assert_eq!(i.via, vec![Via::Scan]);
            assert_eq!(i.name, "probe-me");
            assert!(i.expires > Instant::now() + Duration::from_secs(100));
            assert!(seen.iter().any(|s| s.id == a.id));
            // It lingered for late answers, and no longer.
            assert!(started.elapsed() >= LINGER && started.elapsed() < LINGER + Duration::from_secs(2));
            Ok(())
        })
    }
}
