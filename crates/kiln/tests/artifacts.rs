//! A job's artifacts as a real server takes them (`crate::api`'s `put_artifacts`): whole, or in
//! pieces taken only where the pieces so far end and kept only when they come to the upload's
//! size and digest, so an upload that broke off never leaves a release anything cut short.

mod common;

use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::time::Duration;

use sha2::Digest as _;

/// One request with a body of bytes: the status and the body.
fn send(port: u16, method: &str, path: &str, token: &str, body: &[u8]) -> anyhow::Result<(u16, Vec<u8>)> {
    let mut s = TcpStream::connect(("127.0.0.1", port))?;
    s.set_read_timeout(Some(Duration::from_secs(30)))?;
    write!(s, "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len())?;
    s.write_all(body)?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw)?;
    let at = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or_else(|| anyhow::anyhow!("no headers"))?;
    let head = String::from_utf8_lossy(raw.get(..at).unwrap_or_default()).to_ascii_lowercase();
    let status: u16 = head.split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    let rest = raw.get(at + 4..).unwrap_or_default().to_vec();
    let body = if head.contains("transfer-encoding: chunked") { dechunk(&rest) } else { rest };
    Ok((status, body))
}

/// A chunked body's bytes, as they were (`common::dechunk` reads text).
fn dechunk(mut s: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(eol) = s.windows(2).position(|w| w == b"\r\n") {
        let size = String::from_utf8_lossy(s.get(..eol).unwrap_or_default()).trim().to_owned();
        let n = usize::from_str_radix(size.split(';').next().unwrap_or(""), 16).unwrap_or(0);
        if n == 0 {
            break;
        }
        let data = s.get(eol + 2..eol + 2 + n).unwrap_or_default();
        out.extend_from_slice(data);
        s = s.get(eol + 2 + n + 2..).unwrap_or_default();
    }
    out
}

fn have(body: &[u8]) -> Option<u64> {
    serde_json::from_slice::<serde_json::Value>(body).ok()?.get("have")?.as_u64()
}

#[test]
fn artifacts_come_whole_or_in_pieces_and_only_whole_are_kept() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let server = common::start(dir.path(), &common::Options::default())?;
    let (port, admin) = (server.port, server.admin.clone());
    let tar: Vec<u8> = (0..300_000u32).map(|i| u8::try_from(i % 251).unwrap_or(0)).collect();
    let digest = hex::encode(sha2::Sha256::digest(&tar));
    let up = "/api/jobs/j1/artifacts/upload";

    // A server that takes pieces says how much came: none yet.
    let (st, body) = send(port, "GET", up, &admin, b"")?;
    assert_eq!((st, have(&body)), (200, Some(0)), "{}", String::from_utf8_lossy(&body));

    // Pieces where the last ended; one elsewhere is refused with where they end.
    let (first, rest) = tar.split_at(100_000);
    let (st, body) = send(port, "PUT", &format!("{up}?at=0"), &admin, first)?;
    assert_eq!((st, have(&body)), (200, Some(100_000)));
    let (st, body) = send(port, "PUT", &format!("{up}?at=50000"), &admin, rest)?;
    assert_eq!((st, have(&body)), (409, Some(100_000)), "a piece not where they end");
    let (st, body) = send(port, "PUT", &format!("{up}?at=100000"), &admin, rest)?;
    assert_eq!((st, have(&body)), (200, Some(300_000)));

    // Their end with a digest that isn't theirs: nothing kept, the pieces let go of.
    let wrong = "0".repeat(64);
    let (st, _) = send(port, "POST", &format!("{up}?size=300000&sha256={wrong}"), &admin, b"")?;
    assert_eq!(st, 409);
    let (st, _) = send(port, "GET", "/api/jobs/j1/artifacts", &admin, b"")?;
    assert_eq!(st, 404, "nothing kept from pieces that didn't come to the upload");
    let (st, body) = send(port, "GET", up, &admin, b"")?;
    assert_eq!((st, have(&body)), (200, Some(0)));

    // Again, ended with their size and digest: kept, as they came.
    for (at, piece) in [(0usize, first), (100_000, rest)] {
        let (st, _) = send(port, "PUT", &format!("{up}?at={at}"), &admin, piece)?;
        assert_eq!(st, 200);
    }
    let (st, _) = send(port, "POST", &format!("{up}?size=300000&sha256={digest}"), &admin, b"")?;
    assert_eq!(st, 204, "{}", server.log());
    let (st, got) = send(port, "GET", "/api/jobs/j1/artifacts", &admin, b"")?;
    assert_eq!(st, 200);
    assert!(got == tar, "the artifacts kept aren't what was sent ({} bytes of {})", got.len(), tar.len());

    // Whole, as runners from before send them.
    let (st, _) = send(port, "PUT", "/api/jobs/j2/artifacts/upload", &admin, &tar)?;
    assert_eq!(st, 204);
    let (st, got) = send(port, "GET", "/api/jobs/j2/artifacts", &admin, b"")?;
    assert!(st == 200 && got == tar, "{st}, {} bytes", got.len());
    Ok(())
}

/// A piece that stops short of its length is answered once nothing more comes for a while: what
/// came of it is kept, the runner told where the pieces end, and the connection closed. (A runner's
/// HTTP client could leave a piece's end unsent and wait for an answer, the server for the rest,
/// until the runner gave up on the piece and sent it again: over Wi-Fi, the Macs' release uploads
/// ran out of time.)
#[test]
fn a_piece_that_stops_short_keeps_what_came_and_is_answered() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let server = common::start(dir.path(), &common::Options::default())?;
    let (port, admin) = (server.port, server.admin.clone());
    let tar: Vec<u8> = (0..300_000u32).map(|i| u8::try_from(i % 251).unwrap_or(0)).collect();
    let up = "/api/jobs/j3/artifacts/upload";

    // A piece of 100,000 bytes, of which 60,000 come; then nothing, the connection left open.
    let mut s = TcpStream::connect(("127.0.0.1", port))?;
    s.set_read_timeout(Some(Duration::from_secs(60)))?;
    write!(s, "PUT {up}?at=0 HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {admin}\r\nContent-Type: application/octet-stream\r\nContent-Length: 100000\r\n\r\n")?;
    s.write_all(tar.get(..60_000).unwrap_or_default())?;
    let started = std::time::Instant::now();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw)?;
    let waited = started.elapsed();
    let at = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or_else(|| anyhow::anyhow!("no answer, {waited:?}: {}", String::from_utf8_lossy(&raw)))?;
    let head = String::from_utf8_lossy(raw.get(..at).unwrap_or_default()).to_ascii_lowercase();
    let rest = raw.get(at + 4..).unwrap_or_default();
    let body = if head.contains("transfer-encoding: chunked") { dechunk(rest) } else { rest.to_vec() };
    assert!(head.starts_with("http/1.1 200"), "{head}");
    assert!(head.contains("connection: close"), "the unread rest ends the connection: {head}");
    assert_eq!(have(&body), Some(60_000), "what came is kept");
    assert!(waited < Duration::from_secs(40), "answered well inside a runner's 120 s for a piece: {waited:?}");

    // The rest goes on from there, and the whole is kept as it was sent.
    let (st, body) = send(port, "PUT", &format!("{up}?at=60000"), &admin, tar.get(60_000..).unwrap_or_default())?;
    assert_eq!((st, have(&body)), (200, Some(300_000)));
    let digest = hex::encode(sha2::Sha256::digest(&tar));
    let (st, _) = send(port, "POST", &format!("{up}?size=300000&sha256={digest}"), &admin, b"")?;
    assert_eq!(st, 204, "{}", server.log());
    let (st, got) = send(port, "GET", "/api/jobs/j3/artifacts", &admin, b"")?;
    assert!(st == 200 && got == tar, "{st}, {} bytes", got.len());
    Ok(())
}
