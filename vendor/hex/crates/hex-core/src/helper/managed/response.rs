//! Joint plan step 14 (2c; ROOT, 2026-11-13): an append-only native-helper request for the slowest stop response the
//! helper's managed CPU control has seen in its own holds, from a scope's credit running out to its members read
//! stopped. It names no scope: the response is the helper's, every scope's control taking it. The helper answers it,
//! none before its first hold, and the node derives its charge window from it (`MacRuntime::charge_window`).
use super::*;

const KIND: u16 = 0x0015;
const ANSWER: u16 = 0x8013;
/// The header alone.
pub const MAX_MESSAGE: usize = HEADER_BYTES;
/// The header, whether a response is seen, and its nanoseconds.
pub const MAX_ANSWER: usize = HEADER_BYTES + 1 + size_of::<u64>();

/// The request asking the slowest response seen.
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

/// The answer naming `slowest`, none before the helper's first hold; a response of no time is refused, as a control's
/// check refuses it.
pub fn encode_answer(version: u16, id: u64, slowest: Option<Duration>) -> Result<Vec<u8>, Fault> {
    let nanos = match slowest {
        Some(response) if response.is_zero() => return Err(invalid("native CPU response")),
        Some(response) => {
            Some(u64::try_from(response.as_nanos()).map_err(|_| invalid("native CPU response"))?)
        }
        None => None,
    };
    encode_bounded(MAX_ANSWER, |out| {
        header(out, version, ANSWER, id)?;
        match nanos {
            Some(nanos) => {
                u8(out, 1)?;
                u64(out, nanos)
            }
            None => u8(out, 0),
        }
    })
}

/// The response answer in `bytes`, which carries no descriptor: its version, id and the slowest response seen.
pub fn read_answer(bytes: &[u8], fds: usize) -> Result<(u16, u64, Option<Duration>), Fault> {
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
    let slowest = if input.bool("native CPU response seen")? {
        let nanos = input.u64()?;
        if nanos == 0 {
            return Err(invalid("native CPU response"));
        }
        Some(Duration::from_nanos(nanos))
    } else {
        None
    };
    input.end(kind, version)?;
    Ok((version, id, slowest))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Joint plan step 14 (2c): the slowest response a helper has seen crosses to its node whole and alone. Do encode
    /// the request and read it back, with a descriptor and cut short at every length; then answer it with 3.5 ms and
    /// with none (no hold seen yet), each read back, cut short, and read by another kind's reader. Expect each back
    /// exactly and every other refused.
    #[test]
    fn w14_the_slowest_response_seen_crosses_whole_and_alone() {
        let bytes = encode(VERSION, 41).unwrap();
        assert_eq!(read(&bytes, 0).unwrap(), (VERSION, 41));
        assert!(read(&bytes, 1).is_err());
        for size in 0..bytes.len() {
            assert!(read(&bytes[..size], 0).is_err());
        }
        assert!(read_request(&bytes, 0).is_err());
        assert!(rate::read(&bytes, 0).is_err());
        for slowest in [Some(Duration::from_micros(3_500)), None] {
            let answer = encode_answer(VERSION, 41, slowest).unwrap();
            assert_eq!(read_answer(&answer, 0).unwrap(), (VERSION, 41, slowest));
            assert!(read_answer(&answer, 1).is_err());
            for size in 0..answer.len() {
                assert!(read_answer(&answer[..size], 0).is_err());
            }
            assert!(super::super::read_answer(&answer, 0).is_err());
            assert!(read(&answer, 0).is_err());
        }
        assert!(encode_answer(VERSION, 41, Some(Duration::ZERO)).is_err());
    }
}
