//! Header maps kept per thread (goal 9 of kiln's API runtime program, plan 01a1a87b2cec step 5: a tenth of the CPU).
//! A response's header map, once its head is written, is given back emptied, its allocation kept; the next request
//! read on that thread, by any of its connections, takes it for its headers. A connection kept per connection what
//! its last response gave back, and an HTTP/1.1 connection let it go whenever it went idle between requests, as the
//! readers of a page do between theirs: each of their requests made a map of two allocations (its slots and its
//! entries), and each answer one more to keep the spare.

use std::cell::Cell;
use std::sync::LazyLock;

use http::{HeaderMap, HeaderValue};

use crate::h1::head::MAX_HEADERS;

/// Header maps kept per thread: as many as its write buffers ([`crate::buf`]'s pool), the most of either that its
/// answers in flight hand back at once.
const KEPT: usize = crate::buf::POOL;

thread_local! {
    /// Header maps given back on this thread, emptied.
    static MAPS: Cell<Vec<HeaderMap>> = const { Cell::new(Vec::new()) };
}

/// The most room a map given back may have to be kept: what a head of the most header lines a request may have
/// ([`MAX_HEADERS`]) takes. One that grew past it isn't kept to hold that much for the thread.
fn most() -> usize {
    static MOST: LazyLock<usize> = LazyLock::new(|| HeaderMap::<HeaderValue>::with_capacity(MAX_HEADERS).capacity());
    *MOST
}

/// A header map for a request's `want` headers: one given back on this thread (emptied, its allocation kept), else a
/// new one.
pub(crate) fn take(want: usize) -> HeaderMap {
    let kept = MAPS
        .try_with(|m| {
            let mut maps = m.take();
            let map = maps.pop();
            m.set(maps);
            map
        })
        .ok()
        .flatten();
    match kept {
        Some(mut map) => {
            map.reserve(want);
            map
        }
        None => HeaderMap::with_capacity(want),
    }
}

/// Gives back a header map whose headers went out (a response's, its head written), emptied, for a later request on
/// this thread: unless it never allocated, grew past [`most`], or the thread keeps [`KEPT`] already.
pub(crate) fn give(mut map: HeaderMap) {
    let room = map.capacity();
    if room == 0 || room > most() {
        return;
    }
    map.clear();
    let _ = MAPS.try_with(move |m| {
        let mut maps = m.take();
        if maps.len() < KEPT {
            maps.push(map);
        }
        m.set(maps);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Goal 9, plan 01a1a87b2cec step 5: a map given back is the next one taken on its thread, emptied, its room
    /// kept; one that never allocated or grew past a head's most isn't kept, and a thread keeps at most [`KEPT`].
    #[test]
    fn g9_s5_a_map_given_back_is_the_next_taken_emptied() {
        let mut answer = HeaderMap::new();
        answer.insert(http::header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
        let room = answer.capacity();
        assert!(room > 0);
        give(answer);
        let taken = take(3);
        assert!(taken.is_empty(), "emptied");
        assert_eq!(taken.capacity(), room, "its room kept: nothing allocated for 3 headers");
        assert_eq!(take(3).capacity(), HeaderMap::<HeaderValue>::with_capacity(3).capacity(), "none kept: a new one");
        // Never allocated, or grown past a head's most: not kept.
        give(HeaderMap::new());
        let mut big = HeaderMap::with_capacity(MAX_HEADERS * 2);
        big.insert(http::header::HOST, HeaderValue::from_static("x"));
        give(big);
        assert_eq!(take(0).capacity(), 0, "neither was kept");
        // At most KEPT a thread.
        for _ in 0..KEPT + 3 {
            let mut m = HeaderMap::new();
            m.insert(http::header::HOST, HeaderValue::from_static("x"));
            give(m);
        }
        let kept = (0..KEPT + 3).map(|_| take(0)).filter(|m| m.capacity() > 0).count();
        assert_eq!(kept, KEPT);
    }
}
