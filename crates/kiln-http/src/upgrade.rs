//! Upgrades: a connection handed over to another protocol (a WebSocket, a CONNECT tunnel) once the
//! response says it switches. On the server, a request asking to upgrade carries an [`OnUpgrade`];
//! answering it `101 Switching Protocols` hands the connection to whoever awaits that. On the
//! client, a response of `101` to a request that asked carries one the same way.

use std::io::{self, IoSlice};
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};

use bytes::{Buf as _, Bytes};
use futures_channel::oneshot;
use futures_io::{AsyncRead, AsyncWrite};

use crate::Error;
use crate::error::Kind;

/// A transport a connection runs over, boxed once it's upgraded.
pub(crate) trait Transport: AsyncRead + AsyncWrite + Send + Unpin + 'static {}

impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> Transport for T {}

/// A connection handed over: its transport, and what was read on it past the switch already.
pub struct Upgraded {
    read: Bytes,
    io: Box<dyn Transport>,
}

impl Upgraded {
    pub(crate) fn new<T: Transport>(io: T, read: Bytes) -> Upgraded {
        Upgraded { read, io: Box::new(io) }
    }
}

impl std::fmt::Debug for Upgraded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Upgraded").field("read", &self.read.len()).finish_non_exhaustive()
    }
}

impl AsyncRead for Upgraded {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if !this.read.is_empty() {
            let n = this.read.len().min(buf.len());
            if let (Some(to), Some(from)) = (buf.get_mut(..n), this.read.get(..n)) {
                to.copy_from_slice(from);
            }
            this.read.advance(n);
            return Poll::Ready(Ok(n));
        }
        Pin::new(&mut this.io).poll_read(cx, buf)
    }
}

impl AsyncWrite for Upgraded {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().io).poll_write(cx, buf)
    }

    fn poll_write_vectored(self: Pin<&mut Self>, cx: &mut Context<'_>, bufs: &[IoSlice<'_>]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().io).poll_write_vectored(cx, bufs)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_close(cx)
    }
}

type Handoff = oneshot::Receiver<Result<Upgraded, Error>>;

/// The connection a message's upgrade hands over, once it does. In the message's extensions (where
/// [`on`] finds it), so it clones; the first clone awaited takes the connection.
#[derive(Clone)]
pub struct OnUpgrade {
    rx: Option<Arc<Mutex<Handoff>>>,
}

impl std::fmt::Debug for OnUpgrade {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OnUpgrade")
    }
}

impl std::future::Future for OnUpgrade {
    type Output = Result<Upgraded, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let Some(rx) = &self.rx else {
            return Poll::Ready(Err(Error::with(Kind::Upgrade, "the message didn't ask to upgrade")));
        };
        let mut rx = rx.lock().unwrap_or_else(PoisonError::into_inner);
        match Pin::new(&mut *rx).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(outcome)) => Poll::Ready(outcome),
            Poll::Ready(Err(oneshot::Canceled)) => Poll::Ready(Err(Error::with(Kind::Upgrade, "the connection ended without switching"))),
        }
    }
}

/// The connection's side of an upgrade: it hands its transport over, or drops this to refuse.
pub(crate) struct Pending {
    tx: oneshot::Sender<Result<Upgraded, Error>>,
}

impl Pending {
    pub(crate) fn fulfill(self, upgraded: Upgraded) {
        let _ = self.tx.send(Ok(upgraded));
    }
}

/// A pending upgrade and the message's half of it.
pub(crate) fn pair() -> (Pending, OnUpgrade) {
    let (tx, rx) = oneshot::channel();
    (Pending { tx }, OnUpgrade { rx: Some(Arc::new(Mutex::new(rx))) })
}

/// Takes the [`OnUpgrade`] out of a request or a response (one that didn't ask has none: awaiting
/// it fails).
pub fn on<M: sealed::Message>(mut msg: M) -> OnUpgrade {
    msg.extensions_mut().remove::<OnUpgrade>().unwrap_or(OnUpgrade { rx: None })
}

mod sealed {
    pub trait Message {
        fn extensions_mut(&mut self) -> &mut http::Extensions;
    }

    impl<B> Message for http::Request<B> {
        fn extensions_mut(&mut self) -> &mut http::Extensions {
            http::Request::extensions_mut(self)
        }
    }

    impl<B> Message for &mut http::Request<B> {
        fn extensions_mut(&mut self) -> &mut http::Extensions {
            http::Request::extensions_mut(self)
        }
    }

    impl<B> Message for http::Response<B> {
        fn extensions_mut(&mut self) -> &mut http::Extensions {
            http::Response::extensions_mut(self)
        }
    }

    impl<B> Message for &mut http::Response<B> {
        fn extensions_mut(&mut self) -> &mut http::Extensions {
            http::Response::extensions_mut(self)
        }
    }
}
