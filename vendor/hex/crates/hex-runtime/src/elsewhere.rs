//! Off Linux, until each platform's box is built: a typed refusal naming the platform
//! (CLAUDE.md §12). macOS's box is QoS and limits, Windows' a job object (docs/design/runtime.md);
//! both are open in STATUS.md.

use hex_api::Refusal;

/// The platform this build is for.
pub const PLATFORM: &str = std::env::consts::OS;

/// The refusal every class gives here.
pub fn unsupported(what: &'static str) -> Refusal {
    Refusal::Platform {
        what,
        platform: PLATFORM,
        why: "Hex's boxes are built for Linux first; this platform's box is open (STATUS.md)"
            .to_owned(),
    }
}
