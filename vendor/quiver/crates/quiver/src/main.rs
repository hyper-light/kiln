//! The `quiver` binary.

use std::io::Write;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<Option<String>> = std::env::args_os()
        .skip(1)
        .map(|a| a.into_string().ok())
        .collect();
    let reply = quiver::reply(&args);
    let wrote = if reply.to_stderr {
        std::io::stderr().lock().write_all(reply.text.as_bytes())
    } else {
        std::io::stdout().lock().write_all(reply.text.as_bytes())
    };
    // A closed output is a failure the status reports, never a panic.
    match wrote {
        Ok(()) => ExitCode::from(reply.status),
        Err(_) => ExitCode::from(1),
    }
}
