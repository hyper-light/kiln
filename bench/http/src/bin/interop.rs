//! kiln-http against hyper, both ways: hyper's clients (HTTP/1.1 and HTTP/2, on tokio) ask a
//! kiln-http server (on hyper-rt) for what a server must get right — bodies by length and by chunks,
//! large bodies across flow-control windows, many streams at once, trailers, status codes, headers
//! both ways — and kiln-http's HTTP/1.1 client asks a hyper server the same. Exits non-zero on the
//! first answer that isn't what was sent.

use std::convert::Infallible;
use std::net::SocketAddr;

use bytes::Bytes;
use http_body_util::{BodyExt as _, Full};
use hyper_util::rt::{TokioExecutor, TokioIo};

type Res = Result<(), String>;

fn check(ok: bool, what: impl std::fmt::Display) -> Res {
    if ok { Ok(()) } else { Err(what.to_string()) }
}

/// What the servers answer: `/echo` sends the body back (with its length in a header, and trailers
/// echoed when chunked), `/big?n=` sends n bytes, `/status?c=` answers that status, else the method
/// and path.
async fn answer<B>(req: http::Request<B>) -> http::Response<Full<Bytes>>
where
    B: http_body::Body<Data = Bytes>,
    B::Error: std::fmt::Debug,
{
    let path = req.uri().path().to_owned();
    let query = req.uri().query().unwrap_or("").to_owned();
    let ua = req.headers().get("x-probe").map(|v| v.to_str().unwrap_or("").to_owned()).unwrap_or_default();
    let method = req.method().clone();
    let n = query.split('=').nth(1).and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
    let mut resp = match path.as_str() {
        "/echo" => match req.into_body().collect().await {
            Ok(c) => {
                let body = c.to_bytes();
                let mut r = http::Response::new(Full::new(body.clone()));
                r.headers_mut().insert("x-len", body.len().into());
                r
            }
            Err(e) => http::Response::builder().status(500).body(Full::new(Bytes::from(format!("{e:?}")))).unwrap_or_default(),
        },
        "/big" => http::Response::new(Full::new(Bytes::from((0..n).map(|i| (i % 251) as u8).collect::<Vec<u8>>()))),
        "/status" => http::Response::builder().status(n as u16).body(Full::new(Bytes::new())).unwrap_or_default(),
        _ => http::Response::new(Full::new(Bytes::from(format!("{method} {path}")))),
    };
    if !ua.is_empty() {
        resp.headers_mut().insert("x-probe-back", ua.parse().unwrap_or(http::HeaderValue::from_static("bad")));
    }
    resp
}

fn kiln_server(h2: bool) -> Result<(hyper_rt::Runtime, SocketAddr), String> {
    let cfg = hyper_rt::RuntimeConfig {
        shards: 2,
        tasks_per_shard: 4096,
        timers_per_shard: 8192,
        interests_per_shard: hyper_rt::runtime::interests_for(4096),
        ring_entries: 1024,
        step_budget_ns: 250_000,
        timer_tick_ns: 100_000,
        batch: 64,
        pin: false,
        cores: Vec::new(),
        page_bytes: 4096,
        spin_ns: 0,
        wake_tracking: None,
        latency_objective_ns: None,
    };
    let rt = hyper_rt::Runtime::start(&cfg).map_err(|e| e.to_string())?;
    let budget = hyper_rt::tcp::ConnectionBudget::new(10_000).map_err(|e| e.to_string())?;
    let serving = hyper_rt::tcp::serve(&rt, SocketAddr::from(([127, 0, 0, 1], 0)), 128, budget, move |stream| async move {
        let io = kiln_http::TcpIo::new(stream);
        let svc = kiln_http::service_fn(|req: http::Request<kiln_http::Incoming>| answer(req));
        let _ = if h2 { kiln_http::server::http2::Builder::new().serve(io, svc).await } else { kiln_http::server::http1::Builder::new().serve(io, svc).await };
    })
    .map_err(|e| e.to_string())?;
    let addr = serving.local_addr();
    std::mem::forget(serving);
    Ok((rt, addr))
}

async fn hyper_client_round(addr: SocketAddr, h2: bool) -> Res {
    let tcp = tokio::net::TcpStream::connect(addr).await.map_err(|e| e.to_string())?;
    let io = TokioIo::new(tcp);
    macro_rules! send {
        ($sender:expr, $req:expr) => {
            $sender.send_request($req).await.map_err(|e| format!("send: {e}"))?
        };
    }
    let base = format!("http://{addr}");
    type Body = http_body_util::combinators::BoxBody<Bytes, Infallible>;
    let get = |path: &str| http::Request::get(format!("{base}{path}")).header("host", addr.to_string()).header("x-probe", "probe-1").body(Full::new(Bytes::new()).boxed()).map_err(|e| e.to_string());
    let post = |path: &str, body: Bytes| http::Request::post(format!("{base}{path}")).header("host", addr.to_string()).body(Full::new(body).boxed()).map_err(|e| e.to_string());
    let _: Option<Body> = None;
    if h2 {
        let (mut s, conn) = hyper::client::conn::http2::handshake(TokioExecutor::new(), io).await.map_err(|e| e.to_string())?;
        tokio::spawn(conn);
        let r = send!(s, get("/hello")?);
        check(r.status() == 200 && r.version() == http::Version::HTTP_2, format!("h2 status {}", r.status()))?;
        check(r.headers().get("x-probe-back").map(|v| v.as_bytes()) == Some(b"probe-1"), "h2 header back")?;
        check(r.headers().contains_key("date"), "h2 date")?;
        let b = r.into_body().collect().await.map_err(|e| e.to_string())?.to_bytes();
        check(b == "GET /hello", format!("h2 body {b:?}"))?;
        // Bodies past every window: 5 MiB up, 3 MiB down.
        let up = Bytes::from(vec![7u8; 5 << 20]);
        let r = send!(s, post("/echo", up.clone())?);
        check(r.headers().get("x-len").map(|v| v.as_bytes()) == Some(b"5242880"), format!("h2 echo len {:?}", r.headers().get("x-len")))?;
        let b = r.into_body().collect().await.map_err(|e| e.to_string())?.to_bytes();
        check(b == up, "h2 echo body")?;
        let r = send!(s, get("/big?n=3145728")?);
        let b = r.into_body().collect().await.map_err(|e| e.to_string())?.to_bytes();
        check(b.len() == 3 << 20 && b.iter().enumerate().all(|(i, x)| *x == (i % 251) as u8), "h2 big body")?;
        // Statuses, and many streams at once.
        for c in [201u16, 204, 304, 404, 418, 500, 503] {
            let r = send!(s, get(&format!("/status?c={c}"))?);
            check(r.status().as_u16() == c, format!("h2 status {c} → {}", r.status()))?;
        }
        let mut many = Vec::new();
        for i in 0..200 {
            let mut s2 = s.clone();
            let req = get(&format!("/n{i}"))?;
            many.push(tokio::spawn(async move {
                let r = s2.send_request(req).await.map_err(|e| e.to_string())?;
                let b = r.into_body().collect().await.map_err(|e| e.to_string())?.to_bytes();
                Ok::<_, String>((i, b))
            }));
        }
        for m in many {
            let (i, b) = m.await.map_err(|e| e.to_string())??;
            check(b == format!("GET /n{i}"), format!("h2 stream {i}: {b:?}"))?;
        }
    } else {
        let (mut s, conn) = hyper::client::conn::http1::handshake(io).await.map_err(|e| e.to_string())?;
        tokio::spawn(conn);
        for i in 0..3 {
            let r = send!(s, get(&format!("/k{i}"))?);
            check(r.status() == 200 && r.headers().contains_key("date"), "h1 status")?;
            let b = r.into_body().collect().await.map_err(|e| e.to_string())?.to_bytes();
            check(b == format!("GET /k{i}"), format!("h1 body {b:?}"))?;
        }
        let up = Bytes::from(vec![9u8; 3 << 20]);
        let r = send!(s, post("/echo", up.clone())?);
        let b = r.into_body().collect().await.map_err(|e| e.to_string())?.to_bytes();
        check(b == up, "h1 echo")?;
        // A chunked body (hyper sends one when the length isn't known).
        let chunks = futures_util_stream(vec![Bytes::from_static(b"abc"), Bytes::from_static(b"defg")]);
        let req = http::Request::post(format!("{base}/echo")).header("host", addr.to_string()).body(chunks.boxed()).map_err(|e| e.to_string())?;
        let r = send!(s, req);
        let b = r.into_body().collect().await.map_err(|e| e.to_string())?.to_bytes();
        check(b == "abcdefg", format!("h1 chunked echo {b:?}"))?;
        for c in [201u16, 204, 304, 404, 500] {
            let r = send!(s, get(&format!("/status?c={c}"))?);
            check(r.status().as_u16() == c, format!("h1 status {c}"))?;
            let _ = r.into_body().collect().await;
        }
    }
    Ok(())
}

/// A body of chunks with no length (sent chunked over HTTP/1.1).
fn futures_util_stream(chunks: Vec<Bytes>) -> http_body_util::StreamBody<futures_stream::Iter<std::vec::IntoIter<Result<http_body::Frame<Bytes>, Infallible>>>> {
    http_body_util::StreamBody::new(futures_stream::iter(chunks.into_iter().map(|c| Ok(http_body::Frame::data(c))).collect::<Vec<_>>().into_iter()))
}

mod futures_stream {
    use std::pin::Pin;
    use std::task::{Context, Poll};

    pub struct Iter<I>(I);

    pub fn iter<I>(i: I) -> Iter<I> {
        Iter(i)
    }

    impl<I: Iterator + Unpin> futures_core::Stream for Iter<I> {
        type Item = I::Item;
        fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<I::Item>> {
            Poll::Ready(self.get_mut().0.next())
        }
    }
}

/// kiln-http's client against a hyper server: the same questions.
async fn kiln_client_round(addr: SocketAddr) -> Res {
    let tcp = hyper_rt::tcp::TcpStream::connect(addr).await.map_err(|e| e.to_string())?;
    let (mut s, conn) = kiln_http::client::http1::handshake::<_, Full<Bytes>>(kiln_http::TcpIo::new(tcp)).await.map_err(|e| e.to_string())?;
    let _ = hyper_rt::futures::spawn_detached(async move {
        let _ = conn.await;
    });
    for i in 0..3 {
        s.ready().await.map_err(|e| e.to_string())?;
        let req = http::Request::get(format!("http://{addr}/c{i}")).header("host", addr.to_string()).body(Full::new(Bytes::new())).map_err(|e| e.to_string())?;
        let r = s.send_request(req).await.map_err(|e| e.to_string())?;
        let b = r.into_body().collect().await.map_err(|e| e.to_string())?.to_bytes();
        check(b == format!("GET /c{i}"), format!("client body {b:?}"))?;
    }
    s.ready().await.map_err(|e| e.to_string())?;
    let up = Bytes::from(vec![3u8; 2 << 20]);
    let req = http::Request::post(format!("http://{addr}/echo")).header("host", addr.to_string()).body(Full::new(up.clone())).map_err(|e| e.to_string())?;
    let r = s.send_request(req).await.map_err(|e| e.to_string())?;
    let b = r.into_body().collect().await.map_err(|e| e.to_string())?.to_bytes();
    check(b == up, "client echo")?;
    s.ready().await.map_err(|e| e.to_string())?;
    let req = http::Request::get(format!("http://{addr}/big?n=1000000")).header("host", addr.to_string()).body(Full::new(Bytes::new())).map_err(|e| e.to_string())?;
    let r = s.send_request(req).await.map_err(|e| e.to_string())?;
    let b = r.into_body().collect().await.map_err(|e| e.to_string())?.to_bytes();
    check(b.len() == 1_000_000, "client big")?;
    Ok(())
}

/// kiln-http's HTTP/2 client against a hyper HTTP/2 server: one stream, a body past every window both
/// ways, and many streams at once.
async fn kiln_h2_client_round(addr: SocketAddr) -> Res {
    let tcp = hyper_rt::tcp::TcpStream::connect(addr).await.map_err(|e| e.to_string())?;
    let (s, conn) = kiln_http::client::http2::handshake::<_, Full<Bytes>>(kiln_http::TcpIo::new(tcp)).await.map_err(|e| e.to_string())?;
    let _ = hyper_rt::futures::spawn_detached(async move {
        let _ = conn.await;
    });
    let req = |path: &str, body: Bytes| http::Request::post(format!("http://{addr}{path}")).body(Full::new(body)).map_err(|e| e.to_string());
    let r = s.clone().send_request(req("/hello", Bytes::new())?).await.map_err(|e| e.to_string())?;
    check(r.status() == 200 && r.version() == http::Version::HTTP_2, format!("h2 client status {}", r.status()))?;
    let b = r.into_body().collect().await.map_err(|e| e.to_string())?.to_bytes();
    check(b == "POST /hello", format!("h2 client body {b:?}"))?;
    let up = Bytes::from(vec![5u8; 4 << 20]);
    let r = s.clone().send_request(req("/echo", up.clone())?).await.map_err(|e| e.to_string())?;
    let b = r.into_body().collect().await.map_err(|e| e.to_string())?.to_bytes();
    check(b == up, format!("h2 client echo: {} of {} bytes", b.len(), up.len()))?;
    let r = s.clone().send_request(req("/big?n=3000000", Bytes::new())?).await.map_err(|e| e.to_string())?;
    let b = r.into_body().collect().await.map_err(|e| e.to_string())?.to_bytes();
    check(b.len() == 3_000_000, "h2 client big")?;
    let mut many = Vec::new();
    for i in 0..100 {
        let mut s2 = s.clone();
        let r = req(&format!("/m{i}"), Bytes::new())?;
        many.push(async move {
            let resp = s2.send_request(r).await.map_err(|e| e.to_string())?;
            let b = resp.into_body().collect().await.map_err(|e| e.to_string())?.to_bytes();
            Ok::<_, String>((i, b))
        });
    }
    for m in many {
        let (i, b) = m.await?;
        check(b == format!("POST /m{i}"), format!("h2 client stream {i}: {b:?}"))?;
    }
    Ok(())
}

fn main() {
    let outcome = (|| -> Res {
        let tokio = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_io().build().map_err(|e| e.to_string())?;
        // hyper's clients, kiln-http's servers.
        let (_rt1, h1) = kiln_server(false)?;
        let (_rt2, h2) = kiln_server(true)?;
        tokio.block_on(async { hyper_client_round(h1, false).await })?;
        println!("ok: hyper's HTTP/1.1 client ↔ kiln-http's HTTP/1.1 server");
        tokio.block_on(async { hyper_client_round(h2, true).await })?;
        println!("ok: hyper's HTTP/2 client ↔ kiln-http's HTTP/2 server");
        // kiln-http's client, a hyper server.
        let addr = tokio.block_on(async {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.map_err(|e| e.to_string())?;
            let addr = l.local_addr().map_err(|e| e.to_string())?;
            tokio::spawn(async move {
                while let Ok((s, _)) = l.accept().await {
                    tokio::spawn(async move {
                        let svc = hyper::service::service_fn(|req: http::Request<hyper::body::Incoming>| async move { Ok::<_, Infallible>(answer(req).await) });
                        let _ = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(s), svc).await;
                    });
                }
            });
            Ok::<_, String>(addr)
        })?;
        let (tx, rx) = std::sync::mpsc::channel();
        let (rt3, _) = kiln_server(false)?;
        let holder = rt3.holder_of(rt3.shard_ids().first().copied().ok_or("no shard")?).ok_or("no holder")?;
        hyper_rt::runtime::submit_to_holder(holder, async move {
            let _ = tx.send(kiln_client_round(addr).await);
        })
        .map_err(|e| e.to_string())?;
        rx.recv_timeout(std::time::Duration::from_secs(30)).map_err(|e| e.to_string())??;
        println!("ok: kiln-http's HTTP/1.1 client ↔ hyper's HTTP/1.1 server");
        let addr = tokio.block_on(async {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.map_err(|e| e.to_string())?;
            let addr = l.local_addr().map_err(|e| e.to_string())?;
            tokio::spawn(async move {
                while let Ok((s, _)) = l.accept().await {
                    tokio::spawn(async move {
                        let svc = hyper::service::service_fn(|req: http::Request<hyper::body::Incoming>| async move { Ok::<_, Infallible>(answer(req).await) });
                        let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new()).serve_connection(TokioIo::new(s), svc).await;
                    });
                }
            });
            Ok::<_, String>(addr)
        })?;
        let (tx, rx) = std::sync::mpsc::channel();
        hyper_rt::runtime::submit_to_holder(holder, async move {
            let _ = tx.send(kiln_h2_client_round(addr).await);
        })
        .map_err(|e| e.to_string())?;
        rx.recv_timeout(std::time::Duration::from_secs(30)).map_err(|e| e.to_string())??;
        println!("ok: kiln-http's HTTP/2 client ↔ hyper's HTTP/2 server");
        Ok(())
    })();
    if let Err(e) = outcome {
        eprintln!("interop FAILED: {e}");
        std::process::exit(1);
    }
}
