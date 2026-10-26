//! What each HTTP stack costs per request in user space alone: its HTTP/1.1 server over an in-memory
//! transport that hands it the same pipelined batch on every read (16 GETs, as wrk's pipelining
//! sends) and takes every write, polled on one thread with no runtime and no system calls. kiln-http
//! and hyper serve the same handler; `--batch` sets the requests per read.

use std::convert::Infallible;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use std::time::Instant;

use bytes::Bytes;
use http_body_util::Full;

/// The transport: every other read is a batch, the reads between are "nothing yet" (so one poll of
/// the server is one batch); writes are counted and dropped.
struct Mock {
    batch: Vec<u8>,
    fresh: bool,
    written: usize,
}

impl Mock {
    fn new(batch: &[u8]) -> Mock {
        Mock { batch: batch.to_vec(), fresh: true, written: 0 }
    }

    fn read_into(&mut self, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<usize> {
        if !self.fresh {
            self.fresh = true;
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        self.fresh = false;
        let n = self.batch.len().min(buf.len());
        buf[..n].copy_from_slice(&self.batch[..n]);
        Poll::Ready(n)
    }
}

impl futures_io::AsyncRead for Mock {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<io::Result<usize>> {
        self.get_mut().read_into(cx, buf).map(Ok)
    }
}

impl futures_io::AsyncWrite for Mock {
    fn poll_write(self: Pin<&mut Self>, _cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        self.get_mut().written += buf.len();
        Poll::Ready(Ok(buf.len()))
    }
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl hyper::rt::Read for Mock {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, mut buf: hyper::rt::ReadBufCursor<'_>) -> Poll<io::Result<()>> {
        let mut tmp = [0u8; 64 << 10];
        let want = buf.remaining().min(tmp.len());
        match self.get_mut().read_into(cx, &mut tmp[..want]) {
            Poll::Ready(n) => {
                buf.put_slice(&tmp[..n]);
                Poll::Ready(Ok(()))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl hyper::rt::Write for Mock {
    fn poll_write(self: Pin<&mut Self>, _cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        self.get_mut().written += buf.len();
        Poll::Ready(Ok(buf.len()))
    }
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// Polls `fut` `polls` times: nanoseconds per request.
fn run<F: Future>(fut: F, polls: usize, per_poll: usize) -> f64 {
    let mut fut = std::pin::pin!(fut);
    let mut cx = Context::from_waker(Waker::noop());
    for _ in 0..1000 {
        let _ = fut.as_mut().poll(&mut cx);
    }
    let t = Instant::now();
    for _ in 0..polls {
        let _ = fut.as_mut().poll(&mut cx);
    }
    t.elapsed().as_nanos() as f64 / (polls * per_poll) as f64
}

fn main() {
    let mut batch = 16usize;
    let mut polls = 100_000usize;
    let mut trials = 15usize;
    let mut only = String::new();
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        match k.as_str() {
            "--batch" => batch = it.next().and_then(|v| v.parse().ok()).unwrap_or(batch),
            "--polls" => polls = it.next().and_then(|v| v.parse().ok()).unwrap_or(polls),
            "--trials" => trials = it.next().and_then(|v| v.parse().ok()).unwrap_or(trials),
            "--only" => only = it.next().unwrap_or_default(),
            _ => {}
        }
    }
    if std::env::args().any(|a| a == "--sizes") {
        let svc = kiln_http::service_fn(|_req: http::Request<kiln_http::Incoming>| async { http::Response::new(Full::new(Bytes::from_static(b"Hello, World!"))) });
        let h1 = kiln_http::server::http1::Builder::new().serve(Mock::new(b""), svc);
        let svc = kiln_http::service_fn(|_req: http::Request<kiln_http::Incoming>| async { http::Response::new(Full::new(Bytes::from_static(b"Hello, World!"))) });
        let h2 = kiln_http::server::http2::Builder::new().serve(Mock::new(b""), svc);
        let svc = hyper::service::service_fn(|_req: http::Request<hyper::body::Incoming>| async { Ok::<_, Infallible>(http::Response::new(Full::new(Bytes::from_static(b"Hello, World!")))) });
        let hy = hyper::server::conn::http1::Builder::new().serve_connection(Mock::new(b""), svc);
        println!("{{\"kiln_h1_future\":{},\"kiln_h2_future\":{},\"hyper_h1_future\":{}}}", std::mem::size_of_val(&h1), std::mem::size_of_val(&h2), std::mem::size_of_val(&hy));
        return;
    }
    let one = b"GET / HTTP/1.1\r\nHost: 127.0.0.1:8080\r\n\r\n";
    let reqs = one.repeat(batch);
    let polls = (polls * 16 / batch.max(1)).max(1);

    // The best of interleaved trials: a shared machine's noise only ever adds.
    let (mut kiln, mut hyper) = (f64::MAX, f64::MAX);
    for _ in 0..trials {
        if only != "hyper" {
            kiln = kiln.min({
                let mut b = kiln_http::server::http1::Builder::new();
                b.header_read_timeout(None);
                let svc = kiln_http::service_fn(|_req: http::Request<kiln_http::Incoming>| async { http::Response::new(Full::new(Bytes::from_static(b"Hello, World!"))) });
                run(b.serve(Mock::new(&reqs), svc), polls, batch)
            });
        }
        if only != "kiln" {
            hyper = hyper.min({
                let svc = hyper::service::service_fn(|_req: http::Request<hyper::body::Incoming>| async { Ok::<_, Infallible>(http::Response::new(Full::new(Bytes::from_static(b"Hello, World!")))) });
                let conn = hyper::server::conn::http1::Builder::new().pipeline_flush(true).serve_connection(Mock::new(&reqs), svc);
                run(conn, polls, batch)
            });
        }
    }
    println!("{{\"batch\":{batch},\"kiln_http_ns\":{kiln:.1},\"hyper_ns\":{hyper:.1},\"ratio\":{:.2}}}", hyper / kiln);
}
