//! HTTP/2 over real sockets on the runtime: kiln-http's client against its server, and the server
//! against frames written by hand, for what a peer gets wrong.

use std::net::SocketAddr;
use std::time::Duration;

use bytes::Bytes;
use futures::{AsyncReadExt as _, AsyncWriteExt as _};
use http::{Request, Response, StatusCode};
use http_body_util::{BodyExt as _, Full, StreamBody};
use hyper_rt::tcp::{TcpListener, TcpStream};

use super::frame;
use crate::testing::{block_on, spawn};
use crate::{Incoming, TcpIo, service_fn};

type Boxed = http_body_util::combinators::UnsyncBoxBody<Bytes, std::io::Error>;

static DROPPED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Marks its handler's end.
struct Guard;

impl Drop for Guard {
    fn drop(&mut self) {
        DROPPED.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

fn full(s: impl Into<Bytes>) -> Boxed {
    Full::new(s.into()).map_err(|never| match never {}).boxed_unsync()
}

async fn handle(req: Request<Incoming>) -> Response<Boxed> {
    let path = req.uri().path().to_owned();
    match path.as_str() {
        "/echo" => match req.into_body().collect().await {
            Ok(c) => {
                let (body, trailers) = (c.trailers().cloned(), c.to_bytes());
                let mut r = Response::new(full(trailers));
                if let Some(t) = body.and_then(|t| t.get("x-sum").cloned()) {
                    r.headers_mut().insert("x-sum-was", t);
                }
                r
            }
            Err(e) => Response::builder().status(StatusCode::BAD_REQUEST).body(full(e.to_string())).unwrap_or_default(),
        },
        "/stream" => {
            let (tx, rx) = futures::channel::mpsc::unbounded::<Result<http_body::Frame<Bytes>, std::io::Error>>();
            spawn(async move {
                for i in 0..3 {
                    let _ = tx.unbounded_send(Ok(http_body::Frame::data(Bytes::from(format!("part{i};")))));
                    let _ = hyper_rt::futures::sleep(5_000_000).await;
                }
            });
            Response::new(StreamBody::new(rx).boxed_unsync())
        }
        "/big" => Response::new(full(vec![b'z'; 3 << 20])),
        "/slow" => {
            let _ = hyper_rt::futures::sleep(30_000_000_000).await;
            Response::new(full("late"))
        }
        "/guarded" => {
            let _guard = Guard;
            let _ = hyper_rt::futures::sleep(30_000_000_000).await;
            Response::new(full("late"))
        }
        _ => Response::builder()
            .header("x-version", format!("{:?}", req.version()))
            .body(full(format!("{} {path} {}", req.method(), req.uri().authority().map_or("-", |a| a.as_str()))))
            .unwrap_or_default(),
    }
}

fn serve(builder: crate::server::http2::Builder) -> Result<SocketAddr, String> {
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)), 64).map_err(|e| e.to_string())?;
    let addr = listener.local_addr().map_err(|e| e.to_string())?;
    spawn(async move {
        while let Ok(stream) = listener.accept().await {
            spawn(async move {
                let _ = builder.serve(TcpIo::new(stream), service_fn(handle)).await;
            });
        }
    });
    Ok(addr)
}

async fn client(addr: SocketAddr) -> Result<crate::client::http2::SendRequest<Boxed>, String> {
    let tcp = TcpStream::connect(addr).await.map_err(|e| e.to_string())?;
    let (send, conn) = crate::client::http2::handshake::<_, Boxed>(TcpIo::new(tcp)).await.map_err(|e| e.to_string())?;
    spawn(async move {
        let _ = conn.await;
    });
    Ok(send)
}

fn get(path: &str) -> Result<Request<Boxed>, String> {
    Request::get(format!("http://kiln.test{path}")).body(full("")).map_err(|e| e.to_string())
}

#[test]
fn requests_and_responses_go_both_ways_many_at_once() -> Result<(), String> {
    block_on(async {
        let addr = serve(crate::server::http2::Builder::new())?;
        let mut send = client(addr).await?;
        let r = send.send_request(get("/hello")?).await.map_err(|e| e.to_string())?;
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.headers().get("x-version").map(|v| v.as_bytes()), Some(&b"HTTP/2.0"[..]));
        assert!(r.headers().contains_key("date"));
        let body = r.into_body().collect().await.map_err(|e| e.to_string())?.to_bytes();
        assert_eq!(body, "GET /hello kiln.test");
        // Fifty at once on the one connection.
        let many = futures::future::join_all((0..50).map(|i| {
            let mut s = send.clone();
            async move {
                let r = s.send_request(get(&format!("/n{i}"))?).await.map_err(|e| e.to_string())?;
                let b = r.into_body().collect().await.map_err(|e| e.to_string())?.to_bytes();
                Ok::<_, String>((i, b))
            }
        }))
        .await;
        for m in many {
            let (i, b) = m?;
            assert_eq!(b, format!("GET /n{i} kiln.test"));
        }
        Ok(())
    })?
}

#[test]
fn bodies_larger_than_every_window_arrive_whole_both_ways() -> Result<(), String> {
    block_on(async {
        let addr = serve(crate::server::http2::Builder::new())?;
        let mut send = client(addr).await?;
        let up = Bytes::from((0..(5u32 << 20)).map(|i| (i % 253) as u8).collect::<Vec<u8>>());
        let req = Request::post("http://kiln.test/echo").body(full(up.clone())).map_err(|e| e.to_string())?;
        let back = send.send_request(req).await.map_err(|e| e.to_string())?.into_body().collect().await.map_err(|e| e.to_string())?.to_bytes();
        assert!(back == up, "echoed {} of {} bytes", back.len(), up.len());
        let big = send.send_request(get("/big")?).await.map_err(|e| e.to_string())?.into_body().collect().await.map_err(|e| e.to_string())?.to_bytes();
        assert_eq!(big.len(), 3 << 20);
        // A streamed request with trailers, and a streamed response.
        let mut trailers = http::HeaderMap::new();
        trailers.insert("x-sum", http::HeaderValue::from_static("42"));
        let frames = futures::stream::iter(vec![
            Ok::<_, std::io::Error>(http_body::Frame::data(Bytes::from_static(b"ab"))),
            Ok(http_body::Frame::data(Bytes::from_static(b"cd"))),
            Ok(http_body::Frame::trailers(trailers)),
        ]);
        let req = Request::post("http://kiln.test/echo").body(StreamBody::new(frames).boxed_unsync()).map_err(|e| e.to_string())?;
        let r = send.send_request(req).await.map_err(|e| e.to_string())?;
        assert_eq!(r.headers().get("x-sum-was").map(|v| v.as_bytes()), Some(&b"42"[..]));
        assert_eq!(r.into_body().collect().await.map_err(|e| e.to_string())?.to_bytes(), "abcd");
        let s = send.send_request(get("/stream")?).await.map_err(|e| e.to_string())?.into_body().collect().await.map_err(|e| e.to_string())?.to_bytes();
        assert_eq!(s, "part0;part1;part2;");
        let head = Request::head("http://kiln.test/x").body(full("")).map_err(|e| e.to_string())?;
        let r = send.send_request(head).await.map_err(|e| e.to_string())?;
        assert!(r.into_body().collect().await.map_err(|e| e.to_string())?.to_bytes().is_empty());
        Ok(())
    })?
}

// ------------------------------------------------------------------------------------------ by hand

/// A raw HTTP/2 conversation: the preface and settings sent, then frames each way.
struct Raw {
    io: TcpIo,
    read: Vec<u8>,
}

impl Raw {
    async fn open(addr: SocketAddr, preface: bool) -> Result<Raw, String> {
        let io = TcpIo::new(TcpStream::connect(addr).await.map_err(|e| e.to_string())?);
        let mut raw = Raw { io, read: Vec::new() };
        if preface {
            let mut out = frame::PREFACE.to_vec();
            frame::put_settings(&mut out, &[]);
            raw.send(&out).await?;
        }
        Ok(raw)
    }

    async fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.io.write_all(bytes).await.map_err(|e| e.to_string())
    }

    /// The next frame from the server (head and payload); `None` once it closed.
    async fn next(&mut self) -> Result<Option<(frame::Head, Vec<u8>)>, String> {
        loop {
            if let Some(h) = frame::head(&self.read)
                && self.read.len() >= 9 + h.len
            {
                let payload = self.read.get(9..9 + h.len).unwrap_or_default().to_vec();
                self.read.drain(..9 + h.len);
                return Ok(Some((h, payload)));
            }
            let mut buf = [0u8; 16 << 10];
            let n = match hyper_rt::futures::within(5_000_000_000, self.io.read(&mut buf)).await {
                Ok(Some(r)) => r.map_err(|e| e.to_string())?,
                _ => return Err("the server said nothing for five seconds".into()),
            };
            if n == 0 {
                return Ok(None);
            }
            self.read.extend_from_slice(buf.get(..n).unwrap_or_default());
        }
    }

    /// Frames until one of `kind` arrives (or the connection ends): that one.
    async fn until(&mut self, kind: u8) -> Result<Option<(frame::Head, Vec<u8>)>, String> {
        while let Some((h, p)) = self.next().await? {
            if h.kind == kind {
                return Ok(Some((h, p)));
            }
        }
        Ok(None)
    }

    /// The GOAWAY's error code, once it comes.
    async fn goaway(&mut self) -> Result<Option<u32>, String> {
        Ok(self.until(frame::GOAWAY).await?.and_then(|(_, p)| frame::u32_at(&p, 4)))
    }
}

/// A GET's header block for `path` (HPACK with no table use).
fn block(path: &str, extra: &[(&str, &str)]) -> Vec<u8> {
    let mut b = vec![0x82, 0x86];
    b.push(0x04);
    b.push(path.len() as u8);
    b.extend_from_slice(path.as_bytes());
    b.extend_from_slice(&[0x01, 9]);
    b.extend_from_slice(b"kiln.test");
    for (n, v) in extra {
        b.push(0x00);
        b.push(n.len() as u8);
        b.extend_from_slice(n.as_bytes());
        b.push(v.len() as u8);
        b.extend_from_slice(v.as_bytes());
    }
    b
}

fn headers(out: &mut Vec<u8>, stream: u32, flags: u8, block: &[u8]) {
    frame::put_head(out, block.len(), frame::HEADERS, flags, stream);
    out.extend_from_slice(block);
}

#[test]
fn a_bad_start_is_refused() -> Result<(), String> {
    block_on(async {
        let addr = serve(crate::server::http2::Builder::new())?;
        // Not the preface: the connection just ends.
        let mut raw = Raw::open(addr, false).await?;
        raw.send(b"PRI * HTTP/2.0\r\n\r\nXX\r\n\r\n").await?;
        while raw.next().await?.is_some() {}
        // The preface, then something other than SETTINGS.
        let mut raw = Raw::open(addr, false).await?;
        let mut out = frame::PREFACE.to_vec();
        frame::put_ping(&mut out, false, [0; 8]);
        raw.send(&out).await?;
        assert_eq!(raw.goaway().await?, Some(frame::PROTOCOL_ERROR));
        Ok(())
    })?
}

#[test]
fn pings_and_settings_are_answered() -> Result<(), String> {
    block_on(async {
        let addr = serve(crate::server::http2::Builder::new())?;
        let mut raw = Raw::open(addr, true).await?;
        let (h, _) = raw.until(frame::SETTINGS).await?.ok_or("no settings")?;
        assert_eq!(h.flags & frame::ACK, 0);
        let mut out = Vec::new();
        frame::put_ping(&mut out, false, *b"12345678");
        raw.send(&out).await?;
        let (h, p) = raw.until(frame::PING).await?.ok_or("no pong")?;
        assert_eq!((h.flags & frame::ACK, p.as_slice()), (frame::ACK, &b"12345678"[..]));
        Ok(())
    })?
}

#[test]
fn framing_errors_end_the_connection() -> Result<(), String> {
    block_on(async {
        let addr = serve(crate::server::http2::Builder::new())?;
        type Case = (&'static str, fn(&mut Vec<u8>), u32);
        let cases: [Case; 5] = [
            ("a frame over 16 KiB", |o| frame::put_head(o, 20_000, frame::DATA, 0, 1), frame::FRAME_SIZE_ERROR),
            ("HEADERS on an even stream", |o| headers(o, 2, frame::END_HEADERS | frame::END_STREAM, &block("/", &[])), frame::PROTOCOL_ERROR),
            (
                "a frame between a block's pieces",
                |o| {
                    headers(o, 1, frame::END_STREAM, &block("/", &[]));
                    frame::put_ping(o, false, [0; 8]);
                },
                frame::PROTOCOL_ERROR,
            ),
            ("a connection window opened by nothing", |o| frame::put_window_update(o, 0, 0), frame::PROTOCOL_ERROR),
            ("a header block that isn't HPACK", |o| headers(o, 1, frame::END_HEADERS | frame::END_STREAM, &[0x80]), frame::COMPRESSION_ERROR),
        ];
        for (what, write, code) in cases {
            let mut raw = Raw::open(addr, true).await?;
            let mut out = Vec::new();
            write(&mut out);
            raw.send(&out).await?;
            assert_eq!(raw.goaway().await?, Some(code), "{what}");
        }
        Ok(())
    })?
}

#[test]
fn malformed_and_excess_requests_are_refused_their_stream_only() -> Result<(), String> {
    block_on(async {
        let mut b = crate::server::http2::Builder::new();
        b.max_concurrent_streams(2).max_header_list_size(1024);
        let addr = serve(b)?;
        let mut raw = Raw::open(addr, true).await?;
        let mut out = Vec::new();
        // An uppercase header name, and a connection-specific header: malformed.
        headers(&mut out, 1, frame::END_HEADERS | frame::END_STREAM, &block("/a", &[("X-Upper", "1")]));
        headers(&mut out, 3, frame::END_HEADERS | frame::END_STREAM, &block("/b", &[("connection", "close")]));
        raw.send(&out).await?;
        for want in [1, 3] {
            let (h, p) = raw.until(frame::RST_STREAM).await?.ok_or("no reset")?;
            assert_eq!((h.stream, frame::u32_at(&p, 0)), (want, Some(frame::PROTOCOL_ERROR)));
        }
        // Past the concurrency limit (two slow ones open): refused.
        let mut out = Vec::new();
        for id in [5, 7, 9] {
            headers(&mut out, id, frame::END_HEADERS | frame::END_STREAM, &block("/slow", &[]));
        }
        raw.send(&out).await?;
        let (h, p) = raw.until(frame::RST_STREAM).await?.ok_or("no refusal")?;
        assert_eq!((h.stream, frame::u32_at(&p, 0)), (9, Some(frame::REFUSED_STREAM)));
        // A header list past the limit: answered 431.
        let mut raw = Raw::open(addr, true).await?;
        let mut out = Vec::new();
        let mut big = block("/", &[]);
        big.extend_from_slice(&[0x00, 0x01, b'x']);
        super::hpack::put_int(&mut big, 0, 7, 1500);
        big.extend_from_slice("v".repeat(1500).as_bytes());
        headers(&mut out, 1, frame::END_HEADERS | frame::END_STREAM, &big);
        raw.send(&out).await?;
        let (h, p) = raw.until(frame::HEADERS).await?.ok_or("no answer")?;
        assert_eq!(h.stream, 1);
        let mut d = super::hpack::Decoder::new(4096);
        let mut fields = Vec::new();
        d.decode(&Bytes::from(p), 1 << 16, &mut fields).map_err(|_| "hpack")?;
        assert_eq!(fields.first().map(|f| f.value.clone()), Some(Bytes::from_static(b"431")));
        Ok(())
    })?
}

#[test]
fn flow_control_is_enforced_and_resets_are_bounded() -> Result<(), String> {
    block_on(async {
        let mut b = crate::server::http2::Builder::new();
        b.initial_stream_window_size(1024);
        let addr = serve(b)?;
        let mut raw = Raw::open(addr, true).await?;
        // The smaller window holds once the client has taken it (acknowledged the settings).
        raw.until(frame::SETTINGS).await?.ok_or("no settings")?;
        let mut out = Vec::new();
        frame::put_head(&mut out, 0, frame::SETTINGS, frame::ACK, 0);
        headers(&mut out, 1, frame::END_HEADERS, &block("/echo", &[]));
        frame::put_head(&mut out, 2000, frame::DATA, 0, 1);
        out.extend_from_slice(&[0u8; 2000]);
        raw.send(&out).await?;
        let (h, p) = raw.until(frame::RST_STREAM).await?.ok_or("no reset")?;
        assert_eq!((h.stream, frame::u32_at(&p, 0)), (1, Some(frame::FLOW_CONTROL_ERROR)));
        // Opening and resetting streams as fast as it can: told to calm down.
        let addr = serve(crate::server::http2::Builder::new())?;
        let mut raw = Raw::open(addr, true).await?;
        let mut out = Vec::new();
        for i in 0..600u32 {
            let id = 1 + 2 * i;
            headers(&mut out, id, frame::END_HEADERS | frame::END_STREAM, &block("/slow", &[]));
            frame::put_rst(&mut out, id, frame::CANCEL);
        }
        raw.send(&out).await?;
        assert_eq!(raw.goaway().await?, Some(frame::ENHANCE_YOUR_CALM));
        Ok(())
    })?
}

#[test]
fn a_quiet_connection_is_pinged_and_one_that_never_answers_is_closed() -> Result<(), String> {
    block_on(async {
        let mut b = crate::server::http2::Builder::new();
        b.keep_alive_interval(Some(Duration::from_millis(50))).keep_alive_timeout(Duration::from_millis(100));
        let addr = serve(b)?;
        let mut raw = Raw::open(addr, true).await?;
        let (h, _) = raw.until(frame::PING).await?.ok_or("no ping")?;
        assert_eq!(h.flags & frame::ACK, 0);
        // Never answered: the connection ends.
        while raw.next().await?.is_some() {}
        Ok(())
    })?
}

#[test]
fn a_client_that_goes_away_ends_its_requests() -> Result<(), String> {
    block_on(async {
        let addr = serve(crate::server::http2::Builder::new())?;
        let mut raw = Raw::open(addr, true).await?;
        let mut out = Vec::new();
        headers(&mut out, 1, frame::END_HEADERS | frame::END_STREAM, &block("/guarded", &[]));
        raw.send(&out).await?;
        hyper_rt::futures::sleep(20_000_000).await.map_err(|e| e.to_string())?;
        drop(raw);
        for _ in 0..200 {
            if DROPPED.load(std::sync::atomic::Ordering::SeqCst) {
                return Ok(());
            }
            hyper_rt::futures::sleep(5_000_000).await.map_err(|e| e.to_string())?;
        }
        Err("the handler outlived its client".into())
    })?
}
