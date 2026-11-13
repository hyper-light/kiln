//! Read-only flat view of the SAME helper's native-owner table. This snapshot never grants,
//! creates or removes a scope; its caller joins every key to the canonical Box/Pool records.
use super::*;

const REQUEST: u16 = 0x0011;
const ANSWER: u16 = 0x800f;
const FIXED: usize = HEADER_BYTES + PROC_BYTES + size_of::<u32>();
const MIN_KEY_BYTES: usize = 1 + size_of::<u32>() + 1 + 16 + 32 + PROC_BYTES;

#[derive(Debug, PartialEq, Eq)]
pub struct Census {
    pub helper: ProcId,
    pub owners: Vec<Key>,
}

/// The caller supplies its existing native table bound (runtime MAX_BOXES), never a wire value.
/// Avoids another independently tuned owner allowance and checks framing width before allocation.
pub fn message_bound(native_bound: usize) -> Result<usize, Fault> {
    u32::try_from(native_bound).map_err(|_| invalid("native table count representation"))?;
    let bytes = KEY_BYTES
        .checked_mul(native_bound)
        .and_then(|n| n.checked_add(FIXED))
        .ok_or_else(|| invalid("native census byte bound"))?;
    u32::try_from(bytes).map_err(|_| invalid("native census frame representation"))?;
    Ok(bytes)
}

impl Census {
    pub fn check(&self, native_bound: usize) -> Result<(), Fault> {
        proc_check(Some(self.helper))?;
        if self.owners.len() > native_bound {
            return Err(Fault::Oversized {
                bytes: self.owners.len(),
                bound: native_bound,
            });
        }
        for key in &self.owners {
            key_check(key)?;
            if key.helper != self.helper {
                return Err(invalid("native census helper incarnation"));
            }
        }
        if self
            .owners
            .windows(2)
            .any(|pair| matches!(pair,[a,b] if a.owner>=b.owner))
        {
            return Err(invalid("native census sorted unique owners"));
        }
        Ok(())
    }
}

pub fn encode_request(version: u16, id: u64, helper: ProcId) -> Result<Vec<u8>, Fault> {
    encode_bounded(FIXED, |out| {
        header(out, version, REQUEST, id)?;
        put_proc(out, helper)
    })
}

pub fn read_request(bytes: &[u8], fds: usize) -> Result<(u16, u64, ProcId), Fault> {
    descriptors(fds, 0)?;
    if bytes.len() > FIXED {
        return Err(Fault::Oversized {
            bytes: bytes.len(),
            bound: FIXED,
        });
    }
    let (mut input, version, kind, id) = super::super::header(bytes)?;
    if kind != REQUEST {
        return Err(Fault::Kind {
            kind,
            version,
            expected: "a native-owner census request",
        });
    }
    let helper = read_proc(&mut input)?;
    input.end(kind, version)?;
    Ok((version, id, helper))
}

pub fn encode_answer(
    version: u16,
    id: u64,
    census: &Census,
    native_bound: usize,
) -> Result<Vec<u8>, Fault> {
    census.check(native_bound)?;
    let maximum = message_bound(native_bound)?;
    encode_bounded(maximum, |out| {
        header(out, version, ANSWER, id)?;
        put_proc(out, census.helper)?;
        u32(
            out,
            u32::try_from(census.owners.len()).map_err(|_| invalid("native census count"))?,
        )?;
        for key in &census.owners {
            put_key(out, key)?;
        }
        Ok(())
    })
}

pub fn read_answer(
    bytes: &[u8],
    fds: usize,
    native_bound: usize,
) -> Result<(u16, u64, Census), Fault> {
    descriptors(fds, 0)?;
    let maximum = message_bound(native_bound)?;
    if bytes.len() > maximum {
        return Err(Fault::Oversized {
            bytes: bytes.len(),
            bound: maximum,
        });
    }
    let (mut input, version, kind, id) = super::super::header(bytes)?;
    if kind != ANSWER {
        return Err(Fault::Kind {
            kind,
            version,
            expected: "a native-owner census answer",
        });
    }
    let helper = read_proc(&mut input)?;
    let count =
        usize::try_from(input.u32()?).map_err(|_| invalid("native census count representation"))?;
    if count > native_bound {
        return Err(Fault::Oversized {
            bytes: count,
            bound: native_bound,
        });
    }
    // Check the available minimum encoded key space BEFORE reserving a hostile advertised count.
    let need = MIN_KEY_BYTES
        .checked_mul(count)
        .and_then(|n| n.checked_add(input.at()))
        .ok_or_else(|| invalid("native census key size"))?;
    if need > bytes.len() {
        return Err(Fault::Truncated {
            need,
            have: bytes.len(),
        });
    }
    let mut owners = Vec::new();
    owners
        .try_reserve_exact(count)
        .map_err(|_| Fault::Storage {
            what: "native census keys",
            bound: count,
        })?;
    for _ in 0..count {
        owners.push(read_key(&mut input)?);
    }
    input.end(kind, version)?;
    let census = Census { helper, owners };
    census.check(native_bound)?;
    Ok((version, id, census))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn helper() -> ProcId {
        ProcId {
            pid: 11,
            start_ticks: 13,
        }
    }
    fn key(owner: OwnerKey, n: u8) -> Key {
        Key {
            owner,
            scope: NativeScope::from_bytes([n; 16]),
            commitment: [n; 32],
            helper: helper(),
        }
    }
    fn snapshot() -> Census {
        Census {
            helper: helper(),
            owners: vec![
                key(OwnerKey::Box(BoxId::new("held-box").unwrap()), 1),
                key(OwnerKey::Pool([3; 32]), 2),
            ],
        }
    }

    /// AC-T7.4.1: the returned view retains the same native scope/commitment/helper incarnation;
    /// the old managed/legacy headers keep their existing meaning and descriptor rules.
    #[test]
    fn native_census_roundtrips_without_reinterpreting_legacy_messages() {
        let request = encode_request(VERSION, 17, helper()).unwrap();
        assert_eq!(&request[..4], &MAGIC);
        assert_eq!(&request[6..8], &REQUEST.to_le_bytes());
        assert_eq!(read_request(&request, 0).unwrap(), (VERSION, 17, helper()));
        let census = snapshot();
        let answer = encode_answer(VERSION, 19, &census, 2).unwrap();
        assert_eq!(&answer[6..8], &ANSWER.to_le_bytes());
        assert_eq!(read_answer(&answer, 0, 2).unwrap(), (VERSION, 19, census));
        assert!(read_request(&request, 1).is_err());
        assert!(read_answer(&answer, 1, 2).is_err());
        let old = super::super::encode_request(
            VERSION,
            23,
            &Request::Control {
                key: key(OwnerKey::Pool([3; 32]), 2),
                action: Action::Terminal,
                controller: None,
            },
        )
        .unwrap();
        assert!(super::super::read_request(&old, 0).is_ok());
        assert!(read_request(&old, 0).is_err());
    }

    /// AC-T7.4.1/R01: malformed/foreign/duplicate views refuse before granting any scope authority;
    /// truncated or excessive counts never become a partial accepted snapshot.
    #[test]
    fn native_census_refuses_hostile_counts_and_reused_or_unordered_owners() {
        let census = snapshot();
        let answer = encode_answer(VERSION, 29, &census, 2).unwrap();
        for size in 0..answer.len() {
            assert!(read_answer(&answer[..size], 0, 2).is_err());
        }
        assert!(read_answer(&answer, 0, 1).is_err());
        let mut hostile = answer.clone();
        hostile[HEADER_BYTES + PROC_BYTES..FIXED].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(read_answer(&hostile, 0, 2).is_err());
        assert!(message_bound(usize::MAX).is_err());
        let mut foreign = snapshot();
        foreign.owners[0].helper.start_ticks = 17;
        assert!(encode_answer(VERSION, 31, &foreign, 2).is_err());
        let mut duplicate = snapshot();
        duplicate.owners[1] = duplicate.owners[0].try_clone().unwrap();
        assert!(encode_answer(VERSION, 31, &duplicate, 2).is_err());
        let mut unordered = snapshot();
        unordered.owners.reverse();
        assert!(encode_answer(VERSION, 31, &unordered, 2).is_err());
    }
}
