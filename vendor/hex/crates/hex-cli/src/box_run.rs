//! `hex box run <limits> -- program args…`: runs a program in a `process` box (a cgroup v2 resource
//! bound), streaming the program's stdio through unchanged, and exits with the program's status.
//! This is the primitive kiln wraps a CI host leg or a harness process in, so no build or agent can
//! starve the machine (CP-T7.1, AC-T7.1.1/AC-T7.1.2).
//!
//! It inherits its own stdio and environment to the program, so a caller (kiln's runner) that
//! captures the `hex` process's output captures the program's transparently. On a normal exit it
//! removes the box; it ignores SIGINT/SIGTERM so that when its caller signals the whole process
//! group, the boxed program receives them and ends while this wrapper survives to clean up.
//!
//! It moves itself into a `.hexbox-run` leaf of the cgroup it starts in, and stays a member of it until the
//! kernel's last switch away from it, after its caller has reaped it (Linux v6.19 kernel/exit.c:983, then
//! cgroup_task_dead from finish_task_switch, kernel/sched/core.c:5156). A caller that removes that cgroup
//! once this has exited waits for its `cgroup.events` to read `populated 0` first, as box_bench.py's
//! remove_tree does; an rmdir before then is refused with EBUSY.

use std::process::Command;
use std::time::{Duration, Instant};

use hex_api::Proc;
use hex_api::{Bounds, BoxId, IoRates, Refusal};
use hex_core::patience::{Patience, Piece};
use hex_runtime::linux::cgroup::{self, BoxCgroup, BoxRoot};
use hex_runtime::linux::{disks, proc, process, sys};

/// Runs `hex box run`'s arguments (everything after `box run`); returns the process exit code.
pub fn main(args: &[String]) -> u8 {
    match go(args) {
        Ok(code) => code,
        Err(e) => {
            let _ = std::io::Write::write_all(
                &mut std::io::stderr().lock(),
                format!("hex box run: {e}\n").as_bytes(),
            );
            // 125: the box couldn't be set up (distinct from the program's own status), after docker's convention.
            125
        }
    }
}

struct Parsed {
    bounds: Bounds,
    cwd: Option<String>,
    id: Option<String>,
    program: Vec<String>,
}

fn go(args: &[String]) -> Result<u8, Refusal> {
    let p = parse(args)?;
    let (first, rest) = p.program.split_first().ok_or(Refusal::Invalid {
        what: "box run",
        why: "no program after `--`".to_owned(),
    })?;

    // The io bounds target the disk the program works on; resolve from its working folder.
    let probe = p.cwd.clone().unwrap_or_else(|| ".".to_owned());
    let disks = disks::under(std::path::Path::new(&probe))?;
    let root = box_root(disks)?;
    let id = BoxId::new(&p.id.clone().unwrap_or_else(default_id))?;
    let cg = root.create(&id, &p.bounds)?;

    let mut cmd = Command::new(first);
    cmd.args(rest);
    if let Some(cwd) = &p.cwd {
        cmd.current_dir(cwd);
    }
    // stdio and environment are inherited (not cleared): this is a transparent wrapper.
    process::join_on_exec(&cg, &mut cmd)?;
    let spawned = cmd.spawn();
    // The child is forked before SIGINT/SIGTERM are ignored, so it keeps the default disposition
    // (it ends on them); this wrapper ignores them from here on to outlive its caller's group signal.
    sys::ignore_int_term();
    let child = match spawned {
        Ok(c) => c,
        Err(e) => {
            let _ = cg.remove();
            return Err(Refusal::Os {
                op: "starting the program in a box",
                why: format!("{first}: {e}"),
            });
        }
    };
    let mut watched = proc::PidfdProc::of_child(child.id())?;
    // Followed to its end by its pidfd (`Duration::MAX`: until it), with no deadline: the program runs as long as it
    // needs, and its caller ends it by signal (which reaches the boxed child). It was an hour's slice, picked
    // (decision 9; ROOT, 2026-11-14).
    let exit = loop {
        if let Some(e) = watched.wait_for(Duration::MAX)? {
            break e;
        }
    };

    // Clean the box up: end any strays, follow it to its emptying, remove it. Strays a kill didn't reach are none of
    // this wrapper's to wait on, so the remove refuses then, as it did.
    let killed = Instant::now();
    if cg.kill().is_ok() {
        let _ = emptied(&cg, killed);
    }
    let _ = cg.remove();

    Ok(exit_code(exit))
}

/// Opens the box root this wrapper makes boxes under. cgroup v2's no-internal-process rule means
/// the wrapper's own cgroup can't both hold this process and parent the box, so the wrapper moves
/// itself into a `.hexbox-run` leaf and makes boxes under a sibling `.hexbox`. Both sit under the
/// wrapper's own cgroup, which must be delegated (its parent hands it cpu/io/memory/pids) — on a
/// kiln runner that means the runner's unit sets `Delegate=yes`; the refusal says so when it isn't.
fn box_root(disks: Vec<hex_core::cgroup::Dev>) -> Result<BoxRoot, Refusal> {
    let own = cgroup::own_cgroup()?;
    let leaf = own.join(".hexbox-run");
    std::fs::create_dir_all(&leaf)
        .map_err(|e| Refusal::os("making the wrapper's cgroup leaf", e))?;
    std::fs::write(leaf.join("cgroup.procs"), format!("{}", std::process::id()))
        .map_err(|e| Refusal::os("moving the wrapper into its cgroup leaf", e))?;
    BoxRoot::open(&own.join(".hexbox"), disks)
}

/// The box's emptying once its strays are killed (decision 9; the review's (c), 2026-11-14): within W*, the slowest
/// emptying this wrapper has seen; past it, and before the first, its run-out told and the box followed by its life,
/// its `cgroup.events`, to its emptying, so it is removed empty, never given up at a time. It was 10 s, the oci
/// runtime's delete patience copied, past which the remove was refused and the box left.
static CLEANUP: Patience = Patience::measuring("boxrun.emptying");

/// Waits for `cg`, killed at `killed`, to empty, as [`CLEANUP`] waits: whether it emptied.
fn emptied(cg: &BoxCgroup, killed: Instant) -> Result<bool, Refusal> {
    if let Some(window) = CLEANUP.deadline() {
        if cg.wait_empty(window.saturating_sub(killed.elapsed()))? {
            CLEANUP.answered(killed.elapsed());
            return Ok(true);
        }
        CLEANUP.ran_out(
            window,
            killed.elapsed(),
            &[
                Piece::Kind("box cgroup "),
                Piece::Path(cg.dir()),
                Piece::Kind(", by its cgroup.events"),
            ],
        );
    }
    let came = cg.wait_empty(Duration::MAX)?;
    if came {
        CLEANUP.answered(killed.elapsed());
    }
    Ok(came)
}

fn exit_code(exit: hex_api::Exit) -> u8 {
    match exit {
        hex_api::Exit::Code(c) => u8::try_from(c).unwrap_or(1),
        // The shell convention: a signal n becomes 128 + n.
        hex_api::Exit::Signal(s) => u8::try_from(128i32.saturating_add(s)).unwrap_or(1),
        hex_api::Exit::Unknown => 1,
    }
}

fn default_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("boxrun-{}-{nanos}", std::process::id())
}

/// Parses the flags and the `--`-separated program. Unknown flags are refused (never silently
/// dropped); a flag out of range is refused by [`Bounds::check`] when the box is made.
fn parse(args: &[String]) -> Result<Parsed, Refusal> {
    let mut bounds = Bounds::default();
    let mut cwd = None;
    let mut id = None;
    let mut io = IoRates::default();
    let mut program = Vec::new();
    let mut i = 0;
    let num = |v: &Option<&String>, what: &'static str| -> Result<u64, Refusal> {
        v.and_then(|s| s.parse().ok()).ok_or(Refusal::Invalid {
            what,
            why: "a number is needed".to_owned(),
        })
    };
    while let Some(arg) = args.get(i) {
        if arg == "--" {
            program = args.get(i.saturating_add(1)..).unwrap_or_default().to_vec();
            break;
        }
        let next = args.get(i.saturating_add(1));
        match arg.as_str() {
            "--memory-max" => bounds.memory_max = Some(num(&next, "--memory-max")?),
            "--memory-high" => bounds.memory_high = Some(num(&next, "--memory-high")?),
            "--cpu-weight" => {
                bounds.cpu_weight =
                    Some(u16::try_from(num(&next, "--cpu-weight")?).unwrap_or(u16::MAX))
            }
            "--cpu-max-milli" => {
                bounds.cpu_max_milli =
                    Some(u32::try_from(num(&next, "--cpu-max-milli")?).unwrap_or(u32::MAX))
            }
            "--io-weight" => {
                bounds.io_weight =
                    Some(u16::try_from(num(&next, "--io-weight")?).unwrap_or(u16::MAX))
            }
            "--io-latency-us" => {
                bounds.io_latency_us =
                    Some(u32::try_from(num(&next, "--io-latency-us")?).unwrap_or(u32::MAX))
            }
            "--io-read-bps" => io.read_bps = Some(num(&next, "--io-read-bps")?),
            "--io-write-bps" => io.write_bps = Some(num(&next, "--io-write-bps")?),
            "--pids-max" => {
                bounds.pids_max = Some(u32::try_from(num(&next, "--pids-max")?).unwrap_or(u32::MAX))
            }
            "--cwd" => {
                cwd = Some(next.cloned().ok_or(Refusal::Invalid {
                    what: "--cwd",
                    why: "a path is needed".to_owned(),
                })?)
            }
            "--id" => {
                id = Some(next.cloned().ok_or(Refusal::Invalid {
                    what: "--id",
                    why: "an id is needed".to_owned(),
                })?)
            }
            other => {
                return Err(Refusal::Invalid {
                    what: "box run flag",
                    why: format!("unknown flag {other:?}"),
                });
            }
        }
        // Every recognised flag takes one value.
        i = i.saturating_add(2);
    }
    if io != IoRates::default() {
        bounds.io_max = Some(io);
    }
    Ok(Parsed {
        bounds,
        cwd,
        id,
        program,
    })
}
