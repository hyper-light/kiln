//! WebSocket upgrades on top of the hyper server: the RFC 6455 handshake is answered here, and
//! the upgraded connection is framed by `async-tungstenite` (runtime-free; driven by hyper-rt).

use std::pin::Pin;
use std::task::{Context, Poll};

use http::{StatusCode, header};
use hyper::upgrade::Upgraded;

use super::server::{Request, Response, empty};

pub use tungstenite::Message;

/// A server-side WebSocket over an upgraded hyper connection.
pub type WebSocket = async_tungstenite::WebSocketStream<FuturesIo<Upgraded>>;

/// Adapts hyper's IO traits to `futures-io`, which async-tungstenite speaks. Safe code only: the
/// caller's buffer is already initialised, so hyper's `ReadBuf` can wrap it directly.
pub struct FuturesIo<T>(pub T);

impl<T: hyper::rt::Read + Unpin> futures::io::AsyncRead for FuturesIo<T> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<std::io::Result<usize>> {
        let mut rb = hyper::rt::ReadBuf::new(buf);
        match Pin::new(&mut self.0).poll_read(cx, rb.unfilled()) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(rb.filled().len())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<T: hyper::rt::Write + Unpin> futures::io::AsyncWrite for FuturesIo<T> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

/// Is this a WebSocket upgrade request?
pub fn is_upgrade(req: &Request) -> bool {
    let has = |name: header::HeaderName, token: &str| {
        req.headers()
            .get_all(name)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .any(|t| t.trim().eq_ignore_ascii_case(token))
    };
    has(header::CONNECTION, "upgrade") && has(header::UPGRADE, "websocket")
}

/// Answer the handshake. Returns the `101 Switching Protocols` response to send, and a future
/// that yields the WebSocket once hyper has handed the connection over (spawn it).
/// Why a WebSocket handshake was refused.
#[derive(Debug, Clone, Copy)]
pub struct Refused {
    pub status: StatusCode,
    pub reason: &'static str,
}

pub fn accept(
    req: Request,
) -> Result<(Response, impl Future<Output = anyhow::Result<WebSocket>> + Send + 'static), Refused> {
    let bad = |reason: &'static str| Refused { status: StatusCode::BAD_REQUEST, reason };
    if !is_upgrade(&req) {
        return Err(bad("expected a WebSocket upgrade"));
    }
    if req.headers().get(header::SEC_WEBSOCKET_VERSION).and_then(|v| v.to_str().ok()) != Some("13") {
        return Err(bad("unsupported WebSocket version"));
    }
    let Some(key) = req.headers().get(header::SEC_WEBSOCKET_KEY) else {
        return Err(bad("missing Sec-WebSocket-Key"));
    };
    let accept = tungstenite::handshake::derive_accept_key(key.as_bytes());
    let mut resp = http::Response::new(empty());
    *resp.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    let h = resp.headers_mut();
    h.insert(header::UPGRADE, http::HeaderValue::from_static("websocket"));
    h.insert(header::CONNECTION, http::HeaderValue::from_static("Upgrade"));
    if let Ok(v) = http::HeaderValue::from_str(&accept) {
        h.insert(header::SEC_WEBSOCKET_ACCEPT, v);
    }
    let upgrade = async move {
        let upgraded = hyper::upgrade::on(req).await.map_err(|e| anyhow::anyhow!("upgrade: {e}"))?;
        let ws = async_tungstenite::WebSocketStream::from_raw_socket(
            FuturesIo(upgraded),
            tungstenite::protocol::Role::Server,
            Some(tungstenite::protocol::WebSocketConfig::default().max_message_size(Some(16 << 20))),
        )
        .await;
        Ok(ws)
    };
    Ok((resp, upgrade))
}

/// Text frame helper.
pub fn text(s: impl Into<String>) -> Message {
    Message::text(s.into())
}

/// Binary frame helper.
pub fn binary(b: impl Into<bytes::Bytes>) -> Message {
    Message::binary(b.into())
}
