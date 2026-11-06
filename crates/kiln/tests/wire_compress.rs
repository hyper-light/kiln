//! What the browser UI receives goes compressed when the browser can take it (W21-R15): a real kiln
//! server's API answers brotli or gzip to an `Accept-Encoding` that takes it, with a weak `ETag` an
//! unchanged poll gets a `304` for; and its sockets (the events socket here) agree
//! permessage-deflate with a client that offers it, compressing what they send and inflating what
//! comes compressed. A client that asks for neither gets what it always did.
//!
//! `cargo test -p kiln --test wire_compress`

mod common;

use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use common::{Options, Server};
use serde_json::Value;

/// A response: its status line and headers (names lowercased), and its body.
struct Answer {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Answer {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
}

fn get(srv: &Server, path: &str, headers: &str) -> anyhow::Result<Answer> {
    let mut s = TcpStream::connect(("127.0.0.1", srv.port))?;
    s.set_read_timeout(Some(Duration::from_secs(30)))?;
    write!(s, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nAuthorization: Bearer {}\r\n{headers}\r\n", srv.admin)?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw)?;
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or_else(|| anyhow::anyhow!("no header end"))?;
    let head = String::from_utf8_lossy(raw.get(..split).unwrap_or_default()).into_owned();
    let mut lines = head.lines();
    let status = lines.next().and_then(|l| l.split_whitespace().nth(1)).and_then(|c| c.parse().ok()).unwrap_or(0);
    let headers: Vec<(String, String)> = lines.filter_map(|l| l.split_once(':')).map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_owned())).collect();
    let mut body = raw.get(split + 4..).unwrap_or_default().to_vec();
    if headers.iter().any(|(n, v)| n == "transfer-encoding" && v.eq_ignore_ascii_case("chunked")) {
        body = common::dechunk(&String::from_utf8_lossy(&body)).into_bytes();
    }
    Ok(Answer { status, headers, body })
}

#[test]
fn w21_r15_api_answers_compress_and_validate() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let srv = common::start(dir.path(), &Options::default())?;
    // The operations' catalog: tens of KiB of JSON, the same each time it's asked.
    let plain = get(&srv, "/api/ops", "")?;
    anyhow::ensure!(plain.status == 200, "{}", String::from_utf8_lossy(&plain.body));
    assert!(plain.header("content-encoding").is_none(), "asked for nothing, got {:?}", plain.header("content-encoding"));
    assert_eq!(plain.header("vary"), Some("Accept-Encoding"));
    assert_eq!(plain.header("cache-control"), Some("private, no-cache"));
    let etag = plain.header("etag").ok_or_else(|| anyhow::anyhow!("no ETag"))?.to_owned();
    assert!(etag.starts_with("W/\""), "{etag}");
    let catalog: Value = serde_json::from_slice(&plain.body)?;
    // For the codecs' timings, taken in a release build (`crate::http::compress::GZIP_LEVEL`).
    if let Ok(dump) = std::env::var("KILN_WIRE_DUMP") {
        std::fs::create_dir_all(&dump)?;
        std::fs::write(std::path::Path::new(&dump).join("ops.json"), &plain.body)?;
    }

    let br = get(&srv, "/api/ops", "Accept-Encoding: gzip, deflate, br, zstd\r\n")?;
    assert_eq!(br.header("content-encoding"), Some("br"));
    let mut back = Vec::new();
    brotli::BrotliDecompress(&mut br.body.as_slice(), &mut back)?;
    assert_eq!(serde_json::from_slice::<Value>(&back)?, catalog);
    assert_eq!(br.header("etag"), Some(etag.as_str()));

    let gz = get(&srv, "/api/ops", "Accept-Encoding: gzip\r\n")?;
    assert_eq!(gz.header("content-encoding"), Some("gzip"));
    let mut back = Vec::new();
    flate2::read::GzDecoder::new(gz.body.as_slice()).read_to_end(&mut back)?;
    assert_eq!(serde_json::from_slice::<Value>(&back)?, catalog);
    println!("GET /api/ops: {} B as it is, {} B gzip, {} B brotli", plain.body.len(), gz.body.len(), br.body.len());
    assert!(br.body.len() * 4 < plain.body.len());

    // An unchanged poll: 304, no body, the validator again.
    let again = get(&srv, "/api/ops", &format!("Accept-Encoding: br\r\nIf-None-Match: {etag}\r\n"))?;
    assert_eq!(again.status, 304);
    assert!(again.body.is_empty());
    assert_eq!(again.header("etag"), Some(etag.as_str()));
    assert!(again.header("content-encoding").is_none());
    Ok(())
}

/// A frame as a client reads it: (FIN, RSV1, opcode, payload).
fn frame(s: &mut TcpStream) -> anyhow::Result<(bool, bool, u8, Vec<u8>)> {
    let mut h = [0u8; 2];
    s.read_exact(&mut h)?;
    let [b0, b1] = h;
    anyhow::ensure!(b1 & 0x80 == 0, "a server frame is unmasked");
    let len = match b1 & 0x7f {
        126 => {
            let mut l = [0u8; 2];
            s.read_exact(&mut l)?;
            usize::from(u16::from_be_bytes(l))
        }
        127 => {
            let mut l = [0u8; 8];
            s.read_exact(&mut l)?;
            usize::try_from(u64::from_be_bytes(l))?
        }
        n => usize::from(n),
    };
    let mut payload = vec![0u8; len];
    s.read_exact(&mut payload)?;
    Ok((b0 & 0x80 != 0, b0 & 0x40 != 0, b0 & 0x0f, payload))
}

/// A client's frame, masked; RSV1 when `compressed`.
fn send(s: &mut TcpStream, compressed: bool, opcode: u8, payload: &[u8]) -> anyhow::Result<()> {
    let key = [0x11u8, 0x22, 0x33, 0x44];
    let mut f = vec![0x80 | if compressed { 0x40 } else { 0 } | opcode];
    match payload.len() {
        n if n < 126 => f.push(0x80 | u8::try_from(n)?),
        n => {
            f.push(0x80 | 126);
            f.extend(u16::try_from(n)?.to_be_bytes());
        }
    }
    f.extend(key);
    f.extend(payload.iter().zip(key.iter().cycle()).map(|(b, k)| b ^ k));
    s.write_all(&f)?;
    Ok(())
}

/// Opens the events socket, offering `extensions`: the stream and the 101's head.
fn open(srv: &Server, extensions: &str) -> anyhow::Result<(TcpStream, String)> {
    let mut s = TcpStream::connect(("127.0.0.1", srv.port))?;
    s.set_read_timeout(Some(Duration::from_secs(30)))?;
    write!(
        s,
        "GET /api/events HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n{extensions}\r\n",
        srv.admin
    )?;
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        s.read_exact(&mut b)?;
        head.extend_from_slice(&b);
    }
    let head = String::from_utf8_lossy(&head).to_ascii_lowercase();
    anyhow::ensure!(head.starts_with("http/1.1 101"), "not upgraded: {head}");
    Ok((s, head))
}

/// The first text message within 20 s that `want` holds for: (it came compressed, its JSON).
fn message_where(s: &mut TcpStream, inflater: &mut flate2::Decompress, want: impl Fn(&Value) -> bool) -> anyhow::Result<(bool, Value)> {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        let (fin, rsv1, op, payload) = frame(s)?;
        anyhow::ensure!(fin, "kiln sends its messages whole");
        if op != 1 {
            continue;
        }
        let text = if rsv1 {
            let mut input = payload;
            input.extend_from_slice(&[0, 0, 0xff, 0xff]);
            let (mut out, mut at) = (Vec::with_capacity(input.len() * 8 + 1024), 0usize);
            loop {
                let before = inflater.total_in();
                inflater.decompress_vec(input.get(at..).unwrap_or_default(), &mut out, flate2::FlushDecompress::Sync)?;
                at += usize::try_from(inflater.total_in() - before)?;
                if at >= input.len() && out.len() < out.capacity() {
                    break;
                }
                out.reserve(out.capacity().max(1024));
            }
            out
        } else {
            payload
        };
        let v: Value = serde_json::from_slice(&text)?;
        if want(&v) {
            return Ok((rsv1, v));
        }
    }
    anyhow::bail!("no such message within 20 s")
}

#[test]
fn w21_r15_sockets_agree_permessage_deflate_with_who_offers_it() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let srv = common::start(dir.path(), &Options::default())?;
    let refused = |v: &Value| v.get("type").and_then(Value::as_str) == Some("refused");

    // Offered as Chromium offers it: agreed, its messages compressed onto one window.
    let (mut s, head) = open(&srv, "Sec-WebSocket-Extensions: permessage-deflate; client_max_window_bits\r\n")?;
    assert!(head.contains("sec-websocket-extensions: permessage-deflate\r\n"), "{head}");
    let mut inflater = flate2::Decompress::new(false);
    // Something it can't read, sent compressed (RSV1) as a browser would: answered `refused`, compressed.
    let mut deflater = flate2::Compress::new(flate2::Compression::default(), false);
    for _ in 0..2 {
        let mut packed = Vec::with_capacity(256);
        deflater.compress_vec(br#"{"t":"hologram"}"#, &mut packed, flate2::FlushCompress::Sync)?;
        packed.truncate(packed.len().saturating_sub(4));
        send(&mut s, true, 1, &packed)?;
        let (compressed, v) = message_where(&mut s, &mut inflater, refused)?;
        assert!(compressed, "{v}");
    }

    // Offered nothing: no extension, and the same answer uncompressed.
    let (mut s, head) = open(&srv, "")?;
    assert!(!head.contains("sec-websocket-extensions"), "{head}");
    send(&mut s, false, 1, br#"{"t":"hologram"}"#)?;
    let (compressed, _) = message_where(&mut s, &mut flate2::Decompress::new(false), refused)?;
    assert!(!compressed);
    Ok(())
}

/// What a real Chromium does with a kiln server (not production: one of the test's own): it agrees
/// permessage-deflate on the events socket (`WebSocket.extensions`), its fetches come compressed
/// (`encodedBodySize` under `decodedBodySize`), and a fetch of an unchanged answer revalidates with
/// a 304 the page never sees (a body read from the cache, a transfer of headers alone). Run where
/// Playwright is: `KILN_PLAYWRIGHT=<a node_modules holding playwright> cargo test -p kiln --test
/// wire_compress -- --ignored`.
#[test]
#[ignore = "needs Playwright and its Chromium (KILN_PLAYWRIGHT)"]
fn w21_r15_chromium_takes_compressed_answers_and_sockets() -> anyhow::Result<()> {
    let modules = std::env::var("KILN_PLAYWRIGHT").map_err(|_| anyhow::anyhow!("KILN_PLAYWRIGHT names a node_modules holding playwright"))?;
    let dir = tempfile::tempdir()?;
    let srv = common::start(dir.path(), &Options::default())?;
    let script = dir.path().join("check.cjs");
    std::fs::write(&script, BROWSER_CHECK)?;
    let out = std::process::Command::new("node")
        .arg(&script)
        .env("NODE_PATH", &modules)
        .env("KILN_URL", format!("http://127.0.0.1:{}", srv.port))
        .env("KILN_TOKEN", &srv.admin)
        .output()?;
    let said = String::from_utf8_lossy(&out.stdout);
    println!("{said}{}", String::from_utf8_lossy(&out.stderr));
    anyhow::ensure!(out.status.success(), "the browser check failed");
    let v: Value = serde_json::from_str(said.lines().last().unwrap_or_default())?;
    assert_eq!(common::at(&v, &["extensions"]), "permessage-deflate");
    assert_eq!(common::at(&v, &["refused"]), true);
    assert_eq!(common::at(&v, &["encoding"]), "br");
    assert!(common::at(&v, &["first", "encoded"]).as_f64().unwrap_or(f64::MAX) * 4.0 < common::at(&v, &["first", "decoded"]).as_f64().unwrap_or(0.0), "{v}");
    assert_eq!(common::at(&v, &["same"]), true);
    assert!(common::at(&v, &["again", "transfer"]).as_f64().unwrap_or(f64::MAX) < common::at(&v, &["first", "transfer"]).as_f64().unwrap_or(0.0), "{v}");
    Ok(())
}

/// The page's part of the browser check: sign in with the token, fetch the operations' catalog
/// twice, open the events socket and send it something it can't read.
const BROWSER_CHECK: &str = r#"
const { chromium } = require("playwright");
(async () => {
  const browser = await chromium.launch();
  const page = await browser.newPage();
  const encodings = [];
  page.on("response", (r) => { if (r.url().endsWith("/api/ops")) encodings.push([r.status(), r.headers()["content-encoding"] || null]); });
  await page.goto(process.env.KILN_URL + "/api/auth/methods");
  const got = await page.evaluate(async (token) => {
    await fetch("/api/auth/login", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ token }) });
    const timing = (n) => { const e = performance.getEntriesByType("resource").filter((r) => r.name.endsWith("/api/ops"))[n]; return { transfer: e.transferSize, encoded: e.encodedBodySize, decoded: e.decodedBodySize }; };
    const a = await (await fetch("/api/ops")).text();
    const b = await (await fetch("/api/ops")).text();
    const sock = await new Promise((done, fail) => {
      const ws = new WebSocket(location.origin.replace("http", "ws") + "/api/events");
      ws.onerror = () => fail(new Error("socket error"));
      ws.onopen = () => ws.send('{"t":"hologram"}');
      ws.onmessage = (m) => { const v = JSON.parse(m.data); if (v.type === "refused") { done({ extensions: ws.extensions, refused: true }); ws.close(); } };
      setTimeout(() => fail(new Error("no refused frame")), 20000);
    });
    return { ...sock, same: a === b && a.length > 0, first: timing(0), again: timing(1) };
  }, process.env.KILN_TOKEN);
  got.encoding = encodings.length ? encodings[0][1] : null;
  got.statuses = encodings.map((e) => e[0]);
  await browser.close();
  console.log(JSON.stringify(got));
})().catch((e) => { console.error(e); process.exit(1); });
"#;
