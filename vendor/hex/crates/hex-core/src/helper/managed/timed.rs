//! An append-only native-helper request carrying one same-host monotonic expiry, or none where its
//! client has no deadline of its own (the review's C-9: its helper's waits then follow its connection).
//! It preserves the original managed request bytes and parser rather than creating a second command
//! grammar.
use super::*;

const KIND: u16 = 0x0012;
/// The same request with no expiry: its own kind, so a timed request's bytes keep their meaning.
const FOLLOWING: u16 = 0x0016;
const EXPIRY: usize = size_of::<u64>() + size_of::<u32>();
pub const MAX_MESSAGE: usize = super::MAX_MESSAGE + HEADER_BYTES + EXPIRY + size_of::<u32>();

pub fn encode(
    version: u16,
    id: u64,
    expires: Option<Duration>,
    request: &Request,
) -> Result<Vec<u8>, Fault> {
    if expires.is_some_and(|expires| expires.is_zero() || expires == Duration::MAX) {
        return Err(invalid("native request monotonic expiry"));
    }
    check_request(request)?;
    let mut inner = Count {
        bytes: 0,
        maximum: super::MAX_MESSAGE,
    };
    put_request(&mut inner, version, id, request)?;
    encode_bounded(MAX_MESSAGE, |out| {
        match expires {
            Some(expires) => {
                header(out, version, KIND, id)?;
                u64(out, expires.as_secs())?;
                u32(out, expires.subsec_nanos())?;
            }
            None => header(out, version, FOLLOWING, id)?,
        }
        u32(
            out,
            u32::try_from(inner.bytes).map_err(|_| invalid("bounded native request bytes"))?,
        )?;
        put_request(out, version, id, request)
    })
}

pub fn read(bytes: &[u8], fds: usize) -> Result<(u16, u64, Option<Duration>, Request), Fault> {
    if bytes.len() > MAX_MESSAGE {
        return Err(Fault::Oversized {
            bytes: bytes.len(),
            bound: MAX_MESSAGE,
        });
    }
    let (mut input, version, kind, id) = super::super::header(bytes)?;
    let expires = match kind {
        KIND => {
            let seconds = input.u64()?;
            let nanos = input.u32()?;
            if nanos >= 1_000_000_000 {
                return Err(invalid("native expiry nanoseconds"));
            }
            let expires = Duration::new(seconds, nanos);
            if expires.is_zero() || expires == Duration::MAX {
                return Err(invalid("native request monotonic expiry"));
            }
            Some(expires)
        }
        FOLLOWING => None,
        _ => {
            return Err(Fault::Kind {
                kind,
                version,
                expected: "a bounded native helper request",
            });
        }
    };
    let body = input.bytes()?;
    let (inner_version, inner_id, request) = read_request(body, fds)?;
    if version != inner_version || id != inner_id {
        return Err(invalid("native request envelope identity"));
    }
    input.end(kind, version)?;
    Ok((version, id, expires, request))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn control() -> Request {
        Request::Control {
            key: Key {
                owner: OwnerKey::Pool([1; 32]),
                scope: NativeScope::from_bytes([2; 16]),
                commitment: [3; 32],
                helper: ProcId {
                    pid: 7,
                    start_ticks: 11,
                },
            },
            action: Action::Resume,
            controller: Some(ProcId {
                pid: 13,
                start_ticks: 17,
            }),
        }
    }

    /// AC-T7.4.1: a late transferred Resume retains the original expiry and lifetime identity;
    /// transport cannot silently renew it or reinterpret a legacy control request as bounded.
    #[test]
    fn timed_native_request_keeps_expiry_and_exact_legacy_body() {
        let request = control();
        let expires = Duration::new(19, 23);
        let bytes = encode(VERSION, 29, Some(expires), &request).unwrap();
        assert_eq!(
            read(&bytes, 0).unwrap(),
            (VERSION, 29, Some(expires), request)
        );
        let original = encode_request(VERSION, 29, &control()).unwrap();
        assert!(bytes.ends_with(&original));
        assert!(read(&original, 0).is_err());
        assert!(read_request(&bytes, 0).is_err());
        assert!(read(&bytes, 1).is_err());
        for size in 0..bytes.len() {
            assert!(read(&bytes[..size], 0).is_err());
        }
        assert!(encode(VERSION, 29, Some(Duration::ZERO), &control()).is_err());
        assert!(encode(VERSION, 29, Some(Duration::MAX), &control()).is_err());
        let mut mismatched = bytes;
        mismatched[HEADER_BYTES + EXPIRY + size_of::<u32>() + 8
            ..HEADER_BYTES + EXPIRY + size_of::<u32>() + 16]
            .copy_from_slice(&31u64.to_le_bytes());
        assert!(read(&mismatched, 0).is_err());
    }

    /// The review's C-9 (c3): a request whose client has no deadline carries no expiry, in its own kind, and reads back
    /// with none, its body the same request's bytes; a timed request's bytes keep their kind and meaning.
    #[test]
    fn c9_a_request_with_no_deadline_carries_no_expiry_in_its_own_kind() {
        let request = control();
        let following = encode(VERSION, 31, None, &request).unwrap();
        assert_eq!(read(&following, 0).unwrap(), (VERSION, 31, None, control()));
        let original = encode_request(VERSION, 31, &control()).unwrap();
        assert!(following.ends_with(&original));
        assert_eq!(
            following.len() + EXPIRY,
            encode(VERSION, 31, Some(Duration::new(1, 0)), &control())
                .unwrap()
                .len()
        );
        for size in 0..following.len() {
            assert!(read(&following[..size], 0).is_err());
        }
    }
}
