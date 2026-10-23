//! The HTTP/1.1 server and client over real sockets on the runtime: what goes over the wire, byte
//! for byte where it matters.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use futures::{AsyncReadExt as _, AsyncWriteExt as _, StreamExt as _};
use http::{Request, Response, StatusCode};
use http_body_util::{BodyExt as _, Full, StreamBody};
use hyper_rt::tcp::{TcpListener, TcpStream};

use crate::testing::{block_on, spawn};
use crate::{Incoming, TcpIo, service_fn, upgrade};

type Boxed = http_body_util::combinators::UnsyncBoxBody<Bytes, std::io::Error>;

fn full(s: impl Into<Bytes>) -> Boxed {
    Full::new(s.into()).map_err(|never| match never {}).boxed_unsync()
}

/// What the test handler does.
async fn handle(req: Request<Incoming>) -> Response<Boxed> {
    match req.uri().path() {
        "/echo" => match req.into_body().collect().await {
            Ok(b) => Response::new(full(b.to_bytes())),
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
        "/ws" => {
            let on = upgrade::on(req);
            spawn(async move {
                if let Ok(mut up) = on.await {
                    let mut buf = [0u8; 64];
                    while let Ok(n) = up.read(&mut buf).await {
                        if n == 0 || up.write_all(buf.get(..n).unwrap_or_default()).await.is_err() {
                            break;
                        }
                    }
                }
            });
            Response::builder().status(StatusCode::SWITCHING_PROTOCOLS).header("upgrade", "echo").header("connection", "upgrade").body(full("")).unwrap_or_default()
        }
        path => Response::new(full(format!("{} {path}", req.method()))),
    }
}

/// A server on a free port, serving `handle` with `builder`.
fn serve(builder: crate::server::http1::Builder) -> Result<SocketAddr, String> {
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

async fn connect(addr: SocketAddr) -> Result<TcpIo, String> {
    TcpStream::connect(addr).await.map(TcpIo::new).map_err(|e| e.to_string())
}

/// Reads until `done` says what was read is enough, or the stream ends.
async fn read_until(io: &mut TcpIo, done: impl Fn(&[u8]) -> bool) -> Result<Vec<u8>, String> {
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    while !done(&got) {
        let n = io.read(&mut buf).await.map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        got.extend_from_slice(buf.get(..n).unwrap_or_default());
    }
    Ok(got)
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

#[test]
fn requests_on_one_connection_are_answered_in_order_pipelined_or_not() -> Result<(), String> {
    block_on(async {
        let addr = serve(crate::server::http1::Builder::new())?;
        let mut io = connect(addr).await?;
        io.write_all(b"GET /a HTTP/1.1\r\nHost: t\r\n\r\n").await.map_err(|e| e.to_string())?;
        let one = read_until(&mut io, |g| g.ends_with(b"GET /a")).await?;
        let one = text(&one);
        assert!(one.starts_with("HTTP/1.1 200 OK\r\ncontent-length: 6\r\ndate: "), "{one}");
        // Three at once, answered in order (and together).
        io.write_all(b"GET /b HTTP/1.1\r\nHost: t\r\n\r\nGET /c HTTP/1.1\r\nHost: t\r\n\r\nPOST /echo HTTP/1.1\r\nHost: t\r\nContent-Length: 5\r\n\r\nhello").await.map_err(|e| e.to_string())?;
        let three = read_until(&mut io, |g| g.ends_with(b"hello")).await?;
        let three = text(&three);
        let (b, c, h) = (three.find("GET /b"), three.find("GET /c"), three.find("hello"));
        assert!(b < c && c < h && b.is_some(), "{three}");
        Ok(())
    })?
}

#[test]
fn bodies_arrive_whole_by_length_and_by_chunks_and_after_continue() -> Result<(), String> {
    block_on(async {
        let addr = serve(crate::server::http1::Builder::new())?;
        let mut io = connect(addr).await?;
        io.write_all(b"POST /echo HTTP/1.1\r\nHost: t\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n").await.map_err(|e| e.to_string())?;
        hyper_rt::futures::sleep(10_000_000).await.map_err(|e| e.to_string())?;
        io.write_all(b"6\r\n world\r\n0\r\n\r\n").await.map_err(|e| e.to_string())?;
        let got = text(&read_until(&mut io, |g| g.ends_with(b"hello world")).await?);
        assert!(got.contains("content-length: 11"), "{got}");
        // The body waits for leave to come.
        io.write_all(b"PUT /echo HTTP/1.1\r\nHost: t\r\nExpect: 100-continue\r\nContent-Length: 3\r\n\r\n").await.map_err(|e| e.to_string())?;
        let cont = text(&read_until(&mut io, |g| g.ends_with(b"\r\n\r\n")).await?);
        assert_eq!(cont, "HTTP/1.1 100 Continue\r\n\r\n");
        io.write_all(b"abc").await.map_err(|e| e.to_string())?;
        let got = text(&read_until(&mut io, |g| g.ends_with(b"abc")).await?);
        assert!(got.starts_with("HTTP/1.1 200 OK"), "{got}");
        // A large body, in many reads.
        let big = vec![b'x'; 300_000];
        let head = format!("POST /echo HTTP/1.1\r\nHost: t\r\nContent-Length: {}\r\n\r\n", big.len());
        io.write_all(head.as_bytes()).await.map_err(|e| e.to_string())?;
        io.write_all(&big).await.map_err(|e| e.to_string())?;
        let got = read_until(&mut io, |g| g.len() >= 300_000 && g.ends_with(b"xxx")).await?;
        assert!(text(got.get(..80).unwrap_or_default()).contains("content-length: 300000"));
        Ok(())
    })?
}

#[test]
fn a_streamed_response_is_chunked_and_arrives_as_it_is_made() -> Result<(), String> {
    block_on(async {
        let addr = serve(crate::server::http1::Builder::new())?;
        let mut io = connect(addr).await?;
        io.write_all(b"GET /stream HTTP/1.1\r\nHost: t\r\n\r\n").await.map_err(|e| e.to_string())?;
        let got = text(&read_until(&mut io, |g| g.ends_with(b"0\r\n\r\n")).await?);
        assert!(got.contains("transfer-encoding: chunked"), "{got}");
        assert!(got.ends_with("6\r\npart0;\r\n6\r\npart1;\r\n6\r\npart2;\r\n0\r\n\r\n"), "{got}");
        // HTTP/1.0 has no chunks: the body runs to the close.
        let mut io = connect(addr).await?;
        io.write_all(b"GET /stream HTTP/1.0\r\n\r\n").await.map_err(|e| e.to_string())?;
        let got = text(&read_until(&mut io, |_| false).await?);
        assert!(got.contains("connection: close") && got.ends_with("\r\n\r\npart0;part1;part2;"), "{got}");
        Ok(())
    })?
}

#[test]
fn connections_close_when_asked_when_broken_and_when_idle() -> Result<(), String> {
    block_on(async {
        let mut b = crate::server::http1::Builder::new();
        b.header_read_timeout(Some(Duration::from_millis(100)));
        let addr = serve(b)?;
        let mut io = connect(addr).await?;
        io.write_all(b"GET /x HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n").await.map_err(|e| e.to_string())?;
        let got = text(&read_until(&mut io, |_| false).await?);
        assert!(got.contains("connection: close") && got.ends_with("GET /x"), "{got}");
        let mut io = connect(addr).await?;
        io.write_all(b"GET / HTTP/1.1\r\nBad Header: x\r\n\r\n").await.map_err(|e| e.to_string())?;
        let got = text(&read_until(&mut io, |_| false).await?);
        assert!(got.starts_with("HTTP/1.1 400 Bad Request\r\n") && got.contains("connection: close"), "{got}");
        // An idle connection closes once its head is late.
        let mut io = connect(addr).await?;
        let started = std::time::Instant::now();
        let got = read_until(&mut io, |_| false).await?;
        assert!(got.is_empty() && started.elapsed() >= Duration::from_millis(90) && started.elapsed() < Duration::from_secs(5));
        Ok(())
    })?
}

#[test]
fn an_upgrade_hands_the_connection_over() -> Result<(), String> {
    block_on(async {
        let addr = serve(crate::server::http1::Builder::new())?;
        let mut io = connect(addr).await?;
        io.write_all(b"GET /ws HTTP/1.1\r\nHost: t\r\nConnection: Upgrade\r\nUpgrade: echo\r\n\r\nearly").await.map_err(|e| e.to_string())?;
        let got = text(&read_until(&mut io, |g| g.ends_with(b"early")).await?);
        assert!(got.starts_with("HTTP/1.1 101 Switching Protocols\r\n") && got.contains("\r\n\r\nearly"), "{got}");
        io.write_all(b"after").await.map_err(|e| e.to_string())?;
        assert_eq!(read_until(&mut io, |g| g.ends_with(b"after")).await?, b"after");
        Ok(())
    })?
}

#[test]
fn a_client_that_goes_away_ends_its_request() -> Result<(), String> {
    static DROPPED: AtomicBool = AtomicBool::new(false);
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            DROPPED.store(true, Ordering::SeqCst);
        }
    }
    block_on(async {
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)), 8).map_err(|e| e.to_string())?;
        let addr = listener.local_addr().map_err(|e| e.to_string())?;
        spawn(async move {
            if let Ok(stream) = listener.accept().await {
                let svc = service_fn(|_req: Request<Incoming>| async {
                    let _guard = Guard;
                    let _ = hyper_rt::futures::sleep(30_000_000_000).await;
                    Response::new(full("late"))
                });
                let _ = crate::server::http1::Builder::new().serve(TcpIo::new(stream), svc).await;
            }
        });
        let mut io = connect(addr).await?;
        io.write_all(b"GET /long HTTP/1.1\r\nHost: t\r\n\r\n").await.map_err(|e| e.to_string())?;
        hyper_rt::futures::sleep(20_000_000).await.map_err(|e| e.to_string())?;
        drop(io);
        for _ in 0..200 {
            if DROPPED.load(Ordering::SeqCst) {
                return Ok(());
            }
            hyper_rt::futures::sleep(5_000_000).await.map_err(|e| e.to_string())?;
        }
        Err("the handler outlived its client".into())
    })?
}

#[test]
fn the_client_sends_receives_and_keeps_its_connection() -> Result<(), String> {
    block_on(async {
        let addr = serve(crate::server::http1::Builder::new())?;
        let (mut send, conn) = crate::client::http1::handshake::<_, Boxed>(connect(addr).await?).await.map_err(|e| e.to_string())?;
        let ended = Arc::new(AtomicUsize::new(0));
        let e2 = Arc::clone(&ended);
        spawn(async move {
            let _ = conn.await;
            e2.fetch_add(1, Ordering::SeqCst);
        });
        for i in 0..3 {
            send.ready().await.map_err(|e| e.to_string())?;
            let req = Request::post("http://t/echo").header("host", "t").body(full(format!("body {i}"))).map_err(|e| e.to_string())?;
            let resp = send.send_request(req).await.map_err(|e| e.to_string())?;
            assert_eq!(resp.status(), StatusCode::OK);
            let body = resp.into_body().collect().await.map_err(|e| e.to_string())?.to_bytes();
            assert_eq!(body, format!("body {i}"));
        }
        // A streamed request body goes chunked; a streamed response comes back whole.
        send.ready().await.map_err(|e| e.to_string())?;
        let chunks = futures::stream::iter(["a", "b", "c"]).map(|s| Ok::<_, std::io::Error>(http_body::Frame::data(Bytes::from(s))));
        let req = Request::post("http://t/echo").header("host", "t").body(StreamBody::new(chunks).boxed_unsync()).map_err(|e| e.to_string())?;
        let body = send.send_request(req).await.map_err(|e| e.to_string())?.into_body().collect().await.map_err(|e| e.to_string())?.to_bytes();
        assert_eq!(body, "abc");
        send.ready().await.map_err(|e| e.to_string())?;
        let req = Request::get("http://t/stream").header("host", "t").body(full("")).map_err(|e| e.to_string())?;
        let resp = send.send_request(req).await.map_err(|e| e.to_string())?;
        assert!(!send.is_ready(), "busy while the body streams");
        let body = resp.into_body().collect().await.map_err(|e| e.to_string())?.to_bytes();
        assert_eq!(body, "part0;part1;part2;");
        // Upgraded, the client's connection is handed over too.
        send.ready().await.map_err(|e| e.to_string())?;
        let req = Request::get("http://t/ws").header("host", "t").header("connection", "upgrade").header("upgrade", "echo").body(full("")).map_err(|e| e.to_string())?;
        let mut resp = send.send_request(req).await.map_err(|e| e.to_string())?;
        assert_eq!(resp.status(), StatusCode::SWITCHING_PROTOCOLS);
        let mut up = upgrade::on(&mut resp).await.map_err(|e| e.to_string())?;
        up.write_all(b"ping").await.map_err(|e| e.to_string())?;
        let mut buf = [0u8; 4];
        up.read_exact(&mut buf).await.map_err(|e| e.to_string())?;
        assert_eq!(&buf, b"ping");
        assert!(send.is_closed());
        Ok(())
    })?
}
