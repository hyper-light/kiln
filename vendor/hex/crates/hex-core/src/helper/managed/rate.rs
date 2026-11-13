//! Joint plan step 14 (C4 on macOS): an append-only native-helper request that sets one managed scope's CPU rate, its
//! water-filled share as the node's charge learner computes it each window. Its scope is named by its whole key, as a
//! control's is; the helper answers `Done`, or rejects it as it rejects a control it can't do.
use super::*;

const KIND: u16 = 0x0014;
/// The header, the key at its longest, and the rate.
pub const MAX_MESSAGE: usize = HEADER_BYTES + KEY_BYTES + size_of::<u64>();

/// The request setting `key`'s scope to `rate` milli-CPU, which is at least 1: a rate of nothing holds no scope.
pub fn encode(version: u16, id: u64, key: &Key, rate: u64) -> Result<Vec<u8>, Fault> {
    if rate == 0 {
        return Err(invalid("native CPU rate"));
    }
    encode_bounded(MAX_MESSAGE, |out| {
        header(out, version, KIND, id)?;
        put_key(out, key)?;
        u64(out, rate)
    })
}

/// The rate request in `bytes`, which carries no descriptor: its version, id, scope key and rate in milli-CPU.
pub fn read(bytes: &[u8], fds: usize) -> Result<(u16, u64, Key, u64), Fault> {
    if bytes.len() > MAX_MESSAGE {
        return Err(bounded(bytes.len()));
    }
    let (mut input, version, kind, id) = super::super::header(bytes)?;
    if kind != KIND {
        return Err(Fault::Kind {
            kind,
            version,
            expected: "a native CPU rate request",
        });
    }
    descriptors(fds, 0)?;
    let key = read_key(&mut input)?;
    let rate = input.u64()?;
    if rate == 0 {
        return Err(invalid("native CPU rate"));
    }
    input.end(kind, version)?;
    Ok((version, id, key, rate))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> Key {
        Key {
            owner: OwnerKey::Box(hex_api::BoxId::new("vm-1").unwrap()),
            scope: NativeScope::from_bytes([2; 16]),
            commitment: [3; 32],
            helper: ProcId {
                pid: 7,
                start_ticks: 11,
            },
        }
    }

    /// Joint plan step 14 (AC-T7.4.1), C4 on macOS: a scope's rate crosses to its native helper whole and alone. Do:
    /// encode a rate for a scope and read it back; read it with a descriptor, cut short at every length, as another
    /// kind's reader would, and with a rate of nothing; encode a rate of nothing. Expect it back exactly, and every
    /// other refused.
    #[test]
    fn a_rate_request_crosses_whole_and_alone() {
        let bytes = encode(VERSION, 31, &key(), 2_500).unwrap();
        assert!(bytes.len() <= MAX_MESSAGE);
        assert_eq!(read(&bytes, 0).unwrap(), (VERSION, 31, key(), 2_500));
        assert!(read(&bytes, 1).is_err());
        for size in 0..bytes.len() {
            assert!(read(&bytes[..size], 0).is_err());
        }
        assert!(read_request(&bytes, 0).is_err());
        assert!(timed::read(&bytes, 0).is_err());
        let mut nothing = bytes.clone();
        let at = nothing.len() - size_of::<u64>();
        nothing[at..].copy_from_slice(&0u64.to_le_bytes());
        assert!(read(&nothing, 0).is_err());
        assert!(encode(VERSION, 31, &key(), 0).is_err());
    }
}
