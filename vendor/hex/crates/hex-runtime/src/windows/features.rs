//! Which of the Windows features Hex's installer turns on are off, read without elevation (native plan
//! step 6; docs/research/runtime/installer-per-os.md §2.4, §2.6): WMI's `Win32_OptionalFeature`, queried
//! in process ([`super::wmi`], [`setup::FEATURE_QUERY`]), its rows put as the listing
//! [`setup::features_off`] parses. The `oci` class's refusal names each feature that's off
//! ([`setup::windows_oci`]).

use std::collections::BTreeSet;
use std::time::Duration;

use hex_api::Refusal;
use hex_core::setup::{self, WindowsFeature};

use super::wmi;

/// The longest the read may take before it is refused by name: twice its slowest recorded run
/// ([`setup::FEATURE_QUERY_PATIENCE`]), a bound on a wait (CLAUDE.md §2).
const PATIENCE: Duration = setup::FEATURE_QUERY_PATIENCE;

/// The installer's features that are off on this machine.
pub fn off() -> Result<BTreeSet<WindowsFeature>, Refusal> {
    let rows = wmi::query(
        setup::FEATURE_QUERY,
        "Name",
        "InstallState",
        WindowsFeature::ALL.len(),
        PATIENCE,
    )?;
    let listing: String = rows
        .iter()
        .map(|(name, state)| format!("{name} {state}\r\n"))
        .collect();
    setup::features_off(&listing)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// HEX-INST-9 (native plan step 6; the Hex lead, 2026-11-06): the node reads the features in process,
    /// through WMI, and starts no process for it: PowerShell's start cost the read about a second, and a
    /// process past the bound is one more thing to end. Do the read, expect this thread to have started
    /// none ([`super::super::sys::started_here`] counts every process `create` starts on it).
    #[test]
    fn the_feature_read_starts_no_process() {
        let before = super::super::sys::started_here();
        let read = off();
        let started = super::super::sys::started_here() - before;
        assert_eq!(
            started, 0,
            "the feature read started {started} processes ({read:?})"
        );
        read.unwrap();
    }
}
