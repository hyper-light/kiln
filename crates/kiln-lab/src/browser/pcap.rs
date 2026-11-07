//! The bytes a packet capture saw (step 4's check of the interface counters): a pcap file's records,
//! each frame's whole length, between two times. Only the classic pcap format tcpdump writes
//! (microsecond or nanosecond stamps, either byte order).

fn u32_at(b: &[u8], at: usize, little: bool) -> Option<u32> {
    let s: [u8; 4] = b.get(at..at + 4)?.try_into().ok()?;
    Some(if little { u32::from_le_bytes(s) } else { u32::from_be_bytes(s) })
}

/// The frames' original lengths summed over records stamped from `from_us` to `to_us`.
pub fn bytes_between(file: &[u8], from_us: u64, to_us: u64) -> u64 {
    let Some(magic) = u32_at(file, 0, true) else { return 0 };
    let (little, nanos) = match magic {
        0xa1b2_c3d4 => (true, false),
        0xa1b2_3c4d => (true, true),
        0xd4c3_b2a1 => (false, false),
        0x4d3c_b2a1 => (false, true),
        _ => return 0,
    };
    let mut at = 24;
    let mut total = 0u64;
    while let (Some(sec), Some(frac), Some(incl), Some(orig)) = (u32_at(file, at, little), u32_at(file, at + 4, little), u32_at(file, at + 8, little), u32_at(file, at + 12, little)) {
        let us = u64::from(sec) * 1_000_000 + if nanos { u64::from(frac) / 1000 } else { u64::from(frac) };
        if us >= from_us && us <= to_us {
            total += u64::from(orig);
        }
        at += 16 + incl as usize;
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn g1_s4_a_capture_sums_its_frames_in_the_window() {
        let mut f = vec![0u8; 24];
        f.splice(0..4, 0xa1b2_c3d4_u32.to_le_bytes());
        for (sec, orig) in [(10u32, 1514u32), (11, 66), (13, 1000)] {
            for v in [sec, 0, 4, orig] {
                f.extend(v.to_le_bytes());
            }
            f.extend([0u8; 4]);
        }
        assert_eq!(bytes_between(&f, 10_000_000, 12_000_000), 1580);
        assert_eq!(bytes_between(&f, 0, u64::MAX), 2580);
        assert_eq!(bytes_between(&[1, 2, 3], 0, 1), 0);
    }
}
