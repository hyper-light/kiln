//! Bounded diagnostics in the existing VM receipt. An opaque frame grants no
//! guest identity, readiness, guest-box absence or native-scope release.

use hex_api::{Exit, Refusal};
use hex_core::wire;

use super::{AttemptPhase, exhausted, hex_array, hex_byte, invalid, lower_hex, number};

const VERSION: u32 = 1;
const EXIT: &str = "observer-exit\t";
const FAILURE: &str = "observer-failure\t";
const CLEANUP: &str = "observer-cleanup\t";

#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    version: u32,
    identity: u64,
    frame: Vec<u8>,
}

impl Snapshot {
    fn captured(frame: Vec<u8>) -> Result<Self, Refusal> {
        if frame.is_empty() || frame.len() > wire::MAX_MESSAGE {
            return Err(exhausted(wire::MAX_MESSAGE));
        }
        Ok(Self {
            version: VERSION,
            identity: wire::IDENTITY,
            frame,
        })
    }

    fn render(&self, out: &mut impl std::fmt::Write) -> std::fmt::Result {
        write!(
            out,
            "{}\t{:016x}\t{}\t",
            self.version,
            self.identity,
            self.frame.len()
        )?;
        for byte in &self.frame {
            write!(out, "{byte:02x}")?;
        }
        Ok(())
    }

    fn parse(text: &str) -> Result<Self, Refusal> {
        let mut words = text.split('\t');
        let mut next = || {
            words
                .next()
                .ok_or_else(|| invalid("observer fact is truncated"))
        };
        let version = u32::try_from(number(next()?)?)
            .map_err(|_| invalid("observer fact version exceeds u32"))?;
        let identity = u64::from_be_bytes(hex_array(next()?)?);
        let bytes = usize::try_from(number(next()?)?).map_err(|_| exhausted(wire::MAX_MESSAGE))?;
        let frame = next()?;
        let (pairs, tail) = frame.as_bytes().as_chunks::<2>();
        // The v5 envelope's fixed bound applies even when its enclosed version
        // or wire is unknown. Such bytes remain opaque and cleanup-readable.
        if version == 0
            || bytes == 0
            || bytes > wire::MAX_MESSAGE
            || pairs.len() != bytes
            || !tail.is_empty()
            || !frame.bytes().all(lower_hex)
        {
            return Err(invalid("observer fact has noncanonical or excessive bytes"));
        }
        if words.next().is_some() {
            return Err(invalid("observer fact has extra fields"));
        }
        let mut frame = Vec::new();
        frame
            .try_reserve_exact(bytes)
            .map_err(|_| exhausted(bytes))?;
        for pair in pairs {
            frame.push(hex_byte(pair)?);
        }
        Ok(Self {
            version,
            identity,
            frame,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Cleanup {
    Pending,
    Attempting,
    Deleted,
    Missing,
    Failed(Snapshot),
}

#[derive(Debug, PartialEq, Eq)]
struct Failure {
    phase: AttemptPhase,
    cause: Snapshot,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Facts {
    exit: Option<Snapshot>,
    failure: Option<Failure>,
    cleanup: Cleanup,
}

impl Facts {
    pub fn new() -> Self {
        Self {
            exit: None,
            failure: None,
            cleanup: Cleanup::Pending,
        }
    }

    pub fn exited(&mut self, exit: Exit) -> Result<(), Refusal> {
        self.exit = Some(Snapshot::captured(
            wire::snapshot_exit(exit, wire::MAX_MESSAGE)
                .map_err(|_| exhausted(wire::MAX_MESSAGE))?,
        )?);
        Ok(())
    }

    pub fn failed(&mut self, phase: AttemptPhase, why: &Refusal) -> Result<(), Refusal> {
        if self.failure.is_none() {
            self.failure = Some(Failure {
                phase,
                cause: Snapshot::captured(
                    wire::snapshot_refusal(why, wire::MAX_MESSAGE)
                        .map_err(|_| exhausted(wire::MAX_MESSAGE))?,
                )?,
            });
        }
        Ok(())
    }

    pub fn needs_cleanup(&self) -> bool {
        matches!(self.cleanup, Cleanup::Pending)
    }

    pub fn cleaned(&self) -> bool {
        matches!(self.cleanup, Cleanup::Deleted | Cleanup::Missing)
    }

    pub fn failed_cleanup_recorded(&self) -> bool {
        matches!(self.cleanup, Cleanup::Failed(_))
    }

    pub fn cleaning(&mut self) {
        self.cleanup = Cleanup::Attempting;
    }

    pub fn deleted(&mut self, missing: bool) {
        self.cleanup = if missing {
            Cleanup::Missing
        } else {
            Cleanup::Deleted
        };
    }

    pub fn cleanup_failed(&mut self, why: &Refusal) -> Result<(), Refusal> {
        self.cleanup = Cleanup::Failed(Snapshot::captured(
            wire::snapshot_refusal(why, wire::MAX_MESSAGE)
                .map_err(|_| exhausted(wire::MAX_MESSAGE))?,
        )?);
        Ok(())
    }

    pub fn ready(&self) -> Result<bool, Refusal> {
        let Some(exit) = &self.exit else {
            return Ok(false);
        };
        if self.failure.is_some()
            || !self.cleaned()
            || exit.version != VERSION
            || exit.identity != wire::IDENTITY
        {
            return Ok(false);
        }
        // Compare with the sole codec rather than decoding/interning untrusted
        // refusal names or maintaining a second set of wire tags.
        Ok(exit.frame
            == wire::snapshot_exit(Exit::Code(0), wire::MAX_MESSAGE)
                .map_err(|_| exhausted(wire::MAX_MESSAGE))?)
    }

    pub fn render(&self, out: &mut impl std::fmt::Write) -> std::fmt::Result {
        write!(out, "{EXIT}")?;
        match &self.exit {
            Some(exit) => exit.render(out)?,
            None => write!(out, "none")?,
        }
        writeln!(out)?;
        write!(out, "{FAILURE}")?;
        match &self.failure {
            Some(failure) => {
                write!(out, "{}\t", failure.phase.word())?;
                failure.cause.render(out)?;
            }
            None => write!(out, "none")?,
        }
        writeln!(out)?;
        write!(out, "{CLEANUP}")?;
        match &self.cleanup {
            Cleanup::Pending => write!(out, "pending")?,
            Cleanup::Attempting => write!(out, "attempting")?,
            Cleanup::Deleted => write!(out, "deleted")?,
            Cleanup::Missing => write!(out, "missing")?,
            Cleanup::Failed(cause) => {
                write!(out, "failed\t")?;
                cause.render(out)?;
            }
        }
        writeln!(out)
    }

    pub fn parse(lines: &mut std::str::Lines<'_>) -> Result<Self, Refusal> {
        fn row<'a>(lines: &mut std::str::Lines<'a>, prefix: &str) -> Result<&'a str, Refusal> {
            lines
                .next()
                .and_then(|row| row.strip_prefix(prefix))
                .ok_or_else(|| invalid("observer diagnostic row is missing or reordered"))
        }
        let exit = match row(lines, EXIT)? {
            "none" => None,
            frame => Some(Snapshot::parse(frame)?),
        };
        let failure = match row(lines, FAILURE)? {
            "none" => None,
            text => {
                let (phase, frame) = text
                    .split_once('\t')
                    .ok_or_else(|| invalid("observer failure lacks its original phase"))?;
                Some(Failure {
                    phase: AttemptPhase::parse(phase)?,
                    cause: Snapshot::parse(frame)?,
                })
            }
        };
        let cleanup = match row(lines, CLEANUP)? {
            "pending" => Cleanup::Pending,
            "attempting" => Cleanup::Attempting,
            "deleted" => Cleanup::Deleted,
            "missing" => Cleanup::Missing,
            text => Cleanup::Failed(Snapshot::parse(
                text.strip_prefix("failed\t")
                    .ok_or_else(|| invalid("observer cleanup state is unknown"))?,
            )?),
        };
        Ok(Self {
            exit,
            failure,
            cleanup,
        })
    }

    /// Three snapshots, each bounded by the existing wire frame bound. The
    /// longest original phase and cleanup word plus decimal/native widths bound
    /// the exact text growth, checked against the existing record cap pre-run.
    pub fn additional_room(&self) -> Result<usize, Refusal> {
        let frame = wire::MAX_MESSAGE
            .checked_mul(2)
            .and_then(|n| n.checked_add(1)) // envelope version1
            .and_then(|n| n.checked_add(size_of::<u64>().checked_mul(2)?))
            .and_then(|n| {
                n.checked_add(
                    usize::try_from(wire::MAX_MESSAGE.checked_ilog10()?.checked_add(1)?).ok()?,
                )
            })
            .and_then(|n| n.checked_add(3)); // snapshot tabs
        let maximum = frame
            .and_then(|frame| frame.checked_mul(3))
            .and_then(|n| n.checked_add(EXIT.len()))
            .and_then(|n| n.checked_add(FAILURE.len()))
            .and_then(|n| n.checked_add(CLEANUP.len()))
            .and_then(|n| n.checked_add("launching\t".len()))
            .and_then(|n| n.checked_add("failed\t".len()))
            .and_then(|n| n.checked_add(3)) // row LF
            .ok_or_else(|| exhausted(wire::MAX_MESSAGE))?;
        struct Count(usize);
        impl std::fmt::Write for Count {
            fn write_str(&mut self, text: &str) -> std::fmt::Result {
                self.0 = self.0.checked_add(text.len()).ok_or(std::fmt::Error)?;
                Ok(())
            }
        }
        let mut used = Count(0);
        self.render(&mut used).map_err(|_| exhausted(maximum))?;
        maximum
            .checked_sub(used.0)
            .ok_or_else(|| exhausted(maximum))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded(facts: &Facts) -> String {
        let mut text = String::new();
        facts.render(&mut text).unwrap();
        text
    }

    /// AC-T7.4.1: an actual exit never overwrites the original failure or the
    /// cleanup failure. A foreign snapshot remains diagnostics, not readiness.
    #[test]
    fn observer_failure_facts_keep_exit_original_stage_and_cleanup_separately() {
        let mut facts = Facts::new();
        facts.exited(Exit::Code(7)).unwrap();
        facts
            .failed(AttemptPhase::Waiting, &Refusal::Full(7))
            .unwrap();
        facts.cleaning();
        facts.cleanup_failed(&Refusal::Full(9)).unwrap();
        facts
            .failed(AttemptPhase::Deleting, &Refusal::Full(11))
            .unwrap();
        let text = encoded(&facts);
        assert_eq!(Facts::parse(&mut text.lines()).unwrap(), facts);
        assert!(text.contains("observer-failure\twaiting\t1\t"));
        assert!(!facts.ready().unwrap());
        let mut foreign = Facts::parse(&mut text.lines()).unwrap();
        let exit = foreign.exit.as_mut().unwrap();
        exit.version = 2;
        exit.identity ^= 1;
        let text = encoded(&foreign);
        let retained = Facts::parse(&mut text.lines()).unwrap();
        assert_eq!(retained, foreign);
        assert!(!retained.ready().unwrap());
        assert!(retained.failed_cleanup_recorded());
    }

    /// AC-T7.4.1: success requires the exact known zero exit and guest cleanup;
    /// malformed/oversized/duplicate diagnostic rows never synthesize it.
    #[test]
    fn observer_fact_bounds_and_unknown_wire_cannot_become_ready() {
        let mut facts = Facts::new();
        let room = facts.additional_room().unwrap();
        let initial = encoded(&facts).len();
        facts.exited(Exit::Code(0)).unwrap();
        assert!(!facts.ready().unwrap());
        facts.cleaning();
        assert!(!facts.ready().unwrap());
        facts.deleted(false);
        assert!(facts.ready().unwrap());
        let text = encoded(&facts);
        assert!(text.len() <= initial + room);
        assert_eq!(Facts::parse(&mut text.lines()).unwrap(), facts);
        for bad in [
            text.replacen("observer-exit\t1\t", "observer-exit\t0\t", 1),
            text.replacen("observer-exit\t1\t", "observer-exit\t01\t", 1),
            text.replacen("observer-exit", "observer-cleanup", 1),
            format!(
                "observer-exit\t1\t0000000000000000\t{}\t00\nobserver-failure\tnone\nobserver-cleanup\tdeleted\n",
                wire::MAX_MESSAGE + 1
            ),
            text.replacen("\tdeleted", "\tfailed\t1\t0000000000000000\t2\t00", 1),
        ] {
            assert!(Facts::parse(&mut bad.lines()).is_err(), "{bad}");
        }
        facts.exit.as_mut().unwrap().identity ^= 1;
        let text = encoded(&facts);
        let retained = Facts::parse(&mut text.lines()).unwrap();
        assert!(!retained.ready().unwrap());
        assert!(retained.cleaned()); // diagnostic only, never native absence.
    }
}
