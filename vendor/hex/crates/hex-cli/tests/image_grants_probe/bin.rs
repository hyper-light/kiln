//! The grants probe's program (joint plan step 15; `probe.rs` says what it does). `scripts/grants-probe/build.sh`
//! builds it with rustc alone, static, for the guest; cargo never does, and `image_grants_real` tests `probe.rs`.

// The probe's report is read by the test alone (`Report::parse`), so the program leaves it unused.
#[allow(dead_code)]
#[path = "probe.rs"]
mod probe;

use std::io::Write;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let started = match probe::start(&args) {
        Ok(started) => started,
        Err(why) => {
            let _ = writeln!(std::io::stderr(), "grants probe: {why}");
            return ExitCode::FAILURE;
        }
    };
    let _ = writeln!(
        std::io::stderr(),
        "grants probe: listening at {:?}",
        started.at
    );
    for server in started.servers {
        let _ = server.join();
    }
    // Its servers never end while its VM runs: one that did is the probe's failure.
    ExitCode::FAILURE
}
