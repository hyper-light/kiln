//! The baseline: hyper 1.x on tokio's multi-threaded runtime, as a hyper or axum service is
//! usually deployed: one accept loop, a task per connection, work stealing across `--threads`
//! workers. `--proto h1` (hyper's HTTP/1.1 connection), `h2` (HTTP/2 with prior knowledge) or
//! `auto` (hyper-util's: either, by the preface; what `axum::serve` runs). `--pipeline-flush`
//! turns on hyper's batching of pipelined responses (off by default).
//!
//! Every request is answered `200 OK` with `Hello, World!` (13 bytes, plus hyper's `date`).

use std::convert::Infallible;
use std::net::SocketAddr;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Proto {
    H1,
    H2,
    Auto,
}

async fn hello(_req: Request<Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
    Ok(Response::new(Full::new(Bytes::from_static(b"Hello, World!"))))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (mut port, mut threads, mut proto, mut pipeline_flush) = (8080u16, 4usize, Proto::H1, false);
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        match k.as_str() {
            "--port" => port = it.next().ok_or("--port")?.parse()?,
            "--threads" => threads = it.next().ok_or("--threads")?.parse()?,
            "--proto" => {
                proto = match it.next().ok_or("--proto")?.as_str() {
                    "h1" => Proto::H1,
                    "h2" => Proto::H2,
                    "auto" => Proto::Auto,
                    p => return Err(format!("unknown --proto {p}").into()),
                }
            }
            "--pipeline-flush" => pipeline_flush = true,
            other => return Err(format!("unknown flag {other}").into()),
        }
    }
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(threads).enable_io().build()?;
    rt.block_on(serve(port, threads, proto, pipeline_flush))
}

async fn serve(port: u16, threads: usize, proto: Proto, pipeline_flush: bool) -> Result<(), Box<dyn std::error::Error>> {
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port))).await?;
    let name = match proto {
        Proto::H1 => "h1",
        Proto::H2 => "h2",
        Proto::Auto => "auto",
    };
    eprintln!("hyper-tokio: {name} on {}, {threads} threads, pipeline_flush {pipeline_flush}", listener.local_addr()?);
    loop {
        let (stream, _) = listener.accept().await?;
        stream.set_nodelay(true)?;
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let svc = service_fn(hello);
            let _ = match proto {
                Proto::H1 => hyper::server::conn::http1::Builder::new().pipeline_flush(pipeline_flush).serve_connection(io, svc).await.map_err(|e| e.to_string()),
                Proto::H2 => hyper::server::conn::http2::Builder::new(TokioExecutor::new()).serve_connection(io, svc).await.map_err(|e| e.to_string()),
                Proto::Auto => hyper_util::server::conn::auto::Builder::new(TokioExecutor::new()).serve_connection(io, svc).await.map_err(|e| e.to_string()),
            };
        });
    }
}
