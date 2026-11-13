//! An append-only native-helper request carrying one same-host monotonic expiry. It preserves
//! the original managed request bytes and parser rather than creating a second command grammar.
use super::*;

const KIND: u16 = 0x0012;
const EXPIRY: usize = size_of::<u64>() + size_of::<u32>();
pub const MAX_MESSAGE: usize = super::MAX_MESSAGE + HEADER_BYTES + EXPIRY + size_of::<u32>();

pub fn encode(
    version: u16,
    id: u64,
    expires: Duration,
    request: &Request,
) -> Result<Vec<u8>, Fault> {
    if expires.is_zero() || expires == Duration::MAX {
        return Err(invalid("native request monotonic expiry"));
    }
    check_request(request)?;
    let mut inner = Count {
        bytes: 0,
        maximum: super::MAX_MESSAGE,
    };
    put_request(&mut inner, version, id, request)?;
    encode_bounded(MAX_MESSAGE, |out| {
        header(out, version, KIND, id)?;
        u64(out, expires.as_secs())?;
        u32(out, expires.subsec_nanos())?;
        u32(
            out,
            u32::try_from(inner.bytes).map_err(|_| invalid("bounded native request bytes"))?,
        )?;
        put_request(out, version, id, request)
    })
}

pub fn read(bytes: &[u8], fds: usize) -> Result<(u16, u64, Duration, Request), Fault> {
    if bytes.len() > MAX_MESSAGE {
        return Err(Fault::Oversized {
            bytes: bytes.len(),
            bound: MAX_MESSAGE,
        });
    }
    let (mut input, version, kind, id) = super::super::header(bytes)?;
    if kind != KIND {
        return Err(Fault::Kind {
            kind,
            version,
            expected: "a bounded native helper request",
        });
    }
    let seconds = input.u64()?;
    let nanos = input.u32()?;
    if nanos >= 1_000_000_000 {
        return Err(invalid("native expiry nanoseconds"));
    }
    let expires = Duration::new(seconds, nanos);
    if expires.is_zero() || expires == Duration::MAX {
        return Err(invalid("native request monotonic expiry"));
    }
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
        let bytes = encode(VERSION, 29, expires, &request).unwrap();
        assert_eq!(read(&bytes, 0).unwrap(), (VERSION, 29, expires, request));
        let original = encode_request(VERSION, 29, &control()).unwrap();
        assert!(bytes.ends_with(&original));
        assert!(read(&original, 0).is_err());
        assert!(read_request(&bytes, 0).is_err());
        assert!(read(&bytes, 1).is_err());
        for size in 0..bytes.len() {
            assert!(read(&bytes[..size], 0).is_err());
        }
        assert!(encode(VERSION, 29, Duration::ZERO, &control()).is_err());
        assert!(encode(VERSION, 29, Duration::MAX, &control()).is_err());
        let mut mismatched = bytes;
        mismatched[HEADER_BYTES + EXPIRY + size_of::<u32>() + 8
            ..HEADER_BYTES + EXPIRY + size_of::<u32>() + 16]
            .copy_from_slice(&31u64.to_le_bytes());
        assert!(read(&mismatched, 0).is_err());
    }
}
