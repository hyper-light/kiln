//! The protocol socket's send path, sized from its path (goal 3 of the browser protocol program, plan
//! `01a1a832eb2f711f9c6f51d88effe89d` step 3). What waits, waits in kiln-bp's scheduler, where priorities
//! and latest-value coalescing apply; every queue below it is held to about a chunk:
//!
//! - **Messages of at most a chunk.** The scheduler hands the carrier one message at a time of at most
//!   [`room`] bytes, a frame longer than that in fragments, so a WebSocket message (its header
//!   included) is at most a chunk, and so is every TLS record kiln writes for it: TLS can't hand a
//!   browser any byte of a record until the whole record is in (research f28), and a 16 KiB record
//!   is 2.6 s at 50 kbit/s.
//! - **One message in user space.** tungstenite's write buffer is 0 (each message written and flushed
//!   as it's given) and its bound one message; TLS's outgoing buffer is limited to a chunk; the writer
//!   gives the next message only once the last one's flush is done.
//! - **A low-water mark sized from the path.** `TCP_NOTSENT_LOWAT`: a few KiB on a slow link, more on
//!   a fast one, between [`MARK_MIN`] (a chunk) and [`MARK_MAX`] (today's 128 KiB), from the rate the
//!   socket measures (Linux `tcpi_delivery_rate`, else the window's). The kernel takes writes up to
//!   the mark and wakes the writer at half of it (research f19), so the writer refills from the
//!   scheduler then, never ahead of it.
//!
//! Pure: rates and times come in as numbers.

/// Format: the longest header of a server's WebSocket frame: 2 bytes and an 8-byte length, unmasked.
pub const WS_HEADER: usize = 10;

/// Shape: the smallest chunk, goal 2's starting fragment bound (docs/browser-protocol.md,
/// "Fragmentation"): one TCP segment's worth at the usual MSS.
pub const CHUNK_MIN: usize = 1_200;

/// Shape: the largest chunk, TLS's largest record (16 KiB).
pub const CHUNK_MAX: usize = 16 << 10;

/// Shape: how long a chunk may take to leave at the path's rate, milliseconds: what an urgent frame
/// waits behind one chunk at most, on top of what's in flight.
pub const CHUNK_MS: u64 = 25;

/// Shape: the smallest low-water mark: one chunk, so a message write isn't refused for want of room.
pub const MARK_MIN: u32 = 1_200;

/// Shape: the largest, today's mark for every other socket (`crate::http::server`, 128 KiB).
pub const MARK_MAX: u32 = 128 << 10;

/// Shape: the unsent queue the mark keeps, in time at the path's rate, milliseconds: twice the time
/// a writer takes to wake and refill (the kernel wakes it at half the mark), so the pipe stays full
/// on a fast link and an urgent frame waits behind at most this much in the kernel on a slow one.
pub const MARK_MS: u64 = 10;

/// Bytes a path of `rate` bytes a second sends in `ms` milliseconds.
fn bytes_in(rate: u64, ms: u64) -> u64 {
    rate.saturating_mul(ms) / 1_000
}

/// The chunk for a path of `rate` bytes a second (unknown: the smallest): what leaves in [`CHUNK_MS`],
/// between [`CHUNK_MIN`] and [`CHUNK_MAX`].
pub fn chunk(rate: Option<u64>) -> usize {
    let Some(rate) = rate else { return CHUNK_MIN };
    usize::try_from(bytes_in(rate, CHUNK_MS)).unwrap_or(CHUNK_MAX).clamp(CHUNK_MIN, CHUNK_MAX)
}

/// The room a transmit is given for a chunk: the chunk less a WebSocket header, so the message whole is
/// at most the chunk.
pub fn room(chunk: usize) -> usize {
    chunk.saturating_sub(WS_HEADER).max(kiln_bp::server::MIN_FRAGMENT)
}

/// Format: the room kiln-bp's `poll_transmit` keeps for a compressed batch's own frame.
pub const COMPRESS_SLACK: usize = 16;

/// The fragment bound for a chunk: its room less [`COMPRESS_SLACK`], so a fragment fits a transmit
/// whether or not it compresses (a fragment cut to the whole room would never fit a compressing one's).
pub fn fragment(chunk: usize) -> usize {
    room(chunk).saturating_sub(COMPRESS_SLACK).max(kiln_bp::server::MIN_FRAGMENT)
}

/// The low-water mark for a path of `rate` bytes a second with `chunk`: [`MARK_MS`] of the rate,
/// at least a chunk (and [`MARK_MIN`]), at most [`MARK_MAX`].
pub fn mark(rate: Option<u64>, chunk: usize) -> u32 {
    let floor = u32::try_from(chunk).unwrap_or(MARK_MAX).clamp(MARK_MIN, MARK_MAX);
    let Some(rate) = rate else { return floor };
    u32::try_from(bytes_in(rate, MARK_MS)).unwrap_or(MARK_MAX).clamp(floor, MARK_MAX)
}

/// The rate to size from: the kernel's latest delivery-rate sample when the path, not the
/// application, limited it, kept as the most of those seen; else the window's own estimate.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Rate {
    kernel: Option<u64>,
}

impl Rate {
    /// A kernel sample: `rate` bytes a second, `app_limited` when the writer had nothing to send.
    pub fn kernel(&mut self, rate: u64, app_limited: bool) {
        if rate == 0 {
            return;
        }
        // An application-limited sample says the path is at least this fast.
        let best = self.kernel.unwrap_or(0);
        self.kernel = Some(if app_limited { best.max(rate) } else { rate.max(best / 2) });
    }

    /// The rate in effect, with the window's estimate where the kernel gives none.
    pub fn get(&self, window: Option<u64>) -> Option<u64> {
        self.kernel.or(window)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// bp-g3 step 3: the chunk and the mark follow the path's rate within their floor and ceiling.
    #[test]
    fn bp_g3_s3_chunk_and_mark_follow_the_rate() {
        // 50 kbit/s: 6,250 B/s.
        assert_eq!(chunk(Some(6_250)), CHUNK_MIN);
        assert_eq!(mark(Some(6_250), CHUNK_MIN), MARK_MIN);
        // 10 Mbit/s: 1.25 MB/s, a 31 KB chunk capped at 16 KiB, a 12.5 KB mark... at least the chunk.
        assert_eq!(chunk(Some(1_250_000)), CHUNK_MAX);
        assert_eq!(mark(Some(1_250_000), CHUNK_MAX), 16 << 10);
        // 1 Gbit/s: the mark at its ceiling.
        assert_eq!(mark(Some(125_000_000), CHUNK_MAX), MARK_MAX);
        // Unknown: the smallest.
        assert_eq!((chunk(None), mark(None, chunk(None))), (CHUNK_MIN, MARK_MIN));
        assert!(room(CHUNK_MIN) + WS_HEADER <= CHUNK_MIN);
    }

    /// bp-g3 step 3: an application-limited sample only raises the rate; a path-limited one sets it,
    /// but halves it at most at a time.
    #[test]
    fn bp_g3_s3_the_kernel_rate_ignores_app_limited_lows() {
        let mut r = Rate::default();
        assert_eq!(r.get(Some(9)), Some(9));
        r.kernel(1_000_000, false);
        r.kernel(10, true);
        assert_eq!(r.get(None), Some(1_000_000));
        r.kernel(100_000, false);
        assert_eq!(r.get(None), Some(500_000));
        r.kernel(100_000, false);
        assert_eq!(r.get(None), Some(250_000));
    }
}
