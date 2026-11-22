//! Closed times (docs/design/replication.md, "Follower reads at a closed time"; CP-T1.10,
//! AC-T1.10.C1 to C3): every entry an owner proposes carries its proposer's clock (a stamp), and
//! every member applies it at a time of the group's own, the larger of its stamp and one past the
//! time before it. Times rise strictly along the committed log whoever leads and whatever their
//! clocks say, so a replica's applied state at time T is the group's state at T: every entry at T
//! or before, none after. That time is the replica's closed time, and a follower read is answered
//! at it. A pure core: the encoding, the rule, the record, and the lag measured.

use crate::db::Db;
use amphora_api::{self as api, Refusal};

/// A stamped entry's prefix: a log entry kind of its own, as the class entry's and the seed's. No
/// valid unprefixed resolved transaction starts with it: read as postcard it would hold 112 pages
/// whose first is 111 bytes long, and every page is 4096 (`replication::validate_pages`).
const PREFIX: &[u8] = b"amphora.time.v1\0";
/// What a stamp adds to an entry: its prefix and the proposer's time, a `u64` of microseconds
/// since the Unix epoch in eight little-endian bytes (a format width).
pub(crate) const STAMP_BYTES: usize = PREFIX.len().saturating_add(8);
const TABLE: &str = "CREATE TABLE IF NOT EXISTS __amphora_closed (id INTEGER PRIMARY KEY CHECK(id = 1), micros INTEGER NOT NULL)";

/// `data` stamped with `micros`, its proposer's clock.
pub(crate) fn stamp(micros: u64, data: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(STAMP_BYTES.saturating_add(data.len()));
    bytes.extend_from_slice(PREFIX);
    bytes.extend_from_slice(&micros.to_le_bytes());
    bytes.extend_from_slice(data);
    bytes
}

/// An entry's stamp, if it carries one, and the entry inside it. An entry from before stamps, or
/// one hyper-raft made itself (an election's empty entry), carries none.
pub(crate) fn open(data: &[u8]) -> Result<(Option<u64>, &[u8]), Refusal> {
    let Some(rest) = data.strip_prefix(PREFIX) else {
        return Ok((None, data));
    };
    let (micros, inner) = rest
        .split_first_chunk::<8>()
        .ok_or_else(|| api::invalid("stamped entry", "the stamp is cut short"))?;
    Ok((Some(u64::from_le_bytes(*micros)), inner))
}

/// What applying an entry does to its replica's closed time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Next {
    /// The closed time once the entry is applied: none until the first stamped entry.
    pub(crate) time: Option<u64>,
    /// The stamp was at or below the time before it, so the entry's time was lifted past its
    /// proposer's clock: a leader whose clock is behind its predecessor's.
    pub(crate) lifted: bool,
}

/// The closed time after an entry stamped `stamp` (none if unstamped) that `changes` the
/// database's state, the time before it being `before`. A stamped entry takes the larger of its
/// stamp and one past `before`; an unstamped one that changes state takes one past `before`
/// once the group has a time; one that changes nothing (an empty entry, a change of members) keeps
/// it. One microsecond is the time's least step: the stamp's unit.
pub(crate) fn next(
    before: Option<u64>,
    stamp: Option<u64>,
    changes: bool,
) -> Result<Next, Refusal> {
    let past = before
        .map(|time| {
            time.checked_add(1)
                .ok_or_else(|| Refusal::Fenced("the group's closed time overflowed".into()))
        })
        .transpose()?;
    Ok(match (stamp, past) {
        (Some(stamp), Some(past)) => Next {
            time: Some(stamp.max(past)),
            lifted: stamp < past,
        },
        (Some(stamp), None) => Next {
            time: Some(stamp),
            lifted: false,
        },
        (None, Some(past)) if changes => Next {
            time: Some(past),
            lifted: false,
        },
        (None, _) => Next {
            time: before,
            lifted: false,
        },
    })
}

/// Records `time` as the database's closed time in the transaction under way, where it moved from
/// `before`. The table is made where there was no time before (`before` none): by the first stamped
/// entry, at the same index on every member, and again after a seed's image, which has none.
pub(crate) fn record(db: &mut Db, before: Option<u64>, time: Option<u64>) -> anyhow::Result<()> {
    let Some(time) = time else {
        return Ok(());
    };
    if before == Some(time) {
        return Ok(());
    }
    if before.is_none() {
        db.exec(TABLE)?;
    }
    db.query(
        "INSERT INTO __amphora_closed(id, micros) VALUES (1, ?1) ON CONFLICT(id) DO UPDATE SET micros = excluded.micros",
        [amphora_engine::Value::from_i64(i64::try_from(time)?)],
        |_| Ok(true),
    )
}

/// The closed time this database records, none before its group's first stamped entry. Inside a
/// read transaction, as its first statement, it is the time of the state the transaction reads.
pub(crate) fn recorded(db: &Db) -> anyhow::Result<Option<u64>> {
    let mut table = false;
    db.query(
        "SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = '__amphora_closed'",
        [],
        |_| {
            table = true;
            Ok(false)
        },
    )?;
    if !table {
        return Ok(None);
    }
    let mut time = None;
    db.query(
        "SELECT micros FROM __amphora_closed WHERE id = 1",
        [],
        |row| {
            time = Some(u64::try_from(
                row.get::<i64>(0).map_err(|e| anyhow::anyhow!("{e}"))?,
            )?);
            Ok(false)
        },
    )?;
    Ok(time)
}

/// Samples the lag keeps: the fewest of which a 99th percentile is one sample short of the
/// largest, 1 / (1 - 0.99). The quantile is AC-T1.10.3's, which states follower reads' latency at
/// p99: a read asked at the lag behind now waits for its time no more often than that. Not a
/// tuning value: the quantile fixes it (fewer samples and the "p99" is their maximum; more only
/// remember older delays longer, a picked trade-off). What is learned is the lag itself, by every
/// replica from its own delays, continuously.
pub(crate) const LAG_SAMPLES: usize = 100;

/// A replica's replication delay as it measures it: how far its closed time stood behind its own
/// clock each time it moved, over the last [`LAG_SAMPLES`] moves (CockroachDB's
/// `follower_read_timestamp` is the same idea with a configured lag; here it is measured).
#[derive(Debug, Default)]
pub(crate) struct Lag {
    samples: std::collections::VecDeque<u64>,
}

impl Lag {
    /// The closed time moved to `time` when this replica's clock read `now`: one sample, the
    /// oldest dropped past [`LAG_SAMPLES`]. A clock behind the time counts as no lag.
    pub(crate) fn moved(&mut self, now: u64, time: u64) {
        if self.samples.len() >= LAG_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back(now.saturating_sub(time));
    }

    /// The 99th percentile of the samples (the nearest-rank one), none before the first.
    pub(crate) fn p99(&self) -> Option<u64> {
        let mut sorted: Vec<u64> = self.samples.iter().copied().collect();
        sorted.sort_unstable();
        // Nearest rank: ceil(0.99 n), one-based.
        let rank = sorted.len().checked_mul(99)?.div_ceil(100);
        sorted.get(rank.checked_sub(1)?).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AC-T1.10.C1: a stamp round-trips, and an entry without one is itself.
    #[test]
    fn a_stamped_entry_opens_to_its_stamp_and_its_entry() {
        let stamped = stamp(1_700_000_000_000_001, b"inner");
        assert_eq!(stamped.len(), STAMP_BYTES + 5);
        assert_eq!(
            open(&stamped).unwrap(),
            (Some(1_700_000_000_000_001), b"inner".as_slice())
        );
        assert_eq!(open(b"plain").unwrap(), (None, b"plain".as_slice()));
        assert_eq!(open(b"").unwrap(), (None, b"".as_slice()));
        assert_eq!(
            open(&stamp(7, b"")).unwrap(),
            (Some(7), b"".as_slice()),
            "an empty entry stamped"
        );
        let cut = &stamped[..PREFIX.len() + 3];
        assert!(matches!(open(cut), Err(Refusal::Invalid { .. })));
    }

    /// AC-T1.10.C1: times rise strictly over the entries that change state, whatever the stamps:
    /// a stamp at or behind the time before it is lifted one past it, and counted.
    #[test]
    fn times_rise_strictly_whatever_the_stamps_say() {
        let first = next(None, Some(100), true).unwrap();
        assert_eq!(
            first,
            Next {
                time: Some(100),
                lifted: false
            }
        );
        let ahead = next(Some(100), Some(250), true).unwrap();
        assert_eq!(
            ahead,
            Next {
                time: Some(250),
                lifted: false
            }
        );
        let behind = next(Some(250), Some(40), true).unwrap();
        assert_eq!(
            behind,
            Next {
                time: Some(251),
                lifted: true
            }
        );
        let equal = next(Some(251), Some(251), true).unwrap();
        assert_eq!(
            equal,
            Next {
                time: Some(252),
                lifted: true
            }
        );
        // An unstamped entry that changes state, once there is a time.
        assert_eq!(next(Some(252), None, true).unwrap().time, Some(253));
        // One that changes nothing, and any before the first stamp.
        assert_eq!(next(Some(253), None, false).unwrap().time, Some(253));
        assert_eq!(next(None, None, true).unwrap().time, None);
        assert!(matches!(
            next(Some(u64::MAX), Some(1), true),
            Err(Refusal::Fenced(_))
        ));
    }

    /// AC-T1.10.C3: the lag is the nearest-rank p99 of the last LAG_SAMPLES delays, and forgets
    /// older ones.
    #[test]
    fn the_lag_is_the_p99_of_the_last_hundred_delays() {
        let mut lag = Lag::default();
        assert_eq!(lag.p99(), None);
        lag.moved(1_000, 990);
        assert_eq!(lag.p99(), Some(10));
        lag.moved(1_000, 2_000);
        assert_eq!(lag.p99(), Some(10), "a clock behind the time is no lag");
        let mut lag = Lag::default();
        for delay in 1..=100u64 {
            lag.moved(10_000, 10_000 - delay);
        }
        assert_eq!(lag.p99(), Some(99), "the 99th of 1..=100");
        for _ in 0..LAG_SAMPLES {
            lag.moved(10_000, 9_995);
        }
        assert_eq!(lag.p99(), Some(5), "the older delays are forgotten");
    }
}
