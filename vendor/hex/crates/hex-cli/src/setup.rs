//! `hex setup features`: the state of each Windows feature Hex's installer turns on, read by the node's own
//! bounded query (`hex_runtime::windows::features`, within `hex_core::setup::FEATURE_QUERY_PATIENCE`) and
//! printed one `<name> on|off` line each (`hex_core::setup::render_features`). The Windows installer
//! (scripts/install/windows.ps1) reads this rather than querying Windows itself, so the installer and the
//! node share one query and one bound (native plan step 6, HEX-INST-9). Off Windows it is refused by name.

use std::io::Write;

const USAGE: &str = "usage: hex setup features";

/// Runs `hex setup`'s arguments (everything after `setup`); returns the exit status.
pub fn main(args: &[String]) -> u8 {
    let (status, out, err) = match args {
        [verb] if verb == "features" => features(),
        _ => (2, String::new(), format!("hex setup: {USAGE}\n")),
    };
    let wrote = std::io::stdout()
        .lock()
        .write_all(out.as_bytes())
        .and_then(|()| std::io::stderr().lock().write_all(err.as_bytes()));
    // A closed output is a failure the status reports, never a panic.
    if wrote.is_err() { 1 } else { status }
}

/// The status and what to print for `hex setup features`.
#[cfg(target_os = "windows")]
fn features() -> (u8, String, String) {
    match hex_runtime::windows::features::off() {
        Ok(off) => (0, hex_core::setup::render_features(&off), String::new()),
        Err(why) => (1, String::new(), format!("hex setup features: {why}\n")),
    }
}

/// The status and what to print for `hex setup features`: off Windows, a refusal naming the platform.
#[cfg(not(target_os = "windows"))]
fn features() -> (u8, String, String) {
    (
        2,
        String::new(),
        format!(
            "hex setup features isn't available on {}: it reads the Windows optional features Hex's installer turns on\n",
            std::env::consts::OS
        ),
    )
}
