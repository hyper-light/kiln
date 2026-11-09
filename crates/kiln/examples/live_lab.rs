//! The lab's page and its record check for the browser protocol's send path (goal 3 of the browser
//! protocol program, plan `01a1a832eb2f711f9c6f51d88effe89d` step 3; the lab's script is
//! `crates/kiln-lab/web/servers/bp-g3/flood.sh`).
//!
//! - `client`: a page on `/api/live` over TLS, as a browser opens it (kiln's own TLS client, its keys
//!   written where `SSLKEYLOGFILE` says): opens with a ticket, subscribes to the owner's events topic,
//!   and reads what comes for `secs` seconds, acknowledging as goal 2's client does. Writes what it read
//!   (messages, bytes, the largest message, the changes) as JSON.
//! - `records`: the TLS records the server sent on that socket, from a capture of every byte of it
//!   (`capture.py IFACE OUT 65535`) and the page's keys: each record decrypted (TLS 1.3, AES-GCM or
//!   ChaCha20-Poly1305), its plaintext's length and kind. Says how many of the WebSocket's records (those
//!   after the 101) are past `chunk`, and the largest.
//!
//! ```text
//! live_lab client https://10.0.0.1:8443 CA_PEM TOKEN_FILE TICKET_JSON SECS OUT_JSON
//! live_lab records PCAP KEYLOG SERVER_PORT CHUNK
//! ```
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::cast_possible_truncation)]

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use kiln::http::client::Roots;
use kiln::http::tls::{ClientTls, TlsStream, server_name};
use kiln_bp::client::{Client, ClientConfig, ClientEvent};
use kiln_bp::frames::Topic;
use serde_json::{Value, json};
use tungstenite::Message;
use tungstenite::client::IntoClientRequest as _;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("client") if args.len() == 7 => kiln::rt::block_on(client(args[1].clone(), args[2].clone(), args[3].clone(), args[4].clone(), args[5].parse()?, args[6].clone())),
        Some("records") if args.len() == 5 => records(&args[1], &args[2], args[3].parse()?, args[4].parse()?),
        _ => anyhow::bail!("usage: live_lab client URL CA_PEM TOKEN_FILE TICKET_JSON SECS OUT_JSON | records PCAP KEYLOG SERVER_PORT CHUNK"),
    }
}

async fn client(url: String, ca: String, token: String, ticket: String, secs: u64, out: String) -> anyhow::Result<()> {
    use base64::Engine as _;
    let (url, ca, token, ticket, out) = (url.as_str(), ca.as_str(), token.as_str(), ticket.as_str(), out.as_str());
    let uri: http::Uri = url.parse()?;
    let host = uri.host().ok_or_else(|| anyhow::anyhow!("no host in {url}"))?.to_owned();
    let port = uri.port_u16().unwrap_or(443);
    let token = std::fs::read_to_string(token)?.trim().to_owned();
    let ticket: Value = serde_json::from_str(&std::fs::read_to_string(ticket)?)?;
    let ticket = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(ticket.get("ticket").and_then(Value::as_str).unwrap_or_default())?;
    let addr: std::net::SocketAddr = format!("{host}:{port}").parse()?;
    let tcp = kiln::rt::native(hyper_rt::tcp::TcpStream::connect(addr)).await?.map_err(|e| anyhow::anyhow!("connect {addr}: {e}"))?;
    let mut config = ClientTls::new(Roots::Pem(std::fs::read(ca)?), None)?.config()?;
    config.key_log = Box::new(hyper_quic::rustls::KeyLogFile::new());
    let tls = TlsStream::connect(kiln::http::io::TcpIo::new(tcp), config, server_name(&host)?).await?;
    let mut request = format!("wss://{host}:{port}/api/live").into_client_request()?;
    request.headers_mut().insert("authorization", http::HeaderValue::from_str(&format!("Bearer {token}"))?);
    request.headers_mut().insert("sec-websocket-protocol", http::HeaderValue::from_str(&format!("kiln-bp.{}", kiln_bp::VERSION))?);
    let ws_config = tungstenite::protocol::WebSocketConfig::default().max_message_size(Some(4 << 20)).max_frame_size(Some(4 << 20));
    let (mut ws, _) = async_tungstenite::client_async_with_config(request, tls, Some(ws_config)).await?;
    let t0 = Instant::now();
    let now = || u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX);
    let mut page = Client::new(ClientConfig::default(), &ticket, now());
    let (mut messages, mut bytes, mut largest, mut changes, mut opened) = (0u64, 0u64, 0usize, 0u64, false);
    let mut sizes: BTreeMap<usize, u64> = BTreeMap::new();
    let end = t0 + Duration::from_secs(secs);
    while Instant::now() < end {
        while let Some(t) = page.poll_transmit(64 << 10, now()) {
            ws.send(Message::binary(t.bytes)).await?;
        }
        match kiln::rt::timeout(Duration::from_millis(50), ws.next()).await {
            Ok(Some(Ok(Message::Binary(b)))) => {
                messages += 1;
                bytes += b.len() as u64;
                largest = largest.max(b.len());
                *sizes.entry(b.len().next_power_of_two()).or_default() += 1;
                page.receive(&b, now());
            }
            Ok(Some(Ok(Message::Close(_))) | None) => anyhow::bail!("the server closed the socket"),
            Ok(Some(Ok(_))) | Err(_) => {}
            Ok(Some(Err(e))) => return Err(e.into()),
        }
        while let Some(e) = page.poll_event() {
            match e {
                ClientEvent::Opened { .. } if !opened => {
                    opened = true;
                    page.subscribe(1, Topic { kind: kiln::bp::kind::EVENTS, key: String::new() }, 0);
                }
                ClientEvent::Change { .. } => changes += 1,
                _ => {}
            }
        }
    }
    let _ = ws.close(None).await;
    let v = json!({ "opened": opened, "secs": secs, "messages": messages, "bytes": bytes, "largest_message": largest, "changes": changes,
        "messages_by_size_up_to": sizes.iter().map(|(k, n)| (k.to_string(), json!(n))).collect::<serde_json::Map<_, _>>() });
    std::fs::write(out, serde_json::to_vec_pretty(&v)?)?;
    Ok(())
}

/// One direction of a TCP connection's bytes, by sequence number.
#[derive(Default)]
struct Way {
    isn: Option<u32>,
    pieces: BTreeMap<u64, Vec<u8>>,
}

impl Way {
    /// The bytes in order from the first after the SYN, as far as they run without a gap.
    fn stream(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for (at, p) in &self.pieces {
            let at = *at as usize;
            if at > out.len() {
                break;
            }
            if at + p.len() > out.len() {
                out.extend_from_slice(&p[out.len() - at..]);
            }
        }
        out
    }
}

/// The capture's connections to `port`: (client's port) -> (client to server, server to client).
fn connections(pcap: &[u8], port: u16) -> anyhow::Result<BTreeMap<u16, (Way, Way)>> {
    let u32_at = |at: usize| u32::from_le_bytes(pcap[at..at + 4].try_into().unwrap());
    anyhow::ensure!(pcap.len() >= 24 && u32_at(0) == 0xa1b2_c3d4 && u32_at(20) == 1, "a classic little-endian Ethernet pcap");
    let mut conns: BTreeMap<u16, (Way, Way)> = BTreeMap::new();
    let mut at = 24;
    while at + 16 <= pcap.len() {
        let incl = u32_at(at + 8) as usize;
        let orig = u32_at(at + 12) as usize;
        let f = &pcap[at + 16..(at + 16 + incl).min(pcap.len())];
        at += 16 + incl;
        anyhow::ensure!(incl == orig, "a frame cut short in the capture ({incl} of {orig}): capture whole frames");
        if f.len() < 14 + 20 || f[12..14] != [0x08, 0x00] {
            continue;
        }
        let ip = &f[14..];
        let ihl = usize::from(ip[0] & 0x0f) * 4;
        if ip[9] != 6 || ip.len() < ihl + 20 {
            continue;
        }
        let total = usize::from(u16::from_be_bytes([ip[2], ip[3]])).min(ip.len());
        let tcp = &ip[ihl..total];
        let (sport, dport) = (u16::from_be_bytes([tcp[0], tcp[1]]), u16::from_be_bytes([tcp[2], tcp[3]]));
        let seq = u32::from_be_bytes(tcp[4..8].try_into().unwrap());
        let off = usize::from(tcp[12] >> 4) * 4;
        let syn = tcp[13] & 0x02 != 0;
        let data = &tcp[off.min(tcp.len())..];
        let (key, down) = if sport == port { (dport, true) } else if dport == port { (sport, false) } else { continue };
        let pair = conns.entry(key).or_default();
        let way = if down { &mut pair.1 } else { &mut pair.0 };
        if syn {
            way.isn = Some(seq.wrapping_add(1));
            way.pieces.clear();
            continue;
        }
        let Some(isn) = way.isn else { continue };
        if !data.is_empty() {
            let rel = u64::from(seq.wrapping_sub(isn));
            let keep = way.pieces.get(&rel).is_none_or(|p| p.len() < data.len());
            if keep {
                way.pieces.insert(rel, data.to_vec());
            }
        }
    }
    Ok(conns)
}

/// A stream's TLS records: (content type, the record whole with its header).
fn records_of(s: &[u8]) -> Vec<(u8, &[u8])> {
    let mut out = Vec::new();
    let mut at = 0;
    while at + 5 <= s.len() {
        let len = usize::from(u16::from_be_bytes([s[at + 3], s[at + 4]]));
        if at + 5 + len > s.len() {
            break;
        }
        out.push((s[at], &s[at..at + 5 + len]));
        at += 5 + len;
    }
    out
}

/// TLS 1.3's HKDF-Expand-Label with an empty context.
fn expand_label(alg: aws_lc_rs::hkdf::Algorithm, secret: &[u8], label: &str, len: usize) -> Vec<u8> {
    struct Len(usize);
    impl aws_lc_rs::hkdf::KeyType for Len {
        fn len(&self) -> usize {
            self.0
        }
    }
    let full = format!("tls13 {label}");
    let mut info = Vec::new();
    info.extend_from_slice(&(len as u16).to_be_bytes());
    info.push(full.len() as u8);
    info.extend_from_slice(full.as_bytes());
    info.push(0);
    let prk = aws_lc_rs::hkdf::Prk::new_less_safe(alg, secret);
    let mut out = vec![0u8; len];
    prk.expand(&[&info], Len(len)).unwrap().fill(&mut out).unwrap();
    out
}

/// A direction's record keys from a traffic secret, and its records' count so far.
struct Keys {
    key: aws_lc_rs::aead::LessSafeKey,
    iv: [u8; 12],
    seq: u64,
}

impl Keys {
    fn new(suite: u16, secret: &[u8]) -> anyhow::Result<Self> {
        use aws_lc_rs::aead::{AES_128_GCM, AES_256_GCM, CHACHA20_POLY1305, LessSafeKey, UnboundKey};
        use aws_lc_rs::hkdf::{HKDF_SHA256, HKDF_SHA384};
        let (aead, hkdf, len) = match suite {
            0x1301 => (&AES_128_GCM, HKDF_SHA256, 16),
            0x1302 => (&AES_256_GCM, HKDF_SHA384, 32),
            0x1303 => (&CHACHA20_POLY1305, HKDF_SHA256, 32),
            other => anyhow::bail!("cipher suite {other:#06x} isn't TLS 1.3's"),
        };
        let key = LessSafeKey::new(UnboundKey::new(aead, &expand_label(hkdf, secret, "key", len)).map_err(|_| anyhow::anyhow!("a bad key"))?);
        let iv: [u8; 12] = expand_label(hkdf, secret, "iv", 12).try_into().unwrap();
        Ok(Self { key, iv, seq: 0 })
    }

    /// A record decrypted: its inner content type and plaintext, or `None` if these keys aren't its.
    fn open(&mut self, record: &[u8]) -> Option<(u8, Vec<u8>)> {
        let mut nonce = self.iv;
        for (n, s) in nonce[4..].iter_mut().zip(self.seq.to_be_bytes()) {
            *n ^= s;
        }
        let mut buf = record[5..].to_vec();
        let plain = self.key.open_in_place(aws_lc_rs::aead::Nonce::assume_unique_for_key(nonce), aws_lc_rs::aead::Aad::from(&record[..5]), &mut buf).ok()?;
        self.seq += 1;
        let end = plain.iter().rposition(|b| *b != 0)?;
        Some((plain[end], plain[..end].to_vec()))
    }
}

fn records(pcap: &str, keylog: &str, port: u16, chunk: usize) -> anyhow::Result<()> {
    let pcap = std::fs::read(pcap)?;
    let mut secrets: BTreeMap<(String, String), Vec<u8>> = BTreeMap::new();
    for line in std::fs::read_to_string(keylog)?.lines() {
        let w: Vec<&str> = line.split_whitespace().collect();
        if w.len() == 3 {
            secrets.insert((w[0].to_owned(), w[1].to_lowercase()), hex::decode(w[2])?);
        }
    }
    let mut report = Vec::new();
    for (client_port, (up, down)) in connections(&pcap, port)? {
        let (up, down) = (up.stream(), down.stream());
        let (ups, downs) = (records_of(&up), records_of(&down));
        // The ClientHello's random, and the ServerHello's suite.
        let Some((22, hello)) = ups.first().copied() else { continue };
        let random = hex::encode(&hello[5 + 6..5 + 38]);
        let Some((22, sh)) = downs.first().copied() else { continue };
        let sid_len = usize::from(sh[5 + 38]);
        let suite = u16::from_be_bytes([sh[5 + 39 + sid_len], sh[5 + 40 + sid_len]]);
        let secret = |name: &str| secrets.get(&(name.to_owned(), random.clone())).cloned();
        let (Some(hs), Some(app)) = (secret("SERVER_HANDSHAKE_TRAFFIC_SECRET"), secret("SERVER_TRAFFIC_SECRET_0")) else {
            report.push(json!({ "client_port": client_port, "error": "no keys for this connection in the key log" }));
            continue;
        };
        let (mut hs, mut app) = (Keys::new(suite, &hs)?, Keys::new(suite, &app)?);
        let mut in_app = false;
        let (mut ws_records, mut over, mut largest, mut http, mut undecrypted) = (0u64, 0u64, 0usize, 0u64, 0u64);
        let mut by_kind: BTreeMap<u8, u64> = BTreeMap::new();
        let mut sizes: BTreeMap<usize, u64> = BTreeMap::new();
        let mut upgraded = false;
        for (kind, r) in &downs[1..] {
            if *kind != 23 {
                continue;
            }
            let opened = if in_app { app.open(r) } else { hs.open(r).or_else(|| app.open(r).inspect(|_| in_app = true)) };
            let Some((inner, plain)) = opened else {
                undecrypted += 1;
                continue;
            };
            *by_kind.entry(inner).or_default() += 1;
            if inner != 23 {
                continue;
            }
            // The 101 (and any HTTP before it) isn't the WebSocket's.
            if !upgraded {
                http += 1;
                if plain.starts_with(b"HTTP/1.1 101") {
                    upgraded = true;
                }
                continue;
            }
            ws_records += 1;
            largest = largest.max(plain.len());
            *sizes.entry(plain.len().next_power_of_two()).or_default() += 1;
            if plain.len() > chunk {
                over += 1;
            }
        }
        report.push(json!({
            "client_port": client_port, "suite": format!("{suite:#06x}"), "bytes_down": down.len(), "records_down": downs.len(),
            "undecrypted": undecrypted, "inner_kinds": by_kind.iter().map(|(k, n)| (k.to_string(), json!(n))).collect::<serde_json::Map<_, _>>(),
            "http_records": http, "upgraded": upgraded, "ws_records": ws_records, "largest_plaintext": largest, "chunk": chunk, "over_chunk": over,
            "ws_records_by_size_up_to": sizes.iter().map(|(k, n)| (k.to_string(), json!(n))).collect::<serde_json::Map<_, _>>(),
        }));
    }
    println!("{}", serde_json::to_string_pretty(&json!({ "connections": report }))?);
    Ok(())
}
