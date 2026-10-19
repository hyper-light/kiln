//! Streams to their viewers over WebRTC (docs/previews.md, "WebRTC"). With `[server] webrtc_addr`
//! set, a viewer's socket (`crate::api`'s `stream_view`) offers a peer connection and the server
//! answers it: an ICE-lite peer on one UDP port for every viewer (str0m, sans-IO, driven here). A
//! stream's chunks go out as RTP as its runner encoded them (H.264, VP8, VP9 or AV1; Opus for its
//! sound): nothing is decoded or encoded again. What a viewer loses is sent again (NACK) or, past
//! that, mended by a key frame asked of the runner (PLI). The socket stays: a stream's events and
//! what's done in it always go over it, and its chunks too until the peer plays (and again if the
//! peer goes).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use bytes::Bytes;
use futures::FutureExt as _;
use hyper_rt::udp::UdpSocket;
use serde_json::Value;
use str0m::change::SdpOffer;
use str0m::crypto::CryptoProvider;
use str0m::crypto::dtls::DtlsCert;
use str0m::format::{Codec, PayloadParams};
use str0m::media::{Frequency, MediaKind, MediaTime, Mid, Pt};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};

use crate::hub::Hub;

/// Peers at once, every viewer's together.
const MOST: usize = 64;
/// Chunks waiting for the peers before one is dropped (the next key frame mends it).
const QUEUED: usize = 4096;
/// A peer that hasn't connected by then goes (its viewer can't reach this server's port).
const CONNECT_WITHIN: Duration = Duration::from_secs(30);
/// Datagrams taken at a time before the rest of the loop has a turn.
const BATCH: usize = 64;

/// str0m's code, behind an unwind boundary: what it panics on (a viewer's offer, a datagram from
/// anyone) ends that peer, and nothing else.
fn contained<R>(op: impl FnOnce() -> R) -> Option<R> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(op)).ok()
}

/// What a peer tells its viewer's socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Said {
    /// Connected: it starts from a key frame (asked of the runner).
    Connected,
    /// Its viewer lost a picture it can't do without: a key frame.
    Keyframe,
    /// It's gone, and why.
    Gone(String),
}

/// The server's WebRTC: off until [`run`] has its socket.
pub struct Webrtc {
    on: OnceLock<On>,
    to_loop: async_channel::Sender<Cmd>,
    from: async_channel::Receiver<Cmd>,
    next: AtomicU64,
}

/// What every peer is made with: where viewers reach it, its certificate, its crypto.
struct On {
    candidates: Vec<SocketAddr>,
    cert: DtlsCert,
    crypto: Arc<CryptoProvider>,
}

impl Default for Webrtc {
    fn default() -> Self {
        let (to_loop, from) = async_channel::bounded(QUEUED);
        Self { on: OnceLock::new(), to_loop, from, next: AtomicU64::new(1) }
    }
}

enum Cmd {
    Add(Box<Peering>),
    Chunk(u64, Bytes),
    Gone(u64),
}

/// A viewer's peer, as its socket holds it; dropped, it goes.
pub struct Peer {
    id: u64,
    to_loop: async_channel::Sender<Cmd>,
    pub said: async_channel::Receiver<Said>,
}

impl Peer {
    /// A chunk of its stream (`crate::browser::stream::frame`), to send.
    pub fn chunk(&self, data: &Bytes) {
        let _ = self.to_loop.try_send(Cmd::Chunk(self.id, data.clone()));
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.to_loop.try_send(Cmd::Gone(self.id));
    }
}

impl Webrtc {
    /// Whether viewers may offer peers.
    pub fn on(&self) -> bool {
        self.on.get().is_some()
    }

    /// A viewer's offer (SDP) for a stream whose picture is `codec` (WebCodecs' string): its peer,
    /// and the answer (SDP).
    pub fn offer(&self, codec: &str, sdp: &str) -> anyhow::Result<(Peer, String)> {
        let on = self.on.get().ok_or_else(|| anyhow::anyhow!("WebRTC is off on this server (`[server] webrtc_addr`)"))?;
        let family = family(codec).ok_or_else(|| anyhow::anyhow!("a stream in {codec} doesn't go over WebRTC"))?;
        anyhow::ensure!(offers(sdp, family), "this browser's WebRTC doesn't take {codec}");
        let (video, audio) = mids(sdp);
        anyhow::ensure!(video.is_some(), "its offer takes no picture");
        let (rtc, answer) = contained(|| -> anyhow::Result<(Rtc, String)> {
            let offer = SdpOffer::from_sdp_string(sdp).map_err(|e| anyhow::anyhow!("its offer: {e}"))?;
            let mut rtc = Rtc::builder().set_ice_lite(true).set_dtls_cert(on.cert.clone()).set_crypto_provider(on.crypto.clone()).build(Instant::now());
            for at in &on.candidates {
                let c = Candidate::host(*at, "udp").map_err(|e| anyhow::anyhow!("a candidate at {at}: {e}"))?;
                rtc.add_local_candidate(c);
            }
            let answer = rtc.sdp_api().accept_offer(offer).map_err(|e| anyhow::anyhow!("its offer: {e}"))?;
            Ok((rtc, answer.to_sdp_string()))
        })
        .unwrap_or_else(|| Err(anyhow::anyhow!("its offer couldn't be taken")))?;
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tell, said) = async_channel::bounded(16);
        self.to_loop.try_send(Cmd::Add(Box::new(Peering::new(id, rtc, family, (video, audio), tell)))).map_err(|_| anyhow::anyhow!("WebRTC is busy here"))?;
        Ok((Peer { id, to_loop: self.to_loop.clone(), said }, answer))
    }
}

/// The codec family of a WebCodecs codec string (`avc1.42E01f`, `vp8`, `vp09.00.10.08`, `av01…`).
fn family(codec: &str) -> Option<Codec> {
    let c = codec.trim().to_ascii_lowercase();
    if c.starts_with("avc1") || c.starts_with("avc3") || c == "h264" {
        Some(Codec::H264)
    } else if c == "vp8" {
        Some(Codec::Vp8)
    } else if c.starts_with("vp09") || c == "vp9" {
        Some(Codec::Vp9)
    } else if c.starts_with("av01") || c == "av1" {
        Some(Codec::Av1)
    } else {
        None
    }
}

/// An offer's picture and sound (their mids, from its `m=` sections): str0m says them only once
/// its peer has connected.
fn mids(sdp: &str) -> (Option<Mid>, Option<Mid>) {
    let (mut video, mut audio, mut kind) = (None, None, "");
    for l in sdp.lines().map(str::trim) {
        if let Some(m) = l.strip_prefix("m=") {
            kind = m.split(' ').next().unwrap_or("");
        } else if let Some(mid) = l.strip_prefix("a=mid:").map(str::trim).filter(|m| !m.is_empty()) {
            match kind {
                "video" if video.is_none() => video = Some(Mid::from(mid)),
                "audio" if audio.is_none() => audio = Some(Mid::from(mid)),
                _ => {}
            }
        }
    }
    (video, audio)
}

/// Whether an offer (SDP) takes `family` at all.
fn offers(sdp: &str, family: Codec) -> bool {
    let name = match family {
        Codec::H264 => "H264/90000",
        Codec::Vp8 => "VP8/90000",
        Codec::Vp9 => "VP9/90000",
        Codec::Av1 => "AV1/90000",
        _ => return false,
    };
    sdp.lines().any(|l| l.trim().strip_prefix("a=rtpmap:").and_then(|r| r.split_once(' ')).is_some_and(|(_, c)| c.trim().eq_ignore_ascii_case(name)))
}

/// The payload type to send `family` on, of those its viewer takes. H.264 in fragments
/// (packetization mode 1), constrained baseline first (what kiln encodes), else any profile: a
/// baseline, main or high decoder decodes constrained baseline too.
fn pick<'a>(params: impl Iterator<Item = &'a PayloadParams>, family: Codec) -> Option<Pt> {
    let all: Vec<&PayloadParams> = params.filter(|p| p.spec().codec == family).collect();
    if family != Codec::H264 {
        return all.first().map(|p| p.pt());
    }
    let fragments = |p: &PayloadParams| p.spec().format.packetization_mode == Some(1);
    let constrained = |p: &PayloadParams| p.spec().format.profile_level_id.is_some_and(|id| id >> 16 == 0x42 && (id >> 8) & 0x40 != 0);
    all.iter().find(|p| fragments(p) && constrained(p)).or_else(|| all.iter().find(|p| fragments(p))).map(|p| p.pt())
}

/// A chunk as its runner sent it (`crate::browser::stream::frame`).
struct Chunk<'a> {
    kind: u8,
    flags: u8,
    ts: i64,
    part: u8,
    parts: u8,
    bytes: &'a [u8],
}

fn parse(d: &[u8]) -> Option<Chunk<'_>> {
    Some(Chunk {
        kind: *d.first()?,
        flags: *d.get(1)?,
        ts: i64::from_be_bytes(d.get(2..10)?.try_into().ok()?),
        part: *d.get(10)?,
        parts: *d.get(11)?,
        bytes: d.get(12..)?,
    })
}

/// One viewer's peer, as the loop drives it.
struct Peering {
    id: u64,
    rtc: Rtc,
    tell: async_channel::Sender<Said>,
    family: Codec,
    video: Option<Mid>,
    audio: Option<Mid>,
    video_pt: Option<Pt>,
    audio_pt: Option<Pt>,
    connected: bool,
    /// Until a key frame goes, nothing else of the picture does (its viewer couldn't decode it).
    want_key: bool,
    /// When it's next due (`Output::Timeout`).
    due: Instant,
    began: Instant,
    /// The picture's media time (90 kHz) last sent: as it's sent, so a still frame sent again
    /// sharp shows as now, not as when the picture was first sent.
    last_video: u64,
    /// The sound's first timestamp (µs) and its media time (48 kHz), and the last's: its own
    /// clock, gaps (its silences) and all.
    sound: Option<(i64, u64)>,
    last_sound: Option<(i64, u64)>,
    /// A chunk in parts, as they come: its kind and timestamp, and the parts.
    pieces: Option<(u8, i64, Vec<Option<Vec<u8>>>)>,
    told_gone: bool,
    /// str0m panicked on it: it goes, without another call into it.
    broken: bool,
}

impl Peering {
    fn new(id: u64, rtc: Rtc, family: Codec, (video, audio): (Option<Mid>, Option<Mid>), tell: async_channel::Sender<Said>) -> Self {
        let now = Instant::now();
        Self {
            id,
            rtc,
            tell,
            family,
            video,
            audio,
            video_pt: None,
            audio_pt: None,
            connected: false,
            want_key: true,
            due: now,
            began: now,
            last_video: 0,
            sound: None,
            last_sound: None,
            pieces: None,
            told_gone: false,
            broken: false,
        }
    }

    fn say(&self, s: Said) {
        let _ = self.tell.try_send(s);
    }

    fn gone(&mut self, why: &str) {
        if !self.told_gone {
            self.told_gone = true;
            self.say(Said::Gone(why.to_owned()));
        }
        if !self.broken && contained(|| self.rtc.disconnect()).is_none() {
            self.broken = true;
        }
    }

    /// Whether it goes on: connected or still connecting, and its viewer's socket holds it.
    fn alive(&mut self) -> bool {
        if self.broken {
            self.gone("WebRTC failed on this server");
            return false;
        }
        if self.tell.is_closed() {
            self.told_gone = true;
            self.gone("");
            return false;
        }
        if !self.connected && self.began.elapsed() > CONNECT_WITHIN {
            self.gone("it didn't connect: this server's WebRTC port isn't reachable from the viewer");
        }
        if !contained(|| self.rtc.is_alive()).unwrap_or(false) {
            self.gone("its connection ended");
            return false;
        }
        true
    }

    /// Input for it (a datagram, the time): handled, unless str0m panics on it.
    fn input(&mut self, input: Input<'_>) {
        if self.broken {
            return;
        }
        match contained(|| self.rtc.handle_input(input)) {
            Some(Ok(())) => {}
            Some(Err(e)) => self.gone(&format!("{e}")),
            None => self.broken = true,
        }
    }

    /// What it has to send, sent; what it says, handled; until it says when it's next due.
    async fn drain(&mut self, socket: &UdpSocket) {
        loop {
            if self.broken || !contained(|| self.rtc.is_alive()).unwrap_or(false) {
                return;
            }
            match contained(|| self.rtc.poll_output()) {
                Some(Ok(Output::Timeout(t))) => {
                    self.due = t;
                    return;
                }
                Some(Ok(Output::Transmit(t))) => {
                    if let Err(e) = socket.send_to_writable(&t.contents, t.destination).await {
                        tracing::debug!(error = %e, to = %t.destination, "a WebRTC datagram not sent");
                    }
                }
                Some(Ok(Output::Event(e))) => self.event(e),
                Some(Err(e)) => {
                    self.gone(&format!("{e}"));
                    return;
                }
                None => {
                    self.broken = true;
                    return;
                }
            }
        }
    }

    fn event(&mut self, e: Event) {
        match e {
            Event::Connected => {
                self.connected = true;
                self.want_key = true;
                self.say(Said::Connected);
            }
            Event::IceConnectionStateChange(IceConnectionState::Disconnected) => self.gone("its viewer went away"),
            Event::MediaAdded(m) => match m.kind {
                MediaKind::Video => self.video = Some(m.mid),
                MediaKind::Audio => self.audio = Some(m.mid),
            },
            Event::KeyframeRequest(_) => {
                self.want_key = true;
                self.say(Said::Keyframe);
            }
            Event::Closed => self.gone("its viewer closed it"),
            _ => {}
        }
    }

    /// A chunk of its stream, sent as media (a configuration only says the codec).
    fn chunk(&mut self, data: &[u8], now: Instant) {
        let Some(c) = parse(data) else { return };
        let whole;
        let bytes: &[u8] = if c.parts > 1 {
            match self.piece(&c) {
                Some(b) => {
                    whole = b;
                    &whole
                }
                None => return,
            }
        } else {
            c.bytes
        };
        match c.kind {
            b'c' if c.flags != b'a' => {
                let codec = serde_json::from_slice::<Value>(bytes).ok().and_then(|v| v.get("codec").and_then(Value::as_str).and_then(family));
                if let Some(f) = codec.filter(|f| *f != self.family) {
                    self.family = f;
                    self.video_pt = None;
                }
            }
            b'v' => self.picture(bytes, c.flags == b'k', now),
            b'a' => self.sound(bytes, c.ts, now),
            _ => {}
        }
    }

    /// A part of a chunk; the chunk, once it's whole.
    fn piece(&mut self, c: &Chunk<'_>) -> Option<Vec<u8>> {
        let same = self.pieces.as_ref().is_some_and(|(k, ts, p)| *k == c.kind && *ts == c.ts && p.len() == usize::from(c.parts));
        if !same {
            self.pieces = Some((c.kind, c.ts, vec![None; usize::from(c.parts)]));
        }
        let (_, _, parts) = self.pieces.as_mut()?;
        *parts.get_mut(usize::from(c.part))? = Some(c.bytes.to_vec());
        if parts.iter().any(Option::is_none) {
            return None;
        }
        let (_, _, parts) = self.pieces.take()?;
        Some(parts.into_iter().flatten().flatten().collect())
    }

    fn picture(&mut self, bytes: &[u8], key: bool, now: Instant) {
        if self.broken || !self.connected || (self.want_key && !key) {
            return;
        }
        let (Some(mid), family, known) = (self.video, self.family, self.video_pt) else { return };
        let ticks = u64::try_from(now.saturating_duration_since(self.began).as_micros() * 9 / 100).unwrap_or(u64::MAX).max(self.last_video + 1);
        let rtc = &mut self.rtc;
        let sent = contained(|| {
            let w = rtc.writer(mid)?;
            let pt = known.or_else(|| pick(w.payload_params(), family))?;
            Some(w.write(pt, now, MediaTime::new(ticks, Frequency::NINETY_KHZ), bytes.to_vec()).map(|()| pt))
        });
        match sent {
            Some(Some(Ok(pt))) => {
                self.video_pt = Some(pt);
                self.last_video = ticks;
                if key {
                    self.want_key = false;
                }
            }
            Some(Some(Err(e))) => tracing::debug!(error = %e, "a picture not sent over WebRTC"),
            Some(None) => {}
            None => self.broken = true,
        }
    }

    fn sound(&mut self, bytes: &[u8], ts: i64, now: Instant) {
        if self.broken || !self.connected {
            return;
        }
        let Some(mid) = self.audio else { return };
        let now48 = u64::try_from(now.saturating_duration_since(self.began).as_micros() * 48 / 1000).unwrap_or(u64::MAX);
        let (t0, m0) = *self.sound.get_or_insert((ts, now48));
        let at = ts.checked_sub(t0).and_then(|d| u64::try_from(d).ok()).map(|d| m0 + d.saturating_mul(48) / 1000).filter(|a| self.last_sound.is_none_or(|(_, last)| *a > last));
        let at = at.unwrap_or_else(|| {
            // Its clock went back (its encoder began again): from now on, after what was sent.
            let from = now48.max(self.last_sound.map_or(0, |(_, last)| last + 960));
            self.sound = Some((ts, from));
            from
        });
        // After a silence (Opus sends nothing then) a talkspurt starts.
        let talk = self.last_sound.is_none_or(|(last_ts, _)| ts.saturating_sub(last_ts) > 60_000);
        let (rtc, known) = (&mut self.rtc, self.audio_pt);
        let sent = contained(|| {
            let w = rtc.writer(mid)?;
            let pt = known.or_else(|| pick(w.payload_params(), Codec::Opus))?;
            Some(w.start_of_talkspurt(talk).write(pt, now, MediaTime::new(at, Frequency::FORTY_EIGHT_KHZ), bytes.to_vec()).map(|()| pt))
        });
        match sent {
            Some(Some(Ok(pt))) => {
                self.audio_pt = Some(pt);
                self.last_sound = Some((ts, at));
            }
            Some(Some(Err(e))) => tracing::debug!(error = %e, "sound not sent over WebRTC"),
            Some(None) => {}
            None => self.broken = true,
        }
    }
}

/// The server's WebRTC (a child of the server, with `[server] webrtc_addr`): its socket, and every
/// peer on it.
pub async fn run(hub: Arc<Hub>) -> anyhow::Result<()> {
    let addr = hub.cfg.server.webrtc_addr.clone().unwrap_or_default();
    let bind: SocketAddr = addr.trim().parse().with_context(|| format!("webrtc_addr {addr}"))?;
    let socket = UdpSocket::bind(bind).map_err(|e| anyhow::anyhow!("bind {bind} for WebRTC: {e}"))?;
    let port = socket.local_addr().map_err(|e| anyhow::anyhow!("{e}"))?.port();
    let candidates = candidates(&hub.cfg.server.webrtc_advertise, bind, port).await;
    anyhow::ensure!(!candidates.is_empty(), "WebRTC has no address viewers reach it at: set [server] webrtc_advertise");
    let w = &hub.harness.webrtc;
    if w.on.get().is_none() {
        let crypto = Arc::new(str0m::crypto::from_feature_flags());
        let cert = crypto.dtls_provider.generate_certificate().ok_or_else(|| anyhow::anyhow!("WebRTC has no DTLS certificate"))?;
        let _ = w.on.set(On { candidates: candidates.clone(), cert, crypto });
    }
    tracing::info!(%bind, ?candidates, "WebRTC to streams' viewers");
    Peers { socket, candidates, list: Vec::new() }.serve(&w.from).await
}

/// Where viewers reach this server's WebRTC: what `[server] webrtc_advertise` says (at the bound
/// port when it names no port of its own), else the bound address, else (bound on every address)
/// each LAN address of this machine.
async fn candidates(advertise: &[String], bind: SocketAddr, port: u16) -> Vec<SocketAddr> {
    let mut out: Vec<SocketAddr> = Vec::new();
    for a in advertise.iter().map(|a| a.trim()).filter(|a| !a.is_empty()) {
        let at = match a.parse::<std::net::IpAddr>() {
            Ok(ip) => Ok(SocketAddr::new(ip, port)),
            Err(_) => crate::rt::resolve(a).await,
        };
        match at {
            Ok(at) if !out.contains(&at) => out.push(at),
            Ok(_) => {}
            Err(e) => tracing::warn!(address = a, error = %format!("{e:#}"), "a WebRTC address that doesn't resolve"),
        }
    }
    if out.is_empty() {
        if bind.ip().is_unspecified() {
            let lan = crate::frontdoor::netinfo::lan();
            out.extend(crate::frontdoor::netinfo::addresses(&lan).into_iter().filter(|ip| ip.is_ipv4() == bind.is_ipv4()).map(|ip| SocketAddr::new(ip, port)));
        } else {
            out.push(SocketAddr::new(bind.ip(), port));
        }
    }
    out
}

/// The loop's own: its socket, where viewers reach it, its peers.
struct Peers {
    socket: UdpSocket,
    candidates: Vec<SocketAddr>,
    list: Vec<Peering>,
}

enum Next {
    Readable,
    Cmd(Option<Cmd>),
    Due,
}

impl Peers {
    async fn serve(mut self, cmds: &async_channel::Receiver<Cmd>) -> anyhow::Result<()> {
        let mut buf = vec![0u8; 2048];
        loop {
            let mut due = Instant::now() + Duration::from_secs(1);
            for p in &mut self.list {
                p.drain(&self.socket).await;
                due = due.min(p.due);
            }
            self.list.retain_mut(Peering::alive);
            let next = {
                let readable = self.socket.readable().fuse();
                let cmd = cmds.recv().fuse();
                let wake = crate::rt::sleep_until(due).fuse();
                futures::pin_mut!(readable, cmd, wake);
                futures::select_biased! {
                    _ = readable => Next::Readable,
                    c = cmd => Next::Cmd(c.ok()),
                    () = wake => Next::Due,
                }
            };
            match next {
                Next::Readable => {
                    for _ in 0..BATCH {
                        match self.socket.try_recv_from(&mut buf) {
                            Ok(Some((n, from))) => self.received(buf.get(..n).unwrap_or_default(), from).await,
                            Ok(None) => break,
                            Err(e) => {
                                tracing::debug!(error = %e, "WebRTC's socket");
                                break;
                            }
                        }
                    }
                }
                Next::Cmd(None) => return Ok(()),
                Next::Cmd(Some(Cmd::Add(p))) => {
                    if self.list.len() < MOST {
                        self.list.push(*p);
                    } else {
                        p.say(Said::Gone("this server has as many WebRTC viewers as it takes".into()));
                    }
                }
                Next::Cmd(Some(Cmd::Chunk(id, data))) => {
                    let now = Instant::now();
                    if let Some(p) = self.list.iter_mut().find(|p| p.id == id) {
                        p.chunk(&data, now);
                    }
                }
                Next::Cmd(Some(Cmd::Gone(id))) => {
                    if let Some(p) = self.list.iter_mut().find(|p| p.id == id) {
                        p.told_gone = true;
                        p.gone("");
                    }
                }
                Next::Due => {
                    let now = Instant::now();
                    for p in self.list.iter_mut().filter(|p| p.due <= now) {
                        p.input(Input::Timeout(now));
                    }
                }
            }
        }
    }

    /// A datagram, to the peer it's for (by its ICE credentials, then its address).
    async fn received(&mut self, bytes: &[u8], from: SocketAddr) {
        let now = Instant::now();
        let to = self.destination(from);
        let Ok(r) = Receive::new(Protocol::Udp, from, to, bytes) else { return };
        let input = Input::Receive(now, r);
        let mut to = None;
        for (i, p) in self.list.iter_mut().enumerate().filter(|(_, p)| !p.broken) {
            match contained(|| p.rtc.accepts(&input)) {
                Some(true) => {
                    to = Some(i);
                    break;
                }
                Some(false) => {}
                None => p.broken = true,
            }
        }
        let Some(p) = to.and_then(|i| self.list.get_mut(i)) else { return };
        p.input(input);
        p.drain(&self.socket).await;
    }

    /// The candidate a datagram from `from` came to. One socket serves every address the viewers
    /// were told of, so which one it was sent to isn't known: the first of its family (an ICE-lite
    /// peer only answers, from the address the system sends from).
    fn destination(&self, from: SocketAddr) -> SocketAddr {
        self.candidates.iter().copied().find(|c| c.is_ipv4() == from.is_ipv4()).or_else(|| self.candidates.first().copied()).unwrap_or(from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_streams_codec_finds_its_family_and_an_offer_that_takes_it() {
        assert_eq!(family("avc1.42E01f"), Some(Codec::H264));
        assert_eq!(family("vp8"), Some(Codec::Vp8));
        assert_eq!(family("vp09.00.10.08"), Some(Codec::Vp9));
        assert_eq!(family("av01.0.04M.08"), Some(Codec::Av1));
        assert_eq!(family("theora"), None);
        let sdp = "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96 102\r\na=rtpmap:96 VP8/90000\r\na=rtpmap:102 H264/90000\r\na=rtpmap:111 opus/48000/2\r\n";
        assert!(offers(sdp, Codec::H264) && offers(sdp, Codec::Vp8));
        assert!(!offers(sdp, Codec::Vp9) && !offers(sdp, Codec::Av1));
    }

    #[test]
    fn a_chunk_reads_as_it_was_framed() {
        let f = crate::browser::stream::frame(b'v', b'k', 1_234_567, 1, 3, b"abc");
        let c = parse(&f);
        assert!(c.as_ref().is_some_and(|c| (c.kind, c.flags, c.ts, c.part, c.parts, c.bytes) == (b'v', b'k', 1_234_567, 1, 3, &b"abc"[..])));
        assert!(f.get(..11).and_then(parse).is_none());
    }

    /// An offer as Chrome makes one (recvonly video and audio), answered: the peer sends H.264 on
    /// the constrained baseline payload type in fragments.
    #[test]
    fn a_browsers_offer_is_answered_and_its_payload_type_picked() -> anyhow::Result<()> {
        let w = Webrtc::default();
        let crypto = Arc::new(str0m::crypto::from_feature_flags());
        let cert = crypto.dtls_provider.generate_certificate().ok_or_else(|| anyhow::anyhow!("no certificate"))?;
        assert!(w.offer("avc1.42E01f", OFFER).is_err(), "off until its socket is there");
        let _ = w.on.set(On { candidates: vec![SocketAddr::from(([192, 168, 1, 10], 7072))], cert, crypto });
        let (peer, answer) = w.offer("avc1.42E01f", OFFER)?;
        assert!(answer.contains("a=ice-lite") && answer.contains("192.168.1.10 7072"), "{answer}");
        assert!(answer.contains("a=sendonly") && answer.contains("H264/90000"), "{answer}");
        let Ok(Cmd::Add(mut p)) = w.from.try_recv() else { anyhow::bail!("no peer for the loop") };
        let (Some(video), Some(audio)) = (p.video, p.audio) else { anyhow::bail!("its media weren't found in its offer") };
        assert_eq!((video.to_string(), audio.to_string()), ("0".to_owned(), "1".to_owned()));
        let pt = p.rtc.writer(video).and_then(|w| pick(w.payload_params(), Codec::H264));
        let params = p.rtc.codec_config().params().iter().find(|x| Some(x.pt()) == pt).map(|x| x.spec().format);
        assert!(params.is_some_and(|f| f.packetization_mode == Some(1) && f.profile_level_id == Some(0x42e01f)), "{params:?}");
        // A stream in a codec this browser doesn't take isn't offered a peer.
        assert!(w.offer("vp09.00.10.08", OFFER.replace("VP9", "VPX").as_str()).is_err());
        drop(peer);
        assert!(matches!(w.from.try_recv(), Ok(Cmd::Gone(_))));
        Ok(())
    }

    /// A recvonly offer, as Chrome 13x makes it.
    const OFFER: &str = "v=0\r\n\
o=- 4611731400430051336 2 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
a=group:BUNDLE 0 1\r\n\
a=extmap-allow-mixed\r\n\
a=msid-semantic: WMS\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96 97 102 103 108 109\r\n\
c=IN IP4 0.0.0.0\r\n\
a=rtcp:9 IN IP4 0.0.0.0\r\n\
a=ice-ufrag:abcd\r\n\
a=ice-pwd:abcdefghijklmnopqrstuvwx\r\n\
a=ice-options:trickle\r\n\
a=fingerprint:sha-256 7B:8B:F0:65:5F:78:E2:51:3B:AC:6F:F3:3F:46:1B:35:DC:B8:5F:64:1A:24:C2:43:F0:A1:58:D0:A1:2C:19:08\r\n\
a=setup:actpass\r\n\
a=mid:0\r\n\
a=extmap:1 urn:ietf:params:rtp-hdrext:toffset\r\n\
a=extmap:2 http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time\r\n\
a=extmap:3 urn:3gpp:video-orientation\r\n\
a=extmap:4 http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01\r\n\
a=extmap:5 http://www.webrtc.org/experiments/rtp-hdrext/playout-delay\r\n\
a=extmap:9 urn:ietf:params:rtp-hdrext:sdes:mid\r\n\
a=recvonly\r\n\
a=rtcp-mux\r\n\
a=rtcp-rsize\r\n\
a=rtpmap:96 VP8/90000\r\n\
a=rtcp-fb:96 goog-remb\r\n\
a=rtcp-fb:96 transport-cc\r\n\
a=rtcp-fb:96 ccm fir\r\n\
a=rtcp-fb:96 nack\r\n\
a=rtcp-fb:96 nack pli\r\n\
a=rtpmap:97 rtx/90000\r\n\
a=fmtp:97 apt=96\r\n\
a=rtpmap:102 H264/90000\r\n\
a=rtcp-fb:102 goog-remb\r\n\
a=rtcp-fb:102 transport-cc\r\n\
a=rtcp-fb:102 ccm fir\r\n\
a=rtcp-fb:102 nack\r\n\
a=rtcp-fb:102 nack pli\r\n\
a=fmtp:102 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f\r\n\
a=rtpmap:103 rtx/90000\r\n\
a=fmtp:103 apt=102\r\n\
a=rtpmap:108 H264/90000\r\n\
a=rtcp-fb:108 goog-remb\r\n\
a=rtcp-fb:108 transport-cc\r\n\
a=rtcp-fb:108 ccm fir\r\n\
a=rtcp-fb:108 nack\r\n\
a=rtcp-fb:108 nack pli\r\n\
a=fmtp:108 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\r\n\
a=rtpmap:109 rtx/90000\r\n\
a=fmtp:109 apt=108\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
c=IN IP4 0.0.0.0\r\n\
a=rtcp:9 IN IP4 0.0.0.0\r\n\
a=ice-ufrag:abcd\r\n\
a=ice-pwd:abcdefghijklmnopqrstuvwx\r\n\
a=ice-options:trickle\r\n\
a=fingerprint:sha-256 7B:8B:F0:65:5F:78:E2:51:3B:AC:6F:F3:3F:46:1B:35:DC:B8:5F:64:1A:24:C2:43:F0:A1:58:D0:A1:2C:19:08\r\n\
a=setup:actpass\r\n\
a=mid:1\r\n\
a=extmap:14 urn:ietf:params:rtp-hdrext:ssrc-audio-level\r\n\
a=extmap:9 urn:ietf:params:rtp-hdrext:sdes:mid\r\n\
a=recvonly\r\n\
a=rtcp-mux\r\n\
a=rtpmap:111 opus/48000/2\r\n\
a=rtcp-fb:111 transport-cc\r\n\
a=fmtp:111 minptime=10;useinbandfec=1\r\n";
}
