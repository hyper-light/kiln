//! The `Date` every response carries (RFC 9110 §6.6.1), formatted once a second per thread.

use std::cell::Cell;
use std::time::SystemTime;

/// An IMF-fixdate: `Sun, 06 Nov 1994 08:49:37 GMT`.
pub(crate) const LEN: usize = 29;

thread_local! {
    /// The date, and the shard clock's reading at which its second ends.
    static CACHE: Cell<(u64, [u8; LEN])> = const { Cell::new((0, [0; LEN])) };
}

/// Now, as an IMF-fixdate (the same bytes for every response within a second). On a shard, the wall
/// clock is read once a second: the shard's own clock says when the second is over.
pub(crate) fn now() -> [u8; LEN] {
    let mono = hyper_rt::futures::now_ns();
    CACHE.with(|c| {
        let (until, date) = c.get();
        if mono != 0 && mono < until {
            return date;
        }
        let wall = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
        // Off a shard there's no clock of its own to count by: the wall clock's second marks it.
        let until = if mono == 0 { u64::MAX - wall.as_secs() } else { mono.saturating_add(1_000_000_000 - u64::from(wall.subsec_nanos())) };
        if mono == 0 && until == c.get().0 {
            return date;
        }
        let date = format(wall.as_secs());
        c.set((until, date));
        date
    })
}

fn two(out: &mut [u8; LEN], at: usize, v: u64) {
    if let Some(d) = out.get_mut(at..at + 2) {
        d.copy_from_slice(&[b'0' + (v / 10 % 10) as u8, b'0' + (v % 10) as u8]);
    }
}

/// `secs` since the epoch as an IMF-fixdate.
pub(crate) fn format(secs: u64) -> [u8; LEN] {
    const DAYS: [&[u8; 3]; 7] = [b"Thu", b"Fri", b"Sat", b"Sun", b"Mon", b"Tue", b"Wed"];
    const MONTHS: [&[u8; 3]; 12] = [b"Jan", b"Feb", b"Mar", b"Apr", b"May", b"Jun", b"Jul", b"Aug", b"Sep", b"Oct", b"Nov", b"Dec"];
    let days = secs / 86_400;
    let rem = secs % 86_400;
    // The civil date of a day count (Howard Hinnant, "chrono-Compatible Low-Level Date Algorithms").
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    let mut out = *b"Thu, 01 Jan 1970 00:00:00 GMT";
    if let (Some(d), Some(name)) = (out.get_mut(0..3), DAYS.get((days % 7) as usize)) {
        d.copy_from_slice(*name);
    }
    two(&mut out, 5, day);
    if let (Some(m), Some(name)) = (out.get_mut(8..11), MONTHS.get(month.saturating_sub(1) as usize)) {
        m.copy_from_slice(*name);
    }
    two(&mut out, 12, year / 100);
    two(&mut out, 14, year);
    two(&mut out, 17, rem / 3600);
    two(&mut out, 20, rem / 60 % 60);
    two(&mut out, 23, rem % 60);
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn dates_are_imf_fixdates() {
        assert_eq!(&super::format(0), b"Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(&super::format(784_111_777), b"Sun, 06 Nov 1994 08:49:37 GMT");
        assert_eq!(&super::format(951_782_400), b"Tue, 29 Feb 2000 00:00:00 GMT");
        assert_eq!(&super::format(1_792_627_199), b"Wed, 21 Oct 2026 23:59:59 GMT");
    }
}
