//! PID 1 of kiln's carried-Shards smoke (docs/research/s16-vendor/smoke.sh): it says it ran, then syncs and
//! powers its VM off, which its VM process reports as the guest's end (shards-api's `End::PowerOff`). Built
//! static for this host's architecture's musl target, linked by rust-lld, so no C toolchain is needed.

use std::io::Write as _;

unsafe extern "C" {
    fn sync();
    fn reboot(how: i32) -> i32;
}

/// musl's `RB_POWER_OFF` (sys/reboot.h), Linux's `LINUX_REBOOT_CMD_POWER_OFF`.
const RB_POWER_OFF: i32 = 0x4321_fedc_u32 as i32;

fn main() {
    let _ = writeln!(std::io::stdout(), "kiln-smoke: PID 1 ran in the guest");
    // SAFETY: both are musl's own, given what they take; reboot returns only where it fails.
    unsafe {
        sync();
        reboot(RB_POWER_OFF);
    }
}
