//! HTTP/1.1 server: hyper's runtime-agnostic connection state machine, driven on hyper-rt. Each
//! accepted connection becomes one hyper-rt task; upgrades (WebSocket) are supported.

use std::convert::Infallible;
use std::future::Future;
use std::sync::Arc;

use bytes::Bytes;
use http::{HeaderValue, StatusCode, header};
use http_body_util::BodyExt;
use http_body_util::combinators::BoxBody;
use hyper::body::{Frame, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;

/// Response bodies: whole buffers or streams of chunks, boxed to one type.
pub type Body = BoxBody<Bytes, std::io::Error>;
pub type Request = http::Request<Incoming>;
pub type Response = http::Response<Body>;

/// What serves requests. Implemented by the API router.
pub trait Handler: Send + Sync + 'static {
    fn handle(self: Arc<Self>, req: Request) -> impl Future<Output = Response> + Send;
}

/// Serve one accepted connection until the client goes away. Errors are logged at debug level:
/// clients disappearing mid-request are normal.
pub async fn serve_connection<I, H>(io: I, handler: Arc<H>)
where
    I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
    H: Handler,
{
    let service = service_fn(move |req: Request| {
        let h = handler.clone();
        async move { Ok::<_, Infallible>(h.handle(req).await) }
    });
    let conn = http1::Builder::new()
        // Slow or idle clients cannot pin a connection: headers must arrive within the timeout
        // (hyper's timer runs on the shard's timing wheel).
        .timer(super::io::RtTimer)
        .header_read_timeout(std::time::Duration::from_secs(30))
        .keep_alive(true)
        .max_buf_size(1 << 20)
        .serve_connection(io, service)
        .with_upgrades();
    if let Err(e) = conn.await {
        tracing::debug!(error = %e, "http connection ended with an error");
    }
}

// ---------------------------------------------------------------- bodies & responses

pub fn full(bytes: impl Into<Bytes>) -> Body {
    http_body_util::Full::new(bytes.into()).map_err(|never| match never {}).boxed()
}

pub fn empty() -> Body {
    http_body_util::Empty::<Bytes>::new().map_err(|never| match never {}).boxed()
}

/// A body fed by a channel of chunks; the response ends when the sender is dropped.
pub fn channel_body(rx: async_channel::Receiver<Bytes>) -> Body {
    use futures::StreamExt as _;
    let frames = rx.map(|chunk| Ok::<_, std::io::Error>(Frame::data(chunk)));
    BodyExt::boxed(http_body_util::StreamBody::new(frames))
}

pub fn response(status: StatusCode, content_type: &str, body: Body) -> Response {
    let mut r = http::Response::new(body);
    *r.status_mut() = status;
    if let Ok(v) = HeaderValue::from_str(content_type) {
        r.headers_mut().insert(header::CONTENT_TYPE, v);
    }
    r
}

pub fn json<T: serde::Serialize>(status: StatusCode, value: &T) -> Response {
    match serde_json::to_vec(value) {
        Ok(b) => response(status, "application/json", full(b)),
        Err(e) => text(StatusCode::INTERNAL_SERVER_ERROR, &format!("serialize: {e}")),
    }
}

pub fn text(status: StatusCode, s: &str) -> Response {
    response(status, "text/plain; charset=utf-8", full(s.to_owned()))
}

pub fn status(status: StatusCode) -> Response {
    let mut r = http::Response::new(empty());
    *r.status_mut() = status;
    r
}

pub fn with_header(mut r: Response, name: header::HeaderName, value: &str) -> Response {
    if let Ok(v) = HeaderValue::from_str(value) {
        r.headers_mut().append(name, v);
    }
    r
}

// ---------------------------------------------------------------- request helpers

/// Collect a request body, refusing more than `limit` bytes.
pub async fn body_bytes(body: Incoming, limit: usize) -> anyhow::Result<Bytes> {
    let collected = http_body_util::Limited::new(body, limit)
        .collect()
        .await
        .map_err(|e| anyhow::anyhow!("request body: {e}"))?;
    Ok(collected.to_bytes())
}

pub async fn body_json<T: serde::de::DeserializeOwned>(body: Incoming, limit: usize) -> anyhow::Result<T> {
    let b = body_bytes(body, limit).await?;
    if b.is_empty() {
        return serde_json::from_slice(b"{}").map_err(|e| anyhow::anyhow!("json: {e}"));
    }
    serde_json::from_slice(&b).map_err(|e| anyhow::anyhow!("json: {e}"))
}

/// Next data chunk of a streaming request body (`None` at the end).
pub async fn next_chunk(body: &mut Incoming) -> anyhow::Result<Option<Bytes>> {
    loop {
        match body.frame().await {
            None => return Ok(None),
            Some(Err(e)) => anyhow::bail!("request body: {e}"),
            Some(Ok(frame)) => {
                if let Ok(data) = frame.into_data() {
                    return Ok(Some(data));
                }
            }
        }
    }
}

/// Query string → key/value pairs (percent-decoded).
pub fn query(req: &Request) -> std::collections::HashMap<String, String> {
    req.uri()
        .query()
        .unwrap_or("")
        .split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (decode(k), decode(v))
        })
        .collect()
}

/// Percent-decode (and `+` → space, as browsers encode query strings).
pub fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        match b {
            b'+' => out.push(b' '),
            b'%' => {
                let hex = bytes.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok());
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(v) => {
                        out.push(v);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            other => out.push(other),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn header_str<'a>(req: &'a Request, name: &header::HeaderName) -> Option<&'a str> {
    req.headers().get(name).and_then(|v| v.to_str().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_queries() {
        assert_eq!(decode("a%2Fb+c%zz"), "a/b c%zz");
        assert_eq!(decode("agent%3Abench-1"), "agent:bench-1");
    }
}
