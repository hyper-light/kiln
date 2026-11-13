//! A diagnostic-only refusal before a valid scope key exists. It cannot grant or release anything.
use super::*;

const KIND: u16 = 0x8012;
// Failure's largest body is Allocation(u64), including its tag. I/O uses three tags plus i32.
const FAILURE_BYTES: usize = 1 + size_of::<u64>();
pub const MAX_MESSAGE: usize = HEADER_BYTES + FAILURE_BYTES + size_of::<u32>() + ARG_MAX;

#[derive(Debug, PartialEq, Eq)]
pub struct Rejected {
    pub failure: Failure,
    pub detail: Vec<u8>,
}

pub fn encode(version: u16, id: u64, rejected: &Rejected) -> Result<Vec<u8>, Fault> {
    if rejected.detail.len() > ARG_MAX {
        return Err(Fault::Oversized {
            bytes: rejected.detail.len(),
            bound: ARG_MAX,
        });
    }
    encode_bounded(MAX_MESSAGE, |out| {
        header(out, version, KIND, id)?;
        put_failure(out, rejected.failure)?;
        blob(out, &rejected.detail)
    })
}
pub fn read(bytes: &[u8], fds: usize) -> Result<(u16, u64, Rejected), Fault> {
    descriptors(fds, 0)?;
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
            expected: "a diagnostic native-helper refusal",
        });
    }
    let failure = read_failure(&mut input)?;
    let detail = owned_blob(&mut input, ARG_MAX)?;
    input.end(kind, version)?;
    Ok((version, id, Rejected { failure, detail }))
}

#[cfg(test)]
mod tests {
    use super::*;
    /// AC-T7.4.1/R01: diagnostic errors retain actual cause bytes but cannot be decoded as attached,
    /// terminal/completed ownership; partial, hostile count, descriptors and trailing bytes refuse.
    #[test]
    fn unscoped_rejection_preserves_cause_without_terminal_authority() {
        let error = std::io::Error::from_raw_os_error(13);
        let value = Rejected {
            failure: Failure::Io {
                operation: Operation::Finalize,
                code: error.raw_os_error(),
                kind: error.kind(),
            },
            detail: b"same-receipt write refused".to_vec(),
        };
        let bytes = encode(VERSION, 17, &value).unwrap();
        assert_eq!(&bytes[..8], b"HEXM\x01\x00\x12\x80");
        assert_eq!(read(&bytes, 0).unwrap(), (VERSION, 17, value));
        assert!(super::super::read_answer(&bytes, 0).is_err());
        assert!(super::super::completed::read(&bytes, 0, 1).is_err());
        for end in 0..bytes.len() {
            assert!(read(&bytes[..end], 0).is_err());
        }
        assert!(read(&bytes, 1).is_err());
        let mut trailing = bytes;
        trailing.push(0);
        assert!(read(&trailing, 0).is_err());
        assert!(
            encode(
                VERSION,
                17,
                &Rejected {
                    failure: Failure::Control,
                    detail: vec![0; ARG_MAX + 1]
                }
            )
            .is_err()
        );
    }
}
