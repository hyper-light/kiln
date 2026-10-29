//! Strings whose room is kept (`docs/observe.md` §8.2, §12.17): a record of strings of its own a
//! pipeline changes (one an exporter sent) takes the strings it sets from here, and hands back what
//! they replace, so its processors cost no allocation once a few have run. (A line's record is of
//! one buffer, `kiln_obs::logs::buf::RecordBuf`, and takes none.) Each thread keeps its own few,
//! bounded: a thread that hands none back takes new ones.

use std::cell::RefCell;

/// Strings a thread keeps at the most, and the bytes of room they may hold together.
const STRINGS: usize = 1024;
const ROOM: usize = 1 << 20;
/// The most room one string kept may have: a longer one is let go.
const LONGEST: usize = 4 << 10;

struct Spare {
    strings: Vec<String>,
    room: usize,
}

thread_local! {
    static SPARE: RefCell<Spare> = const { RefCell::new(Spare { strings: Vec::new(), room: 0 }) };
}

#[inline]
fn with<T>(f: impl FnOnce(&mut Spare) -> T) -> Option<T> {
    SPARE.try_with(|s| s.try_borrow_mut().ok().map(|mut s| f(&mut s))).ok().flatten()
}

/// An empty string, with room when one was handed back.
#[inline]
pub(crate) fn take() -> String {
    with(|s| {
        let x = s.strings.pop()?;
        s.room -= x.capacity();
        Some(x)
    })
    .flatten()
    .unwrap_or_default()
}

/// `text` copied into a string [`take`]n.
#[inline]
pub(crate) fn copy(text: &str) -> String {
    let mut s = take();
    s.push_str(text);
    s
}

/// `s` handed back: its room kept for another (one too long, or past what is kept, let go).
#[inline]
pub(crate) fn give(mut s: String) {
    let room = s.capacity();
    if room == 0 || room > LONGEST {
        return;
    }
    s.clear();
    with(|p| {
        if p.strings.len() < STRINGS && p.room + room <= ROOM {
            p.room += room;
            p.strings.push(s);
        }
    });
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn room_handed_back_is_taken_again_within_its_bounds() {
        let s = copy("a key of some length");
        let room = s.capacity();
        give(s);
        let again = take();
        assert!(again.is_empty() && again.capacity() == room, "its room, empty");
        // One too long is let go; the rest are kept to their bound.
        give("x".repeat(LONGEST + 1));
        assert_eq!(take().capacity(), 0);
        for _ in 0..STRINGS + 10 {
            give(String::with_capacity(64));
        }
        let kept = with(|p| (p.strings.len(), p.room)).unwrap();
        assert!(kept.0 <= STRINGS && kept.1 <= ROOM, "{kept:?}");
    }
}
