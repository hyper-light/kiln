//! What an entry of the observe group's log holds (`docs/observe.md` §5): one command, applied alike
//! by every replica.
//!
//! ```text
//! entry := (nothing: a leader's empty entry, applied as nothing)
//!        | WRITE u8 | sender u64-le | seq u64-le | at i64-le | batch          (crate::batch)
//!        | RETAIN u8 | metrics' cutoff i64-le | logs' cutoff i64-le | at i64-le
//!        | LIMITS u8 | max series u64-le
//! ```
//!
//! A write names its sender (a node's process: a random id each start) and its place in the
//! sender's order, so that a write sent twice (its answer lost, or its leader gone) is applied once:
//! each replica keeps every sender's last place, the client sessions of Ongaro's thesis ("Consensus:
//! bridging theory and practice", 2014, §6.3). `at` is the sender's clock (ms since the epoch), by
//! which a session ages. Retention is a command too, never each replica's own clock, so every
//! replica drops the same series at the same point of the log; so is the bound on series.

pub const WRITE: u8 = 1;
pub const RETAIN: u8 = 2;
pub const LIMITS: u8 = 3;

/// Bytes a write's command adds before its batch.
pub const WRITE_HEADER: usize = 1 + 8 + 8 + 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command<'a> {
    /// A leader's empty entry.
    Empty,
    Write { sender: u64, seq: u64, at: i64, batch: &'a [u8] },
    /// No sample older than `metrics` (ms) and no log record older than `logs` (ms) is kept from
    /// here on: metrics and logs are kept for their own times (a sample is a byte or two, a log
    /// line a few hundred).
    Retain { metrics: i64, logs: i64, at: i64 },
    /// At most `max_series` series from here on (0: no bound).
    Limits { max_series: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CommandError {
    #[error("an entry of an unknown command, {0}")]
    Unknown(u8),
    #[error("an entry cut short")]
    Short,
    #[error("bytes after a command")]
    Trailing,
}

/// Appends a write's command, before its batch.
pub fn put_write(out: &mut Vec<u8>, sender: u64, seq: u64, at: i64) {
    out.push(WRITE);
    out.extend_from_slice(&sender.to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&at.to_le_bytes());
}

/// A write of `batch` as an entry.
pub fn write(sender: u64, seq: u64, at: i64, batch: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(WRITE_HEADER + batch.len());
    put_write(&mut out, sender, seq, at);
    out.extend_from_slice(batch);
    out
}

/// A write's time, read and set in place (`None`: not a write): a runner's sends say its clock
/// when sent, and the leader stamps what it proposes with its own.
pub fn write_at(entry: &mut [u8], set: Option<i64>) -> Option<i64> {
    if entry.first() != Some(&WRITE) {
        return None;
    }
    let at = entry.get_mut(17..25)?;
    let was = i64::from_le_bytes(<[u8; 8]>::try_from(&*at).ok()?);
    if let Some(t) = set {
        at.copy_from_slice(&t.to_le_bytes());
    }
    Some(was)
}

pub fn retain(metrics: i64, logs: i64, at: i64) -> Vec<u8> {
    let mut out = Vec::with_capacity(25);
    out.push(RETAIN);
    out.extend_from_slice(&metrics.to_le_bytes());
    out.extend_from_slice(&logs.to_le_bytes());
    out.extend_from_slice(&at.to_le_bytes());
    out
}

pub fn limits(max_series: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(9);
    out.push(LIMITS);
    out.extend_from_slice(&max_series.to_le_bytes());
    out
}

fn word(at: &mut &[u8]) -> Result<[u8; 8], CommandError> {
    let (w, rest) = at.split_first_chunk::<8>().ok_or(CommandError::Short)?;
    *at = rest;
    Ok(*w)
}

/// The command an entry's data holds.
pub fn decode(data: &[u8]) -> Result<Command<'_>, CommandError> {
    let Some((&tag, mut at)) = data.split_first() else { return Ok(Command::Empty) };
    let c = match tag {
        WRITE => {
            let sender = u64::from_le_bytes(word(&mut at)?);
            let seq = u64::from_le_bytes(word(&mut at)?);
            let at_ms = i64::from_le_bytes(word(&mut at)?);
            return Ok(Command::Write { sender, seq, at: at_ms, batch: at });
        }
        RETAIN => Command::Retain { metrics: i64::from_le_bytes(word(&mut at)?), logs: i64::from_le_bytes(word(&mut at)?), at: i64::from_le_bytes(word(&mut at)?) },
        LIMITS => Command::Limits { max_series: u64::from_le_bytes(word(&mut at)?) },
        t => return Err(CommandError::Unknown(t)),
    };
    if at.is_empty() { Ok(c) } else { Err(CommandError::Trailing) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_round_trip_and_damage_is_an_error() {
        let w = write(7, 9, 1_700_000_000_000, b"batch bytes");
        assert_eq!(decode(&w), Ok(Command::Write { sender: 7, seq: 9, at: 1_700_000_000_000, batch: b"batch bytes" }));
        assert_eq!(decode(&retain(-5, 3, 6)), Ok(Command::Retain { metrics: -5, logs: 3, at: 6 }));
        assert_eq!(decode(&limits(100_000)), Ok(Command::Limits { max_series: 100_000 }));
        assert_eq!(decode(&[]), Ok(Command::Empty));
        assert_eq!(decode(&[9, 1, 2]), Err(CommandError::Unknown(9)));
        for cut in 1..WRITE_HEADER {
            assert_eq!(decode(&w[..cut]), Err(CommandError::Short));
        }
        let mut r = retain(1, 2, 3);
        r.push(0);
        assert_eq!(decode(&r), Err(CommandError::Trailing));
    }
}
