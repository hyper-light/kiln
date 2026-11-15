//! Joint plan step 14 (2c; ROOT, 2026-11-13): an append-only native-helper request for the stop response the
//! helper's managed CPU control holds its scopes to, learned from its own holds, each from a scope's credit running
//! out to its members read stopped. It names no scope: the response is the helper's, every scope's control taking it.
//! The helper answers its estimate (the review's A-14; ROOT, 2026-11-14), none before its first hold. The node's cycle
//! of its control at a rate (`MacRuntime::cap_cycle`) takes it, asked each time (2c-4b): none is that cycle's cold
//! start, told, and a cold estimate a cycle used and told cold.
use super::*;

const KIND: u16 = 0x0015;
const ANSWER: u16 = 0x8013;
/// The header alone.
pub const MAX_MESSAGE: usize = HEADER_BYTES;
/// The header, whether a response is estimated, its nanoseconds, and whether it is cold.
pub const MAX_ANSWER: usize = HEADER_BYTES + 1 + size_of::<u64>() + 1;

/// The stop response a helper's managed CPU control holds its scopes to (the review's A-14; ROOT, 2026-11-14).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Estimate {
    /// The nearest-rank p99.9 of its last holds' responses, or their slowest while they are too few to have seen it.
    pub response: Duration,
    /// Whether it is their slowest, too few holds to have seen their p99.9; or an older helper's slowest seen, whose
    /// answer ends at its response, as it predates this field.
    pub cold: bool,
}

/// The request asking the helper's estimate.
pub fn encode(version: u16, id: u64) -> Result<Vec<u8>, Fault> {
    encode_bounded(MAX_MESSAGE, |out| header(out, version, KIND, id))
}

/// The response request in `bytes`, which carries no descriptor: its version and id.
pub fn read(bytes: &[u8], fds: usize) -> Result<(u16, u64), Fault> {
    if bytes.len() > MAX_MESSAGE {
        return Err(bounded(bytes.len()));
    }
    let (input, version, kind, id) = super::super::header(bytes)?;
    if kind != KIND {
        return Err(Fault::Kind {
            kind,
            version,
            expected: "a native CPU response request",
        });
    }
    descriptors(fds, 0)?;
    input.end(kind, version)?;
    Ok((version, id))
}

/// The answer naming `estimate`, none before the helper's first hold, whether it is cold appended after its response
/// (A-14); a response of no time is refused, as a control's check refuses it.
pub fn encode_answer(version: u16, id: u64, estimate: Option<Estimate>) -> Result<Vec<u8>, Fault> {
    let estimate = match estimate {
        Some(estimate) if estimate.response.is_zero() => {
            return Err(invalid("native CPU response"));
        }
        Some(estimate) => Some((
            u64::try_from(estimate.response.as_nanos())
                .map_err(|_| invalid("native CPU response"))?,
            estimate.cold,
        )),
        None => None,
    };
    encode_bounded(MAX_ANSWER, |out| {
        header(out, version, ANSWER, id)?;
        match estimate {
            Some((nanos, cold)) => {
                u8(out, 1)?;
                u64(out, nanos)?;
                u8(out, u8::from(cold))
            }
            None => u8(out, 0),
        }
    })
}

/// The response answer in `bytes`, which carries no descriptor: its version, id and the helper's estimate. One that
/// ends at its response is an older helper's, its slowest seen, so cold.
pub fn read_answer(bytes: &[u8], fds: usize) -> Result<(u16, u64, Option<Estimate>), Fault> {
    if bytes.len() > MAX_ANSWER {
        return Err(bounded(bytes.len()));
    }
    let (mut input, version, kind, id) = super::super::header(bytes)?;
    if kind != ANSWER {
        return Err(Fault::Kind {
            kind,
            version,
            expected: "a native CPU response answer",
        });
    }
    descriptors(fds, 0)?;
    let estimate = if input.bool("native CPU response seen")? {
        let nanos = input.u64()?;
        if nanos == 0 {
            return Err(invalid("native CPU response"));
        }
        let cold = input.at() == bytes.len() || input.bool("native CPU response cold")?;
        Some(Estimate {
            response: Duration::from_nanos(nanos),
            cold,
        })
    } else {
        None
    };
    input.end(kind, version)?;
    Ok((version, id, estimate))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Joint plan step 14 (2c) and the review's A-14 (ROOT, 2026-11-14; AC-T7.4.1): a helper's estimate crosses to
    /// its node whole and alone, with whether it is cold, and an older helper's slowest seen reads cold. Do encode the
    /// request and read it back, with a descriptor and cut short at every length; then answer it with a 3.5 ms
    /// estimate, cold and not, and with none (no hold seen yet), each read back, cut short, and read by another kind's
    /// reader; then read an older helper's answer, the same response with no field after it. Expect each back exactly
    /// and every other refused, and the older helper's 3.5 ms cold.
    #[test]
    fn a14_a_helpers_estimate_crosses_whole_with_whether_it_is_cold() {
        let bytes = encode(VERSION, 41).unwrap();
        assert_eq!(read(&bytes, 0).unwrap(), (VERSION, 41));
        assert!(read(&bytes, 1).is_err());
        for size in 0..bytes.len() {
            assert!(read(&bytes[..size], 0).is_err());
        }
        assert!(read_request(&bytes, 0).is_err());
        assert!(rate::read(&bytes, 0).is_err());
        let response = Duration::from_micros(3_500);
        let estimates = [
            Some(Estimate {
                response,
                cold: true,
            }),
            Some(Estimate {
                response,
                cold: false,
            }),
            None,
        ];
        for estimate in estimates {
            let answer = encode_answer(VERSION, 41, estimate).unwrap();
            assert_eq!(read_answer(&answer, 0).unwrap(), (VERSION, 41, estimate));
            assert!(read_answer(&answer, 1).is_err());
            // A cut at the response's end, the older helpers' form, is read below.
            let older = estimate.map(|_| answer.len() - 1);
            for size in (0..answer.len()).filter(|size| Some(*size) != older) {
                assert!(
                    read_answer(&answer[..size], 0).is_err(),
                    "{estimate:?} cut at {size}"
                );
            }
            assert!(super::super::read_answer(&answer, 0).is_err());
            assert!(read(&answer, 0).is_err());
        }
        let warm = encode_answer(VERSION, 41, estimates[1]).unwrap();
        assert_eq!(
            read_answer(&warm[..warm.len() - 1], 0).unwrap(),
            (VERSION, 41, estimates[0]),
            "an older helper's slowest seen reads cold"
        );
        assert!(
            encode_answer(
                VERSION,
                41,
                Some(Estimate {
                    response: Duration::ZERO,
                    cold: true,
                }),
            )
            .is_err()
        );
    }
}
