//! Durable final view of the SAME helper's native receipts. The shell emits it only after every
//! retained native scope has positively released and finalized, before the helper itself exits.
//! This is native completion metadata, never another admission/holder table or process lookup.
use super::*;

const KIND: u16 = 0x8011;
const FIXED: usize = HEADER_BYTES + PROC_BYTES + size_of::<u32>();
const ENTRY_MAX: usize =
    KEY_BYTES + 1 + PROC_BYTES + 1 + size_of::<i32>() + size_of::<u32>() + PATH_MAX;
const ENTRY_MIN: usize =
    1 + size_of::<u32>() + 1 + 16 + 32 + PROC_BYTES + 1 + 1 + size_of::<u32>() + 1;

#[derive(Debug, PartialEq, Eq)]
pub struct Completed {
    pub key: Key,
    pub controller: Option<ProcId>,
    pub status: Exit,
    /// Actual prepared metadata only; scope/commitment/helper birth remain the incarnation fence.
    pub vsock: Vec<u8>,
}
#[derive(Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub helper: ProcId,
    pub owners: Vec<Completed>,
}

pub fn message_bound(native_bound: usize) -> Result<usize, Fault> {
    u32::try_from(native_bound).map_err(|_| invalid("native completed count width"))?;
    let bound = ENTRY_MAX
        .checked_mul(native_bound)
        .and_then(|size| size.checked_add(FIXED))
        .ok_or_else(|| invalid("native completed message width"))?;
    u32::try_from(bound).map_err(|_| invalid("native completed frame width"))?;
    Ok(bound)
}
impl Snapshot {
    pub fn check(&self, native_bound: usize) -> Result<(), Fault> {
        proc_check(Some(self.helper))?;
        if self.owners.len() > native_bound {
            return Err(Fault::Oversized {
                bytes: self.owners.len(),
                bound: native_bound,
            });
        }
        for entry in &self.owners {
            key_check(&entry.key)?;
            proc_check(entry.controller)?;
            if entry.vsock.is_empty() || entry.vsock.len() >= PATH_MAX || entry.vsock.contains(&0) {
                return Err(invalid("completed native socket metadata"));
            }
            if entry.key.helper != self.helper {
                return Err(invalid("completed helper incarnation"));
            }
        }
        if self
            .owners
            .windows(2)
            .any(|pair| matches!(pair,[a,b] if a.key.owner>=b.key.owner))
        {
            return Err(invalid("sorted unique completed owners"));
        }
        Ok(())
    }
}

pub fn encode(version: u16, snapshot: &Snapshot, native_bound: usize) -> Result<Vec<u8>, Fault> {
    snapshot.check(native_bound)?;
    encode_bounded(message_bound(native_bound)?, |out| {
        header(out, version, KIND, 0)?;
        put_proc(out, snapshot.helper)?;
        u32(
            out,
            u32::try_from(snapshot.owners.len()).map_err(|_| invalid("native completion count"))?,
        )?;
        for entry in &snapshot.owners {
            put_key(out, &entry.key)?;
            put_optional_proc(out, entry.controller)?;
            put_exit(out, entry.status)?;
            blob(out, &entry.vsock)?;
        }
        Ok(())
    })
}

pub fn read(bytes: &[u8], fds: usize, native_bound: usize) -> Result<(u16, Snapshot), Fault> {
    descriptors(fds, 0)?;
    let bound = message_bound(native_bound)?;
    if bytes.len() > bound {
        return Err(Fault::Oversized {
            bytes: bytes.len(),
            bound,
        });
    }
    let (mut input, version, kind, id) = super::super::header(bytes)?;
    if kind != KIND || id != 0 {
        return Err(invalid("native completed receipt envelope"));
    }
    let helper = read_proc(&mut input)?;
    let count =
        usize::try_from(input.u32()?).map_err(|_| invalid("native completed count width"))?;
    if count > native_bound {
        return Err(Fault::Oversized {
            bytes: count,
            bound: native_bound,
        });
    }
    let need = ENTRY_MIN
        .checked_mul(count)
        .and_then(|size| size.checked_add(input.at()))
        .ok_or_else(|| invalid("native completed minimum width"))?;
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
            what: "native completion receipts",
            bound: count,
        })?;
    for _ in 0..count {
        owners.push(Completed {
            key: read_key(&mut input)?,
            controller: input.option(read_proc)?,
            status: read_exit(&mut input)?,
            vsock: owned_blob(&mut input, PATH_MAX)?,
        });
    }
    input.end(kind, version)?;
    let snapshot = Snapshot { helper, owners };
    snapshot.check(native_bound)?;
    Ok((version, snapshot))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn snapshot() -> Snapshot {
        let helper = ProcId {
            pid: 11,
            start_ticks: 13,
        };
        Snapshot {
            helper,
            owners: vec![
                Completed {
                    key: Key {
                        owner: OwnerKey::Box(BoxId::new("first-finalized").unwrap()),
                        scope: NativeScope::from_bytes([17; 16]),
                        commitment: [19; 32],
                        helper,
                    },
                    controller: Some(ProcId {
                        pid: 23,
                        start_ticks: 29,
                    }),
                    status: Exit::Code(31),
                    vsock: b"/private/reusable-vsock".to_vec(),
                },
                Completed {
                    key: Key {
                        owner: OwnerKey::Pool([37; 32]),
                        scope: NativeScope::from_bytes([41; 16]),
                        commitment: [43; 32],
                        helper,
                    },
                    controller: None,
                    status: Exit::Unknown,
                    vsock: b"/private/other-vsock".to_vec(),
                },
            ],
        }
    }
    /// AC-T7.4.1: a last-helper shutdown retains earlier finalized scopes' immutable native facts,
    /// even when their canonical Released publication was interrupted; descriptor/name reuse grants nothing.
    #[test]
    fn completed_helper_receipt_retains_all_current_native_lifetimes() {
        let expected = snapshot();
        let bytes = encode(VERSION, &expected, 2).unwrap();
        assert_eq!(&bytes[..8], b"HEXM\x01\x00\x11\x80");
        assert_eq!(read(&bytes, 0, 2).unwrap(), (VERSION, expected));
        assert!(read(&bytes, 1, 2).is_err());
        assert!(super::super::read_answer(&bytes, 0).is_err());
        let mut earlier = snapshot();
        earlier.owners[0].controller.as_mut().unwrap().start_ticks += 1;
        assert_ne!(encode(VERSION, &earlier, 2).unwrap(), bytes);
        let mut reused = snapshot();
        reused.owners[0].key.scope = NativeScope::from_bytes([47; 16]);
        assert_ne!(encode(VERSION, &reused, 2).unwrap(), bytes);
        // An empty fresh helper may durably prove no VMM was submitted before failed startup.
        let empty = Snapshot {
            helper: snapshot().helper,
            owners: Vec::new(),
        };
        assert_eq!(
            read(&encode(VERSION, &empty, 0).unwrap(), 0, 0).unwrap().1,
            empty
        );
    }
    /// AC-T7.4.1/R01: interrupted, excessive, duplicate/foreign, trailing or legacy frames never
    /// become a completed native-owner snapshot or allocate an advertised hostile pointer count.
    #[test]
    fn completed_helper_receipt_refuses_partial_and_forged_frames() {
        let bytes = encode(VERSION, &snapshot(), 2).unwrap();
        for end in 0..bytes.len() {
            assert!(read(&bytes[..end], 0, 2).is_err());
        }
        assert!(read(&bytes, 0, 1).is_err());
        assert!(message_bound(usize::MAX).is_err());
        let mut hostile = bytes.clone();
        hostile[HEADER_BYTES + PROC_BYTES..FIXED].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(read(&hostile, 0, 2).is_err());
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(read(&trailing, 0, 2).is_err());
        let mut duplicate = snapshot();
        duplicate.owners[1].key = duplicate.owners[0].key.try_clone().unwrap();
        assert!(encode(VERSION, &duplicate, 2).is_err());
        let mut foreign = snapshot();
        foreign.owners[0].key.helper.start_ticks += 1;
        assert!(encode(VERSION, &foreign, 2).is_err());
        let mut order = snapshot();
        order.owners.reverse();
        assert!(encode(VERSION, &order, 2).is_err());
    }
}
