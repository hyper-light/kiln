//! `hex __box-helper --socket <path> (--users <uid,…> | --pool-group <name> --pool-gid <gid> --manifest
//! <path> --peak-file <path>)`: the macOS box helper daemon (native plan step 2). Run as root — by launchd
//! from the installer, or by hand in the lab — it serves the node over the Unix socket at `--socket`,
//! starting each box's programs as a pool user: the fixed `--users` (the lab's), or a pool it grows on
//! demand in the `--pool-group`, recording each account it makes in the installer's `--manifest` and the
//! peak of concurrent boxes in `--peak-file` (the installer's, the lead's call 2026-11-06). It caps each box's
//! processes (`RLIMIT_NPROC`) at its spec's `pids_max`, else at the platform's per-uid ceiling
//! (`kern.maxprocperuid`, read at start). macOS only; the node reaches it through
//! [`hex_runtime::macos::helper_client`]. It does one job and refuses every other request by name
//! ([`hex_core::helper`]).

use std::io::Write;
use std::path::{Path, PathBuf};

use hex_runtime::macos::directory::Directory;
use hex_runtime::macos::helper_daemon::Daemon;
use hex_runtime::macos::helper_sys::RealPriv;
use hex_runtime::macos::sys;

/// Runs the daemon until it fails (it serves forever otherwise). Returns a non-zero status on a bad
/// invocation or a socket error.
pub fn main(args: &[String]) -> u8 {
    let mut socket: Option<String> = None;
    let mut users: Vec<u32> = Vec::new();
    let mut group: Option<String> = None;
    let mut gid: Option<u32> = None;
    let mut manifest: Option<PathBuf> = None;
    let mut peak_file: Option<PathBuf> = None;
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--socket" => socket = it.next().cloned(),
            "--users" => {
                let parsed: Result<Vec<u32>, _> = it
                    .next()
                    .map_or("", String::as_str)
                    .split(',')
                    .map(str::parse::<u32>)
                    .collect();
                match parsed {
                    Ok(u) => users = u,
                    Err(e) => {
                        let _ = writeln!(
                            std::io::stderr().lock(),
                            "hex __box-helper: --users takes uids separated by commas: {e}"
                        );
                        return 2;
                    }
                }
            }
            "--pool-group" => group = it.next().cloned(),
            "--pool-gid" => gid = it.next().and_then(|g| g.parse().ok()),
            "--manifest" => manifest = it.next().map(PathBuf::from),
            "--peak-file" => peak_file = it.next().map(PathBuf::from),
            other => {
                let _ = writeln!(
                    std::io::stderr().lock(),
                    "hex __box-helper: unknown argument {other}"
                );
                return 2;
            }
        }
    }
    let Some(socket) = socket else {
        let _ = writeln!(
            std::io::stderr().lock(),
            "hex __box-helper: --socket <path> is required"
        );
        return 2;
    };
    let directory = match (group, gid, manifest, peak_file) {
        (Some(group), Some(gid), Some(manifest), Some(peak_file)) if users.is_empty() => {
            Some(Directory {
                group,
                gid,
                manifest,
                peak_file,
            })
        }
        (None, None, None, None) if !users.is_empty() => None,
        _ => {
            let _ = writeln!(
                std::io::stderr().lock(),
                "hex __box-helper: give --users <uid,…> (a fixed pool), or --pool-group, --pool-gid, \
                 --manifest and --peak-file (a pool grown on demand), not both"
            );
            return 2;
        }
    };
    let per_uid = match sys::max_processes_per_uid().map(u32::try_from) {
        Ok(Ok(n)) => n,
        Ok(Err(_)) => u32::MAX,
        Err(e) => {
            let _ = writeln!(
                std::io::stderr().lock(),
                "hex __box-helper: reading kern.maxprocperuid: {e}"
            );
            return 1;
        }
    };
    let ops = match RealPriv::new() {
        Ok(ops) => ops,
        Err(e) => {
            let _ = writeln!(
                std::io::stderr().lock(),
                "hex __box-helper: making the exception port: {e}"
            );
            return 1;
        }
    };
    let daemon = match directory {
        None => Daemon::new(ops, users, per_uid),
        Some(directory) => match Daemon::growing(ops.with_directory(directory), per_uid) {
            Ok(daemon) => daemon,
            Err(e) => {
                let _ = writeln!(std::io::stderr().lock(), "hex __box-helper: {e}");
                return 1;
            }
        },
    };
    let daemon = std::sync::Arc::new(daemon);
    // Re-apportion on each member crossing (the ledger) and on each member's fork or end, each on a thread
    // beside the socket that wakes on its event alone.
    let crossings = std::sync::Arc::clone(&daemon);
    std::thread::spawn(move || {
        let why = loop {
            if let Err(e) = crossings.pump_crossings() {
                break e;
            }
        };
        let _ = writeln!(
            std::io::stderr().lock(),
            "hex __box-helper: the crossing pump stopped: {why}"
        );
    });
    let members = std::sync::Arc::clone(&daemon);
    std::thread::spawn(move || {
        let why = loop {
            if let Err(e) = members.pump_members() {
                break e;
            }
        };
        let _ = writeln!(
            std::io::stderr().lock(),
            "hex __box-helper: the member pump stopped: {why}"
        );
    });
    match daemon.listen(Path::new(&socket)) {
        Ok(()) => 0,
        Err(e) => {
            let _ = writeln!(std::io::stderr().lock(), "hex __box-helper: {e}");
            1
        }
    }
}
