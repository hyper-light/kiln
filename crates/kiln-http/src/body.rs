//! [`Incoming`]: a body arriving over a connection, a request's on the server, a response's on the
//! client, as an `http_body::Body` of `Bytes`.
//!
//! A body with nothing in it, or all of it read already, costs nothing more than the enum. A body
//! still arriving is a pipe between the connection and whoever reads the body (the handler, or a
//! task it handed the body to): the connection decodes what it reads into the pipe while the reader
//! wants more (read ahead up to a window), and stops when the reader stops, so a slow reader holds
//! the sender back (TCP's window, or HTTP/2's) instead of filling memory.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};

use bytes::Bytes;
use http::HeaderMap;
use http_body::{Frame, SizeHint};

use crate::Error;

/// How far a pipe reads ahead of its reader.
pub(crate) const READ_AHEAD: usize = 256 << 10;

/// A body arriving over a connection.
pub struct Incoming {
    kind: Kind,
}

enum Kind {
    Empty,
    /// All of it, already read.
    Whole(Option<Bytes>),
    Piped(Arc<Pipe>),
}

impl Incoming {
    /// A body with nothing in it.
    pub fn empty() -> Incoming {
        Incoming { kind: Kind::Empty }
    }

    /// A body read whole already.
    pub(crate) fn whole(bytes: Bytes) -> Incoming {
        if bytes.is_empty() { Incoming::empty() } else { Incoming { kind: Kind::Whole(Some(bytes)) } }
    }

    /// A body still arriving, of `length` bytes when that's known; the connection feeds it through the
    /// returned [`Feed`].
    pub(crate) fn piped(length: Option<u64>) -> (Incoming, Feed) {
        let pipe = Arc::new(Pipe {
            state: Mutex::new(State {
                chunks: VecDeque::new(),
                buffered: 0,
                ended: false,
                trailers: None,
                error: None,
                reader: None,
                feeder: None,
                taker: None,
                wanted: false,
                gone: false,
                fed: 0,
                taken: 0,
                length,
            }),
        });
        (Incoming { kind: Kind::Piped(Arc::clone(&pipe)) }, Feed { pipe })
    }
}

impl Default for Incoming {
    fn default() -> Incoming {
        Incoming::empty()
    }
}

impl std::fmt::Debug for Incoming {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self.kind {
            Kind::Empty => "Incoming(empty)",
            Kind::Whole(_) => "Incoming(whole)",
            Kind::Piped(_) => "Incoming(streaming)",
        })
    }
}

impl http_body::Body for Incoming {
    type Data = Bytes;
    type Error = Error;

    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, Error>>> {
        match &mut self.get_mut().kind {
            Kind::Empty => Poll::Ready(None),
            Kind::Whole(bytes) => Poll::Ready(bytes.take().map(|b| Ok(Frame::data(b)))),
            Kind::Piped(pipe) => pipe.poll_frame(cx),
        }
    }

    fn is_end_stream(&self) -> bool {
        match &self.kind {
            Kind::Empty => true,
            Kind::Whole(bytes) => bytes.is_none(),
            Kind::Piped(pipe) => {
                let s = pipe.lock();
                s.ended && s.chunks.is_empty() && s.trailers.is_none() && s.error.is_none()
            }
        }
    }

    fn size_hint(&self) -> SizeHint {
        match &self.kind {
            Kind::Empty => SizeHint::with_exact(0),
            Kind::Whole(bytes) => SizeHint::with_exact(bytes.as_ref().map_or(0, |b| b.len() as u64)),
            Kind::Piped(pipe) => {
                let s = pipe.lock();
                match s.length {
                    Some(n) => SizeHint::with_exact(n.saturating_sub(s.taken as u64)),
                    None => SizeHint::default(),
                }
            }
        }
    }
}

impl Drop for Incoming {
    fn drop(&mut self) {
        if let Kind::Piped(pipe) = &self.kind {
            let feeder = {
                let mut s = pipe.lock();
                s.gone = true;
                s.chunks.clear();
                s.buffered = 0;
                s.feeder.take()
            };
            if let Some(w) = feeder {
                w.wake();
            }
        }
    }
}

pub(crate) struct Pipe {
    state: Mutex<State>,
}

struct State {
    chunks: VecDeque<Bytes>,
    buffered: usize,
    ended: bool,
    trailers: Option<HeaderMap>,
    error: Option<Error>,
    /// Who reads, waiting for a chunk.
    reader: Option<Waker>,
    /// The connection, waiting for the reader to want more.
    feeder: Option<Waker>,
    /// The connection, waiting for the reader to take some (HTTP/2 gives that back to the window).
    taker: Option<Waker>,
    /// The reader found nothing and waits.
    wanted: bool,
    /// The reader dropped the body.
    gone: bool,
    /// Bytes fed in all.
    fed: usize,
    /// Bytes the reader took, in all.
    taken: usize,
    /// The whole body's length, when it's known.
    length: Option<u64>,
}

impl Pipe {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn poll_frame(&self, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, Error>>> {
        let (out, feeder) = {
            let mut s = self.lock();
            if let Some(chunk) = s.chunks.pop_front() {
                s.buffered = s.buffered.saturating_sub(chunk.len());
                s.taken = s.taken.saturating_add(chunk.len());
                // Below half the window, the connection may read on; and what's taken it may give back.
                let feeder = if s.buffered < READ_AHEAD / 2 { s.feeder.take() } else { None }.or_else(|| s.taker.take());
                (Poll::Ready(Some(Ok(Frame::data(chunk)))), feeder)
            } else if let Some(e) = s.error.take() {
                (Poll::Ready(Some(Err(e))), None)
            } else if let Some(t) = s.trailers.take() {
                (Poll::Ready(Some(Ok(Frame::trailers(t)))), None)
            } else if s.ended {
                (Poll::Ready(None), None)
            } else {
                s.wanted = true;
                if !s.reader.as_ref().is_some_and(|w| w.will_wake(cx.waker())) {
                    s.reader = Some(cx.waker().clone());
                }
                (Poll::Pending, s.feeder.take())
            }
        };
        if let Some(w) = feeder {
            w.wake();
        }
        out
    }
}

/// The connection's end of a body's pipe.
pub(crate) struct Feed {
    pipe: Arc<Pipe>,
}

/// What the reader of a body wants of its connection now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Want {
    /// More: the reader waits, or the pipe has room to read ahead.
    More,
    /// Nothing now: the window is full.
    Full,
    /// Nothing ever: the reader dropped the body.
    Gone,
}

impl Feed {
    /// Feeds a chunk, waking the reader.
    pub(crate) fn push(&self, chunk: Bytes) {
        if chunk.is_empty() {
            return;
        }
        let reader = {
            let mut s = self.pipe.lock();
            if s.gone {
                return;
            }
            s.buffered = s.buffered.saturating_add(chunk.len());
            s.fed = s.fed.saturating_add(chunk.len());
            s.chunks.push_back(chunk);
            s.wanted = false;
            s.reader.take()
        };
        if let Some(w) = reader {
            w.wake();
        }
    }

    /// The body ended (with `trailers`, if it had them).
    pub(crate) fn end(&self, trailers: Option<HeaderMap>) {
        let reader = {
            let mut s = self.pipe.lock();
            s.ended = true;
            s.trailers = trailers;
            s.reader.take()
        };
        if let Some(w) = reader {
            w.wake();
        }
    }

    /// The body broke off.
    pub(crate) fn fail(&self, error: Error) {
        let reader = {
            let mut s = self.pipe.lock();
            if s.ended {
                return;
            }
            s.ended = true;
            s.error = Some(error);
            s.reader.take()
        };
        if let Some(w) = reader {
            w.wake();
        }
    }

    /// What the reader wants now, without waiting.
    pub(crate) fn want(&self) -> Want {
        let s = self.pipe.lock();
        if s.gone {
            Want::Gone
        } else if s.wanted || s.buffered < READ_AHEAD {
            Want::More
        } else {
            Want::Full
        }
    }

    /// Ready once the reader wants more (or is gone); until then the connection is woken when it does.
    pub(crate) fn poll_want(&self, cx: &mut Context<'_>) -> Poll<Want> {
        let mut s = self.pipe.lock();
        if s.gone {
            return Poll::Ready(Want::Gone);
        }
        if s.wanted || s.buffered < READ_AHEAD {
            return Poll::Ready(Want::More);
        }
        if !s.feeder.as_ref().is_some_and(|w| w.will_wake(cx.waker())) {
            s.feeder = Some(cx.waker().clone());
        }
        Poll::Pending
    }

    /// Whether the reader is waiting for the very next chunk (not merely room in the window).
    pub(crate) fn is_waiting(&self) -> bool {
        self.pipe.lock().wanted
    }

    /// Bytes the reader took since this was last asked: what HTTP/2 gives back to the sender's window.
    /// With nothing new, the connection is woken when the reader takes some.
    pub(crate) fn taken_since(&self, cx: &mut Context<'_>, last: &mut usize) -> usize {
        let mut s = self.pipe.lock();
        let n = s.taken.saturating_sub(*last);
        *last = s.taken;
        if n == 0 && !s.gone && !s.taker.as_ref().is_some_and(|w| w.will_wake(cx.waker())) {
            s.taker = Some(cx.waker().clone());
        }
        n
    }

    /// Bytes buffered for the reader and not yet taken.
    pub(crate) fn buffered(&self) -> usize {
        self.pipe.lock().buffered
    }
}

impl Drop for Feed {
    fn drop(&mut self) {
        // A connection that goes away mid-body breaks the body off.
        let reader = {
            let mut s = self.pipe.lock();
            if s.ended {
                return;
            }
            s.ended = true;
            s.error = Some(Error::closed());
            s.reader.take()
        };
        if let Some(w) = reader {
            w.wake();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body::Body as _;

    fn poll(body: &mut Incoming) -> Poll<Option<Result<Frame<Bytes>, Error>>> {
        Pin::new(body).poll_frame(&mut Context::from_waker(Waker::noop()))
    }

    #[test]
    fn a_piped_body_carries_chunks_trailers_and_its_end() -> Result<(), String> {
        let (mut body, feed) = Incoming::piped(Some(5));
        assert!(poll(&mut body).is_pending());
        assert_eq!(feed.want(), Want::More);
        assert!(feed.is_waiting());
        feed.push(Bytes::from_static(b"he"));
        feed.push(Bytes::from_static(b"llo"));
        assert_eq!(body.size_hint().exact(), Some(5));
        let Poll::Ready(Some(Ok(f))) = poll(&mut body) else { return Err("a chunk".into()) };
        assert_eq!(f.into_data().ok().as_deref(), Some(&b"he"[..]));
        assert_eq!(body.size_hint().exact(), Some(3));
        let mut trailers = HeaderMap::new();
        trailers.insert("x-done", http::HeaderValue::from_static("1"));
        feed.end(Some(trailers));
        assert!(matches!(poll(&mut body), Poll::Ready(Some(Ok(_)))));
        let Poll::Ready(Some(Ok(t))) = poll(&mut body) else { return Err("trailers".into()) };
        assert!(t.is_trailers());
        assert!(matches!(poll(&mut body), Poll::Ready(None)));
        assert!(body.is_end_stream());
        Ok(())
    }

    #[test]
    fn a_body_whose_connection_goes_breaks_off_and_one_dropped_is_gone() {
        let (mut body, feed) = Incoming::piped(None);
        drop(feed);
        assert!(matches!(poll(&mut body), Poll::Ready(Some(Err(e))) if e.is_closed()));
        let (body, feed) = Incoming::piped(None);
        drop(body);
        assert_eq!(feed.want(), Want::Gone);
    }

    #[test]
    fn a_full_window_holds_the_connection_back() {
        let (mut body, feed) = Incoming::piped(None);
        feed.push(Bytes::from(vec![0u8; READ_AHEAD]));
        assert_eq!(feed.want(), Want::Full);
        assert!(feed.poll_want(&mut Context::from_waker(Waker::noop())).is_pending());
        assert!(matches!(poll(&mut body), Poll::Ready(Some(Ok(_)))));
        assert_eq!(feed.want(), Want::More);
        let mut last = 0;
        let mut cx = Context::from_waker(Waker::noop());
        assert_eq!(feed.taken_since(&mut cx, &mut last), READ_AHEAD);
        assert_eq!(feed.taken_since(&mut cx, &mut last), 0);
    }
}
