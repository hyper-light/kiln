//! The paid guest executable's work-ready frame. These are guest observations;
//! neither UUID, array position nor heap bytes grants a host DeviceId.

use hex_api::{Program, Refusal};
use std::time::Duration;

use super::{Report, exhausted, fields, hex_array, invalid, word};

pub(super) const HEADER: &str = "hex-vulkan-guest-readiness 1 ";

/// The v1 inventory rows plus one native driver UUID/conformance row per device
/// and a final completed-device count. All limits derive from the Vulkan ABI.
pub(super) fn output_bound(expected: usize) -> Result<usize, Refusal> {
    let digits = usize::try_from(u32::MAX.ilog10())
        .ok()
        .and_then(|n| n.checked_add(1))
        .ok_or_else(|| exhausted(expected))?;
    let row = 5usize
        .checked_mul(digits)
        .and_then(|n| n.checked_add(super::UUID_HEX_BYTES))
        .and_then(|n| n.checked_add("driver".len()))
        .and_then(|n| n.checked_add(7)) // six spaces and LF.
        .ok_or_else(|| exhausted(expected))?;
    let header = HEADER
        .len()
        .checked_sub(super::HEADER.len())
        .ok_or_else(|| invalid("work-ready header exceeds its derived frame bound"))?;
    super::output_bound(expected)?
        .checked_add(
            expected
                .checked_mul(row)
                .ok_or_else(|| exhausted(expected))?,
        )
        .and_then(|n| n.checked_add(header))
        .and_then(|n| n.checked_add("ready ".len()))
        .and_then(|n| n.checked_add(digits))
        .and_then(|n| n.checked_add(1))
        .ok_or_else(|| exhausted(expected))
}

/// The child receives only a finite remainder. The original parent Instant also
/// bounds process launch, both streams, exit and cleanup; it is never restarted.
pub(super) fn program(
    configured: &super::config::GpuObserver,
    expected: usize,
    remaining: Duration,
) -> Result<Program, Refusal> {
    use std::fmt::Write as _;
    let nanos = u64::try_from(remaining.as_nanos())
        .ok()
        .filter(|n| *n != 0 && *n != u64::MAX)
        .ok_or_else(|| invalid("observer requires a positive finite fence deadline"))?;
    let mut program = configured.program(expected)?;
    program
        .args
        .try_reserve_exact(2)
        .map_err(|_| exhausted(2))?;
    let mut flag = String::new();
    flag.try_reserve_exact("--wait-ns".len())
        .map_err(|_| exhausted("--wait-ns".len()))?;
    flag.push_str("--wait-ns");
    let room = usize::try_from(u64::MAX.ilog10())
        .ok()
        .and_then(|n| n.checked_add(1))
        .ok_or_else(|| exhausted(size_of::<u64>()))?;
    let mut value = String::new();
    value.try_reserve_exact(room).map_err(|_| exhausted(room))?;
    write!(&mut value, "{nanos}").map_err(|_| exhausted(room))?;
    program.args.push(flag);
    program.args.push(value);
    program.check()?;
    Ok(program)
}

pub(super) fn check(text: &str, expected: usize, max_queues: u32) -> Result<(), Refusal> {
    let mut lines = super::frame(text, HEADER, expected, max_queues)?;
    for index in 0..expected {
        let heaps = super::device(lines.next(), index, max_queues)?;
        let [kind, ordinal, uuid, major, minor, subminor, patch] = fields::<7>(
            lines
                .next()
                .ok_or_else(|| invalid("readiness driver row is missing"))?,
        )?;
        if kind != "driver" || usize::try_from(word(ordinal)?).ok() != Some(index) {
            return Err(invalid("readiness driver order or device changed"));
        }
        hex_array::<16>(uuid)?;
        for part in [major, minor, subminor, patch] {
            u8::try_from(word(part)?)
                .map_err(|_| invalid("readiness conformance component exceeds its native byte"))?;
        }
        super::heaps(&mut lines, index, heaps)?;
    }
    let [kind, count] = fields::<2>(
        lines
            .next()
            .ok_or_else(|| invalid("readiness queue/fence completion is missing"))?,
    )?;
    if kind != "ready"
        || usize::try_from(word(count)?).ok() != Some(expected)
        || lines.next().is_some()
    {
        return Err(invalid(
            "readiness completion is partial or has trailing bytes",
        ));
    }
    Ok(())
}

impl Report {
    /// Inventory-only legacy receipts stay readable for their original cleanup.
    /// Every new Live/offer/payload consumer must additionally call this guard.
    pub(in crate::guest) fn require_work_ready(
        &self,
        expected: usize,
        queues: u32,
    ) -> Result<(), Refusal> {
        check(self.text(), expected, queues)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(count: usize) -> String {
        let mut out = format!("{HEADER}{count}\n");
        for index in 0..count {
            // Same-SKU Venus UUIDs may collide. Count/order frame observations;
            // this decoder deliberately neither joins nor selects a host device.
            out.push_str(&format!(
                "device {index} 4202496 22 1 1 2 2 {} 677075 76656e7573 4d657361 2 1 1 2 2\ndriver {index} {} 1 3 0 0\nheap {index} 0 1 4096\nheap {index} 1 0 8192\n",
                "ab".repeat(16), "cd".repeat(16),
            ));
        }
        out.push_str(&format!("ready {count}\n"));
        out
    }

    /// AC-T7.4.1: a completed set preserves every heap and lossy diagnostic UUID.
    #[test]
    fn complete_work_ready_frame_retains_lossy_uuids_driver_and_all_heaps() {
        let text = frame(2);
        let report = Report::parse(text.as_bytes(), 2, 2).unwrap();
        report.require_work_ready(2, 2).unwrap();
        assert_eq!(report.text(), text);
        assert!(text.len() <= output_bound(2).unwrap());
        assert!(report.require_work_ready(1, 2).is_err());
        assert!(report.require_work_ready(2, 1).is_err());
    }

    /// AC-T7.4.1: inventory receipt compatibility grants no new Live permission.
    #[test]
    fn inventory_is_cleanup_readable_but_never_new_work_readiness() {
        let legacy = format!(
            "{}1\ndevice 0 4202496 22 1 1 2 2 {} 677075 76656e7573 4d657361 2 1 1 2 1\nheap 0 0 1 4096\n",
            super::super::HEADER,
            "ab".repeat(16),
        );
        let report = Report::parse(legacy.as_bytes(), 1, 2).unwrap();
        report.check(1, 2).unwrap();
        assert!(report.require_work_ready(1, 2).is_err());
    }

    /// AC-T7.4.1: completed guest facts cannot migrate to another native scope.
    #[test]
    fn completed_driver_frame_stays_bound_to_original_scope_and_configuration() {
        let scope = hex_core::record::NativeScope::from_bytes([7; 16]);
        let evidence = super::super::Evidence {
            scope,
            configuration: [9; 32],
            max_queues: 2,
            report: Report::parse(frame(2).as_bytes(), 2, 2).unwrap(),
        };
        let mut encoded = String::new();
        evidence.render(&mut encoded).unwrap();
        let retained = super::super::Evidence::parse(&encoded, scope, 2)
            .unwrap()
            .unwrap();
        retained.report.require_work_ready(2, 2).unwrap();
        assert_eq!(retained, evidence);
        assert!(super::super::Evidence::parse(&encoded, scope, 1).is_err());
        assert!(
            super::super::Evidence::parse(
                &encoded,
                hex_core::record::NativeScope::from_bytes([8; 16]),
                2,
            )
            .is_err()
        );
    }

    /// AC-T7.4.1: build the paid Program's arguments and read the guest producer
    /// kept beside this decoder; expect it to write this frame's header and row
    /// keywords and to accept exactly the six flags `program` passes.
    #[test]
    fn producer_source_writes_this_frame_and_reads_every_passed_flag() {
        let producer = include_str!("hex-vulkan-guest-ready.c");
        for word in [HEADER.trim_end(), "device", "\\ndriver", "heap", "ready"] {
            assert!(producer.contains(&format!("\"{word}\"")), "{word}");
        }
        let dir = tempfile::tempdir().unwrap();
        let (config, input, allocation) = super::super::config::tests::fixture(dir.path());
        let observer = super::super::config::GpuObserver::new(input, &config, allocation).unwrap();
        let program = program(&observer, 1, Duration::from_secs(1)).unwrap();
        let flags: Vec<&String> = program
            .args
            .iter()
            .filter(|arg| arg.starts_with("--"))
            .collect();
        assert_eq!(flags.len(), 6, "{:?}", program.args);
        for flag in flags {
            assert!(producer.contains(&format!("\"{flag}\"")), "{flag}");
        }
    }

    /// AC-T7.4.1: decode the frame the built Program wrote in scripts/guest/vulkan-ready.sh's
    /// self-test, run through its closure's own dynamic linker, libc and Vulkan loader against a fake
    /// Venus ICD of one device with one queue family; expect one complete work-ready device.
    #[test]
    #[ignore = "decodes a built artifact: HEX_GUEST_READY_FRAME names vulkan-ready.sh's selftest/frame.txt"]
    fn built_program_frame_is_work_ready() {
        let path = std::env::var_os("HEX_GUEST_READY_FRAME").expect("HEX_GUEST_READY_FRAME is set");
        let frame = std::fs::read(path).unwrap();
        let report = Report::parse(&frame, 1, 8).unwrap();
        report.require_work_ready(1, 8).unwrap();
        assert!(report.text().ends_with("ready 1\n"), "{}", report.text());
    }

    /// AC-T7.4.1: hostile/partial data cannot become a smaller ready device set.
    #[test]
    fn cpu_unknown_partial_or_unbounded_driver_frame_never_admits() {
        let text = frame(1);
        for bad in [
            text.replace("1 1 2 2 ab", "1 1 2 4 ab"), // CPU.
            text.replace("1 1 2 2 ab", "1 1 2 0 ab"), // OTHER.
            text.replace("4202496 22", "4202496 13"), // software/foreign driver.
            text.replace("ready 1\n", ""),
            text.replace("ready 1", "ready 0"),
            text.replace("driver 0", "driver 1"),
            text.replace("1 3 0 0\n", "1 256 0 0\n"),
            text.replace(&"cd".repeat(16), &"CD".repeat(16)),
            text.replace("heap 0 1 0 8192", "heap 0 1 1 18446744073709551615"),
            format!("{text}ready 1\n"),
        ] {
            assert!(Report::parse(bad.as_bytes(), 1, 2).is_err(), "{bad}");
        }
        for end in 0..text.len() {
            assert!(Report::parse(text.get(..end).unwrap().as_bytes(), 1, 2).is_err());
        }
        let excess = vec![b'x'; output_bound(1).unwrap() + 1];
        assert!(Report::parse(&excess, 1, 2).is_err());
    }
}
