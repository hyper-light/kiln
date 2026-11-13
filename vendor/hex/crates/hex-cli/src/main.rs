//! The `hex` binary.

use std::io::Write;
use std::process::ExitCode;

fn main() -> ExitCode {
    #[cfg(all(target_os = "macos", feature = "native-trusted-entry-measurement"))]
    match hex_runtime::macos::trusted_entry::dispatch() {
        Ok(Some(code)) => return ExitCode::from(code),
        Ok(None) => {}
        Err(failure) => {
            if let Err(write_error) =
                writeln!(std::io::stderr().lock(), "TRUSTED_ENTRY_ERROR {failure}")
            {
                let _ = writeln!(
                    std::io::stdout().lock(),
                    "TRUSTED_ENTRY_ERROR {failure}; DIAGNOSTIC_WRITE {write_error}"
                );
            }
            return ExitCode::FAILURE;
        }
    }
    // The macOS node's persistent managed-VM owner, the same binary started only by its node, before
    // any other CLI work; its paths are native bytes, so it reads them as such (AC-T7.4.1).
    #[cfg(target_os = "macos")]
    {
        let words: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
        if let [first, rest @ ..] = words.as_slice()
            && first == "native-managed-helper"
        {
            return ExitCode::from(hex_cli::managed_helper::main(rest));
        }
    }
    let args: Vec<Option<String>> = std::env::args_os()
        .skip(1)
        .map(|a| a.into_string().ok())
        .collect();
    // The kernel's first process, `hex` is a Shards guest's init, whatever words the kernel passes it.
    #[cfg(target_os = "linux")]
    if hex_cli::guest::runs_as_init(std::process::id(), std::env::args_os().next().as_deref()) {
        return ExitCode::from(hex_cli::guest::init_main());
    }
    // `hex selftest-box <act> …` runs a small program inside a box (hex_cli::selftest); it ends the
    // process itself rather than returning text, so it is handled before the text replies.
    if let [Some(first), rest @ ..] = args.as_slice()
        && first == "selftest-box"
        && rest.iter().all(Option::is_some)
    {
        let acts: Vec<String> = rest.iter().flatten().cloned().collect();
        if let Some(status) = hex_cli::selftest::run(&acts) {
            return ExitCode::from(status);
        }
    }
    // The Windows `oci` class's launcher, which HCS runs inside a container to start a program in its
    // box's job (hex_runtime::windows::oci::launch).
    #[cfg(target_os = "windows")]
    if let [Some(first)] = args.as_slice()
        && first == "oci-launch"
    {
        return ExitCode::from(hex_runtime::windows::oci::launch::serve());
    }
    // `hex node --socket …` serves the Runtime seam on the node agent's local socket.
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    if let [Some(first), rest @ ..] = args.as_slice()
        && first == "node"
        && rest.iter().all(Option::is_some)
    {
        let rest: Vec<String> = rest.iter().flatten().cloned().collect();
        return ExitCode::from(hex_cli::node::main(&rest));
    }
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    if let [Some(first), rest @ ..] = args.as_slice()
        && first == "schedule"
        && rest.iter().all(Option::is_some)
    {
        let rest: Vec<String> = rest.iter().flatten().cloned().collect();
        return ExitCode::from(hex_cli::schedule::main(&rest));
    }
    // `hex keep` is the Windows node's keeper, a passive process holding its boxes' job handles so they
    // outlive the node (hex_runtime::windows::keeper); it takes its state folder and ready path from the
    // environment the node starts it with.
    #[cfg(windows)]
    if let [Some(first), rest @ ..] = args.as_slice()
        && first == "keep"
        && rest.iter().all(Option::is_some)
    {
        let rest: Vec<String> = rest.iter().flatten().cloned().collect();
        return ExitCode::from(hex_cli::keep(&rest));
    }
    // The `oci` class's command line and its re-exec'd container init (Linux only).
    #[cfg(target_os = "linux")]
    if let [Some(first), rest @ ..] = args.as_slice()
        && rest.iter().all(Option::is_some)
    {
        if first == "__oci-init" {
            return ExitCode::from(hex_cli::oci::init());
        }
        if first == "__oci-exec" {
            return ExitCode::from(hex_cli::oci::exec_helper());
        }
        if first == "__oci-seed" {
            return ExitCode::from(hex_cli::oci::seed());
        }
        if first == "oci" {
            let rest: Vec<String> = rest.iter().flatten().cloned().collect();
            return ExitCode::from(hex_cli::oci::main(&rest));
        }
        // `hex guest relay …` relays a Shards guest's vsock to its node agent's socket.
        if first == "guest" {
            let rest: Vec<String> = rest.iter().flatten().cloned().collect();
            return ExitCode::from(hex_cli::guest::main(&rest));
        }
        // `hex box run <limits> -- program args…` runs a program in a process box.
        if first == "box"
            && let [Some(verb), box_args @ ..] = rest
            && verb == "run"
            && box_args.iter().all(Option::is_some)
        {
            let box_args: Vec<String> = box_args.iter().flatten().cloned().collect();
            return ExitCode::from(hex_cli::box_run::main(&box_args));
        }
    }
    // The macOS box helper daemon (`hex __box-helper --socket … --users …`); macOS only.
    #[cfg(target_os = "macos")]
    if let [Some(first), rest @ ..] = args.as_slice()
        && first == "__box-helper"
        && rest.iter().all(Option::is_some)
    {
        let rest: Vec<String> = rest.iter().flatten().cloned().collect();
        return ExitCode::from(hex_cli::box_helper::main(&rest));
    }
    // `hex setup features`: the Windows features' state for the installer (hex_cli::setup).
    if let [Some(first), rest @ ..] = args.as_slice()
        && first == "setup"
        && rest.iter().all(Option::is_some)
    {
        let rest: Vec<String> = rest.iter().flatten().cloned().collect();
        return ExitCode::from(hex_cli::setup::main(&rest));
    }
    let reply = hex_cli::reply(&args);
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
