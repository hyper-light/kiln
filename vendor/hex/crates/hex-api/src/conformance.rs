//! The [`crate::Runtime`] contract as cases every implementation passes: the simulator in this
//! crate's tests, each real class in its own (CLAUDE.md §9). A [`Fixture`] says how to make a box of
//! the class under test and supplies programs by what they do, so each implementation brings its
//! own programs. Each case reads "do X, expect Y"; [`run`] returns the cases that failed and why.

use std::io::{Read, Write};
use std::net::IpAddr;
use std::time::Duration;

use crate::patience::{Party, Patience};
use crate::{
    Bounds, BoxId, BoxOwner, BoxSpec, BoxState, Class, ClassSpec, CredentialGrant, Destination,
    DeviceId, DeviceKind, Exit, Grant, GuestTime, Member, MemberKind, Mount, Network, OciSpec,
    Ports, Program, Proto, Refusal, Rootfs, Runtime, Signal, Stdio, Upstream,
};

/// What a fixture's program does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Act {
    /// Writes these words and a newline to stdout, then exits 0.
    Say(&'static str),
    /// Exits with this status.
    Fail(i32),
    /// Runs until it is signalled.
    Wait,
    /// Uses this many bytes of memory, then exits 0.
    Hog(u64),
    /// Starts this many processes at once, then exits 0; exits non-zero when one can't start.
    Spawn(u32),
    /// Copies stdin to stdout until stdin closes, then exits 0.
    Echo,
}

/// How to drive the implementation under test.
pub trait Fixture {
    /// A box of the class under test, with `bounds`.
    fn spec(&self, id: BoxId, bounds: Bounds) -> BoxSpec;
    /// An `oci` box whose init does `act` once started; for the `process` class, a box as
    /// [`Fixture::spec`] makes (it has no init, and the cases that use this check it reports none).
    fn init(&self, id: BoxId, act: &Act) -> BoxSpec;
    /// A program that does `act`.
    fn program(&self, act: &Act) -> Program;
    /// Whether this class reports a fork its process bound refused in [`crate::Usage::pids_refused`]
    /// reliably. True for every class whose kernel keeps that count (Linux's cgroup `pids.events`, the
    /// simulator). The Windows job object does not: a member's own refused fork has no §3-clean job-side
    /// count (`docs/research/runtime/windows-pids-refused.md`), so its class returns false and HEX-RT-6
    /// checks the bound's deterministic effect there (a program that can't start all its children exits
    /// non-zero) rather than the count. The bound's enforcement is checked on every platform regardless.
    /// (Ada's decision, 2026-11-03.)
    fn refused_forks_counted(&self) -> bool {
        true
    }
    /// The bound this class's exec puts on a program's arguments and environment, read as the class reads
    /// it (CLAUDE.md §2). `None` states none, and HEX-RT-14 fails on it: every class bounds a program.
    fn exec_limit(&self) -> Option<crate::ExecLimit> {
        None
    }
    /// Whether TERM ends an `oci` box's init that has no handler for it. False for Linux: the kernel
    /// gives a PID namespace's init only KILL and STOP from outside it (pid_namespaces(7)). True for a
    /// class with no POSIX signals, which ends the container instead: HCS's ProcessSignal carries only
    /// console control events, and hcsshim maps TERM to a shutdown and KILL to terminate
    /// (internal/signals/signal.go:185-197), so Windows' `oci` class ends its job. Both are right for their platform, so the differential suite's oracle follows this answer
    /// (the lead's ruling, 2026-11-05). The `process` class has no init, so it doesn't apply there.
    fn term_ends_init(&self) -> bool {
        false
    }
    /// The life that can still change the runtime's answers, as the test sees it (decision 9): the runtime under test,
    /// its node. A case's or a scenario's wait follows it past W*, and fails at its end. Alive by default: an
    /// in-process runtime, and a node its test keeps, live as long as the test.
    fn party(&self) -> Party {
        Party::Live
    }
    /// A wait past W* tells this once: what it waits on, what it wants and what the runtime last showed, for the test's
    /// output. Where a runtime that differs stays alive, the wait goes on, and this is what the person sees of it.
    fn told(&self, note: &str) {
        let _ = note;
    }
}

/// One part of the contract.
pub struct Case {
    pub id: &'static str,
    pub says: &'static str,
    run: fn(&dyn Runtime, &dyn Fixture) -> Result<(), String>,
}

/// A case that failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub case: &'static str,
    pub why: String,
}

/// The suite's waits for a program's end or a box's answer (decision 9, and the rule at a local site of
/// docs/research/runtime/learned-waits.md): W*, the slowest end this process has seen, a bound only while ends come
/// within it. Past it the run-out is told, and the wait follows its party, the program or the box, by its life alone,
/// until the end comes. Before the first end, and from a run-out to the next, it follows its party from the start. No
/// record seeds it: the per-class end records it was seeded from until 2026-11-13 are evidence alone.
pub static ENDS: Patience = Patience::measuring("conformance.ends");

/// Program `p`'s end, waited for as [`ENDS`] waits: within W*, and past it, its run-out told, by the program's life
/// alone, on its own events (`Proc::wait_for` until its end), however long that takes. The end, however long it took,
/// is the sample W* rises to. Refused only as the runtime refuses the wait.
pub fn end_of<P: crate::Proc + ?Sized>(p: &mut P) -> Result<Exit, Refusal> {
    let asked = std::time::Instant::now();
    if let Some(within) = ENDS.deadline() {
        if let Some(exit) = p.wait_for(within)? {
            ENDS.answered(asked.elapsed());
            return Ok(exit);
        }
        // Past W*: no sample (Karn's point), but the census is told, once, and the wait goes on.
        ENDS.ran_out(within);
    }
    loop {
        // `Duration::MAX`: until its end, each runtime's wait taking it as "no deadline".
        if let Some(exit) = p.wait_for(Duration::MAX)? {
            ENDS.answered(asked.elapsed());
            return Ok(exit);
        }
    }
}

/// How long a case watches a program it expects to keep running (the negative watch, "this does not end"): the
/// slowest end this process has seen, since an end may come as late as that; `None` before any end, when there is
/// nothing to watch against (the suite runs its ending cases first).
pub fn still_running() -> Option<Duration> {
    ENDS.slowest()
}

/// Every case, in order.
pub const CASES: &[Case] = &[
    Case {
        id: "HEX-RT-1",
        says: "create makes a box that list and inspect show empty; a second create of its id is refused as Exists",
        run: create_and_exists,
    },
    Case {
        id: "HEX-RT-2",
        says: "exec of a program that says words: they reach its stdout and it exits 0",
        run: say,
    },
    Case {
        id: "HEX-RT-3",
        says: "exec of a program that fails with 3 ends Code(3)",
        run: fail,
    },
    Case {
        id: "HEX-RT-4",
        says: "a waiting program keeps running and the box shows Running; signal TERM ends it with Signal(15)",
        run: signal,
    },
    Case {
        id: "HEX-RT-5",
        says: "a box bounded to 64 MiB: a program using 512 MiB is killed and the kill counted",
        run: memory_bound,
    },
    Case {
        id: "HEX-RT-6",
        says: "a box bounded to 8 processes: a program starting 32 exits non-zero and the refused fork is counted",
        run: pids_bound,
    },
    Case {
        id: "HEX-RT-7",
        says: "delete ends what runs in the box and removes it; then every call on it is NoSuchBox",
        run: delete,
    },
    Case {
        id: "HEX-RT-8",
        says: "create with an invalid bound is refused as Invalid and makes nothing",
        run: invalid_bounds,
    },
    Case {
        id: "HEX-RT-9",
        says: "stdin reaches the program: what is written comes back on stdout",
        run: stdin,
    },
    Case {
        id: "HEX-RT-10",
        says: "an oci box is Created until start and Running after it, and a second start is refused; a process box's start is refused",
        run: start,
    },
    Case {
        id: "HEX-RT-11",
        says: "an oci box whose init exits with 3 is Empty once it has ended, and inspect reports Code(3); a process box reports no exit",
        run: init_exit,
    },
    Case {
        id: "HEX-RT-12",
        says: "an oci box whose init waits reports no exit while it runs; signal KILL ends it, and inspect reports Signal(9)",
        run: init_killed,
    },
    Case {
        id: "HEX-RT-13",
        says: "create of an oci box whose spec names a value that can't be rendered is refused as Invalid naming the field, and makes nothing",
        run: unrenderable,
    },
    Case {
        id: "HEX-RT-14",
        says: "exec of a program whose arguments and environment pass the class's exec bound is refused as Invalid naming the program, before any spawn",
        run: oversized_program,
    },
    Case {
        id: "HEX-RT-15",
        says: "run of a program in a process box: its words reach stdout, it ends Code(0) and the box is gone; a running run's box is Running and its id taken; let go of, its box is removed once its program ends, with what it left there; an oci spec's run is refused as Invalid, and a class that runs none refuses it naming its platform, each making nothing",
        run: run_removes,
    },
    Case {
        id: "HEX-RT-16",
        says: "of two waiting programs in a box, the one killed through its Proc ends with Signal(9) and the other runs on; a second kill of the ended one is Ok",
        run: kill,
    },
    Case {
        id: "HEX-RT-17",
        says: "stdio asked of the runtime: a program that echoes stdin gives back what is written to its stdin end on its stdout end, to its end; a stream both given and asked for is refused as Invalid",
        run: streams,
    },
    Case {
        id: "HEX-RT-18",
        says: "create of a box whose spec names a malformed owner is refused as Invalid naming the field, and makes nothing; boxes of one owner, made and deleted in turn, each run what any box runs",
        run: owned,
    },
];

/// Runs every case on fresh boxes; returns the failures.
pub fn run(rt: &dyn Runtime, fx: &dyn Fixture) -> Vec<Failure> {
    run_cases(CASES, rt, fx)
}

/// The Shards image class's own cases (joint plan step 12, Shards' D231), on a fixture whose boxes are
/// Shards image VMs ([`ClassSpec::ShardsImage`]): [`Fixture::spec`] a VM running its image's members
/// alone, [`Fixture::init`] one whose run's command does the act, [`Fixture::program`] a member's
/// program. Its members are reached through Shards' agent-control endpoint, so what needs that is
/// refused naming it, never stubbed.
pub const IMAGE_CASES: &[Case] = &[
    Case {
        id: "HEX-IMG-1",
        says: "create makes a Shards image VM that list and inspect show Created, of class Shards, with no exit, and a second create of its id is Exists; start boots it, Running, and a second start is refused as Invalid naming start; delete removes it, and every call on it is then NoSuchBox",
        run: image_lifecycle,
    },
    Case {
        id: "HEX-IMG-2",
        says: "stop, a signal KILL, ends a running VM: inspect reports it Empty with Signal(9), a second signal is NotRunning, and it stays until delete",
        run: image_stop,
    },
    Case {
        id: "HEX-IMG-3",
        says: "a VM whose run's command exits 3 is Empty once it has ended, and inspect reports Code(3)",
        run: image_command_exit,
    },
    Case {
        id: "HEX-IMG-4",
        says: "usage of a running VM reports its guest time per vCPU: one entry for each vCPU its allocation sizes, each no smaller at a second read, none past the wall time since its start; before its start and after its end usage is NotRunning",
        run: image_guest_time,
    },
    Case {
        id: "HEX-IMG-5",
        says: "a program exec'd in a running VM is refused Awaits(MemberExec), and a signal but KILL Awaits(MemberSignal); the VM runs on",
        run: image_members,
    },
    Case {
        id: "HEX-IMG-6",
        says: "create of a VM whose image isn't pinned by a digest, whose run's command is empty, with no allocation or naming a device is refused as Invalid naming the field, and makes nothing",
        run: image_unbootable,
    },
    Case {
        id: "HEX-IMG-7",
        says: "create of a VM granted egress by UDP to a name is refused Awaits(MemberGrantsByName), to an IPv6 address Awaits(MemberGrantsByAddress), its host's service by UDP Awaits(MemberHostService); egress to host.docker.internal, its host's Shards name, or a malformed grant Invalid naming grants; each making nothing. A VM granted egress by TCP to a name or an IPv4 address, its host's service by TCP, or a publish of its member's port at the host's loopback, of its members alone and with a command of its own, is made, and inspect shows its every grant",
        run: image_grants,
    },
    Case {
        id: "HEX-IMG-8",
        says: "create of a VM granting its member a credential toward a host no egress grant of its own reaches by name over TCP, toward no host, by a reference no store names, twice, a second credential toward one host, or toward a host another member reaches ungranted, is refused Invalid naming credentials, making nothing; a VM granting a credential toward its member's own egress hosts is made, and inspect shows its every credential grant, or, where its node leases no credentials, it is refused Unconfigured, making nothing",
        run: image_credentials,
    },
];

/// Runs every Shards image case on fresh boxes; returns the failures.
pub fn run_image(rt: &dyn Runtime, fx: &dyn Fixture) -> Vec<Failure> {
    run_cases(IMAGE_CASES, rt, fx)
}

fn run_cases(cases: &[Case], rt: &dyn Runtime, fx: &dyn Fixture) -> Vec<Failure> {
    cases
        .iter()
        .filter_map(|c| (c.run)(rt, fx).err().map(|why| Failure { case: c.id, why }))
        .collect()
}

fn id(name: &str) -> Result<BoxId, String> {
    BoxId::new(&format!("hex-conformance-{name}")).map_err(|e| e.to_string())
}

fn ensure(ok: bool, why: impl FnOnce() -> String) -> Result<(), String> {
    if ok { Ok(()) } else { Err(why()) }
}

/// A fresh box of the fixture's class, starting every class whose workload has an init.
fn made(rt: &dyn Runtime, fx: &dyn Fixture, name: &str, bounds: Bounds) -> Result<BoxId, String> {
    let id = id(name)?;
    // A box left by an earlier run of the suite is removed first; absent is the usual case.
    let _ = rt.delete(&id);
    let spec = fx.spec(id.clone(), bounds);
    let class = spec.class.class();
    rt.create(spec).map_err(|e| format!("create: {e}"))?;
    if class != Class::Process {
        rt.start(&id).map_err(|e| format!("start: {e}"))?;
    }
    Ok(id)
}

fn ended(p: &mut Box<dyn crate::Proc>) -> Result<Exit, String> {
    end_of(p).map_err(|e| format!("wait: {e}"))
}

fn piped() -> Result<(std::io::PipeReader, std::io::PipeWriter), String> {
    std::io::pipe().map_err(|e| format!("pipe: {e}"))
}

fn create_and_exists(rt: &dyn Runtime, fx: &dyn Fixture) -> Result<(), String> {
    let id = made(rt, fx, "exists", Bounds::default())?;
    let listed = rt.list().map_err(|e| e.to_string())?;
    ensure(listed.iter().any(|b| b.id == id), || {
        format!("{id} isn't in {listed:?}")
    })?;
    let info = rt.inspect(&id).map_err(|e| e.to_string())?;
    let class = fx.spec(id.clone(), Bounds::default()).class.class();
    ensure(info.class == class, || {
        format!("inspect says class {:?}", info.class)
    })?;
    // A process box is empty until exec; an oci box's init runs from its start on (HEX-RT-10).
    if class == Class::Process {
        ensure(info.state == BoxState::Empty, || {
            format!("a new box is {:?}", info.state)
        })?;
    }
    let again = rt.create(fx.spec(id.clone(), Bounds::default()));
    ensure(again == Err(Refusal::Exists(id.clone())), || {
        format!("second create: {again:?}")
    })?;
    rt.delete(&id).map_err(|e| e.to_string())
}

fn say(rt: &dyn Runtime, fx: &dyn Fixture) -> Result<(), String> {
    let id = made(rt, fx, "say", Bounds::default())?;
    let (mut r, w) = piped()?;
    let mut p = rt
        .exec(
            &id,
            fx.program(&Act::Say("hello from a box")),
            Stdio {
                stdout: Some(w),
                ..Stdio::null()
            },
        )
        .map_err(|e| format!("exec: {e}"))?;
    let exit = ended(&mut p)?;
    let mut out = String::new();
    r.read_to_string(&mut out)
        .map_err(|e| format!("read: {e}"))?;
    rt.delete(&id).map_err(|e| e.to_string())?;
    ensure(exit == Exit::Code(0), || format!("exit {exit:?}"))?;
    ensure(out == "hello from a box\n", || format!("stdout {out:?}"))
}

fn oversized_program(rt: &dyn Runtime, fx: &dyn Fixture) -> Result<(), String> {
    let limit = fx.exec_limit().ok_or(
        "the class states no bound on a program's arguments and environment (CLAUDE.md §2)",
    )?;
    let id = made(rt, fx, "oversized", Bounds::default())?;
    let mut program = fx.program(&Act::Say("never"));
    // Strings each under the OS's per-string bound (half of it, or of the block where there is none), as
    // many as pass the block: only passing it matters, and any split under the string bound does. Each
    // grows the program by at least a byte, so the loop ends.
    let piece = limit
        .string
        .unwrap_or(limit.block)
        .min(limit.block)
        .div_ceil(2)
        .max(1);
    let mut n = 0usize;
    while limit.size(&program) <= limit.block {
        program
            .env
            .push((format!("HEX_RT_14_{n}"), "x".repeat(piece)));
        n = n.saturating_add(1);
    }
    let refused = rt.exec(&id, program, Stdio::null());
    rt.delete(&id).map_err(|e| e.to_string())?;
    match refused {
        Err(Refusal::Invalid {
            what: "program", ..
        }) => Ok(()),
        Err(e) => Err(format!("refused, but not as an invalid program: {e}")),
        Ok(_) => Err(format!(
            "a program past the class's bound ({} bytes, {}) was taken",
            limit.block, limit.source
        )),
    }
}

fn fail(rt: &dyn Runtime, fx: &dyn Fixture) -> Result<(), String> {
    let id = made(rt, fx, "fail", Bounds::default())?;
    let mut p = rt
        .exec(&id, fx.program(&Act::Fail(3)), Stdio::null())
        .map_err(|e| format!("exec: {e}"))?;
    let exit = ended(&mut p)?;
    rt.delete(&id).map_err(|e| e.to_string())?;
    ensure(exit == Exit::Code(3), || format!("exit {exit:?}"))
}

fn signal(rt: &dyn Runtime, fx: &dyn Fixture) -> Result<(), String> {
    let id = made(rt, fx, "signal", Bounds::default())?;
    let mut p = rt
        .exec(&id, fx.program(&Act::Wait), Stdio::null())
        .map_err(|e| format!("exec: {e}"))?;
    // Running now, at the signal: its Signal(15) end below then proves it hadn't ended on its own (that
    // would be Code(0)), so no watch is waited out.
    let early = p.wait_for(Duration::ZERO).map_err(|e| e.to_string())?;
    ensure(early.is_none(), || {
        format!("a waiting program ended: {early:?}")
    })?;
    let state = rt.inspect(&id).map_err(|e| e.to_string())?.state;
    ensure(state == BoxState::Running, || {
        format!("box {state:?} while a program waits")
    })?;
    rt.signal(&id, Signal::TERM)
        .map_err(|e| format!("signal: {e}"))?;
    let exit = ended(&mut p)?;
    rt.delete(&id).map_err(|e| e.to_string())?;
    ensure(exit == Exit::Signal(15), || format!("exit {exit:?}"))
}

fn kill(rt: &dyn Runtime, fx: &dyn Fixture) -> Result<(), String> {
    let id = made(rt, fx, "kill", Bounds::default())?;
    let mut killed = rt
        .exec(&id, fx.program(&Act::Wait), Stdio::null())
        .map_err(|e| format!("exec: {e}"))?;
    let mut other = rt
        .exec(&id, fx.program(&Act::Wait), Stdio::null())
        .map_err(|e| format!("exec: {e}"))?;
    let watch =
        still_running().ok_or("no program's end seen yet to watch a waiting one against")?;
    let early = killed.wait_for(watch).map_err(|e| e.to_string())?;
    ensure(early.is_none(), || {
        format!("a waiting program ended: {early:?}")
    })?;
    killed.kill().map_err(|e| format!("kill: {e}"))?;
    let exit = ended(&mut killed)?;
    let again = killed.kill();
    let on = other.wait_for(watch).map_err(|e| e.to_string())?;
    rt.delete(&id).map_err(|e| e.to_string())?;
    ensure(exit == Exit::Signal(9), || format!("exit {exit:?}"))?;
    ensure(again.is_ok(), || {
        format!("a kill of the ended program: {again:?}")
    })?;
    ensure(on.is_none(), || {
        format!("the other program ended with the kill: {on:?}")
    })
}

fn streams(rt: &dyn Runtime, fx: &dyn Fixture) -> Result<(), String> {
    let id = made(rt, fx, "streams", Bounds::default())?;
    let (_r, w) = piped()?;
    let both = rt.exec(
        &id,
        fx.program(&Act::Echo),
        Stdio {
            stdout: Some(w),
            ask: crate::Ask {
                stdout: true,
                ..crate::Ask::default()
            },
            ..Stdio::null()
        },
    );
    let ask = crate::Ask {
        stdin: true,
        stdout: true,
        stderr: false,
    };
    let mut p = rt
        .exec(
            &id,
            fx.program(&Act::Echo),
            Stdio {
                ask,
                ..Stdio::null()
            },
        )
        .map_err(|e| format!("exec: {e}"))?;
    let given = p.streams().map_err(|e| format!("streams: {e}"))?;
    let again = p.streams().map_err(|e| format!("streams again: {e}"))?;
    let (Some(stdin), Some(stdout)) = (given.stdin, given.stdout) else {
        rt.delete(&id).map_err(|e| e.to_string())?;
        return Err("the streams asked for weren't given".to_owned());
    };
    let mut stdin = crate::OwnedStream::from(stdin);
    stdin
        .write_all(b"round trip\n")
        .map_err(|e| format!("write: {e}"))?;
    drop(stdin);
    let mut out = String::new();
    crate::OwnedStream::from(stdout)
        .read_to_string(&mut out)
        .map_err(|e| format!("read: {e}"))?;
    let exit = ended(&mut p)?;
    rt.delete(&id).map_err(|e| e.to_string())?;
    ensure(
        matches!(
            both.as_ref().err(),
            Some(Refusal::Invalid { what: "stdio", .. })
        ),
        || {
            format!(
                "a stream both given and asked for: {:?}",
                both.as_ref().err()
            )
        },
    )?;
    ensure(given.stderr.is_none(), || "stderr given unasked".to_owned())?;
    ensure(again.stdin.is_none() && again.stdout.is_none(), || {
        "the streams were given twice".to_owned()
    })?;
    ensure(exit == Exit::Code(0), || format!("exit {exit:?}"))?;
    ensure(out == "round trip\n", || format!("stdout {out:?}"))
}

fn memory_bound(rt: &dyn Runtime, fx: &dyn Fixture) -> Result<(), String> {
    let bounds = Bounds {
        memory_max: Some(64 << 20),
        ..Bounds::default()
    };
    let id = made(rt, fx, "memory", bounds)?;
    let mut p = rt
        .exec(&id, fx.program(&Act::Hog(512 << 20)), Stdio::null())
        .map_err(|e| format!("exec: {e}"))?;
    let exit = ended(&mut p)?;
    let usage = rt.usage(&id).map_err(|e| e.to_string())?;
    rt.delete(&id).map_err(|e| e.to_string())?;
    ensure(exit != Exit::Code(0), || {
        "a program over the memory bound exited 0".to_owned()
    })?;
    ensure(usage.oom_kills >= 1, || {
        format!("no kill counted: {usage:?}")
    })
}

fn pids_bound(rt: &dyn Runtime, fx: &dyn Fixture) -> Result<(), String> {
    let bounds = Bounds {
        pids_max: Some(8),
        ..Bounds::default()
    };
    let id = made(rt, fx, "pids", bounds)?;
    let mut p = rt
        .exec(&id, fx.program(&Act::Spawn(32)), Stdio::null())
        .map_err(|e| format!("exec: {e}"))?;
    let exit = ended(&mut p)?;
    let usage = rt.usage(&id).map_err(|e| e.to_string())?;
    rt.delete(&id).map_err(|e| e.to_string())?;
    // The bound blocked a fork the program needed, so it couldn't finish: it exits non-zero. This
    // deterministic effect is checked on every platform (the process bound is kernel-enforced).
    ensure(exit != Exit::Code(0), || {
        "a program over the process bound exited 0".to_owned()
    })?;
    // Where the platform counts the refusal, the exact count is checked too; where it has no §3-clean
    // count of a member's own refused fork (Windows), the effect above is the bar (Ada's call).
    if fx.refused_forks_counted() {
        ensure(usage.pids_refused >= 1, || {
            format!("no refused fork counted: {usage:?}")
        })?;
    }
    Ok(())
}

fn delete(rt: &dyn Runtime, fx: &dyn Fixture) -> Result<(), String> {
    let id = made(rt, fx, "delete", Bounds::default())?;
    let mut p = rt
        .exec(&id, fx.program(&Act::Wait), Stdio::null())
        .map_err(|e| format!("exec: {e}"))?;
    rt.delete(&id).map_err(|e| format!("delete: {e}"))?;
    let exit = ended(&mut p)?;
    ensure(matches!(exit, Exit::Signal(_)), || {
        format!("deleted box's program: {exit:?}")
    })?;
    let gone = Refusal::NoSuchBox(id.clone());
    let calls = [
        ("inspect", rt.inspect(&id).err()),
        ("usage", rt.usage(&id).err()),
        ("signal", rt.signal(&id, Signal::KILL).err()),
        ("start", rt.start(&id).err()),
        (
            "exec",
            rt.exec(&id, fx.program(&Act::Wait), Stdio::null()).err(),
        ),
        ("delete", rt.delete(&id).err()),
    ];
    for (call, got) in calls {
        ensure(got.as_ref() == Some(&gone), || {
            format!("{call} after delete: {got:?}")
        })?;
    }
    let listed = rt.list().map_err(|e| e.to_string())?;
    ensure(!listed.iter().any(|b| b.id == id), || {
        format!("{id} still listed")
    })
}

fn invalid_bounds(rt: &dyn Runtime, fx: &dyn Fixture) -> Result<(), String> {
    let id = id("invalid")?;
    let _ = rt.delete(&id);
    let bad = Bounds {
        cpu_weight: Some(0),
        ..Bounds::default()
    };
    let got = rt.create(fx.spec(id.clone(), bad));
    ensure(
        matches!(
            got,
            Err(Refusal::Invalid {
                what: "cpu_weight",
                ..
            })
        ),
        || format!("create: {got:?}"),
    )?;
    let got = rt.inspect(&id);
    ensure(got == Err(Refusal::NoSuchBox(id.clone())), || {
        format!("a refused create made {got:?}")
    })
}

fn stdin(rt: &dyn Runtime, fx: &dyn Fixture) -> Result<(), String> {
    let id = made(rt, fx, "stdin", Bounds::default())?;
    let (in_r, mut in_w) = piped()?;
    let (mut out_r, out_w) = piped()?;
    let mut p = rt
        .exec(
            &id,
            fx.program(&Act::Echo),
            Stdio {
                stdin: Some(in_r),
                stdout: Some(out_w),
                stderr: None,
                ..Stdio::null()
            },
        )
        .map_err(|e| format!("exec: {e}"))?;
    in_w.write_all(b"round trip\n")
        .map_err(|e| format!("write: {e}"))?;
    drop(in_w);
    let mut out = String::new();
    out_r
        .read_to_string(&mut out)
        .map_err(|e| format!("read: {e}"))?;
    let exit = ended(&mut p)?;
    rt.delete(&id).map_err(|e| e.to_string())?;
    ensure(exit == Exit::Code(0), || format!("exit {exit:?}"))?;
    ensure(out == "round trip\n", || format!("stdout {out:?}"))
}

fn start(rt: &dyn Runtime, fx: &dyn Fixture) -> Result<(), String> {
    let id = id("start")?;
    let _ = rt.delete(&id);
    let spec = fx.spec(id.clone(), Bounds::default());
    let class = spec.class.class();
    rt.create(spec).map_err(|e| format!("create: {e}"))?;
    let outcome = started(rt, &id, class);
    rt.delete(&id).map_err(|e| e.to_string())?;
    outcome
}

/// HEX-RT-10 on a box just made: what its start does, by class.
fn started(rt: &dyn Runtime, id: &BoxId, class: Class) -> Result<(), String> {
    let state = |rt: &dyn Runtime| rt.inspect(id).map(|i| i.state).map_err(|e| e.to_string());
    if class == Class::Process {
        let got = rt.start(id);
        return ensure(
            matches!(got, Err(Refusal::Invalid { what: "start", .. })),
            || format!("start of a process box: {got:?}"),
        );
    }
    let before = state(rt)?;
    ensure(before == BoxState::Created, || {
        format!("an oci box before its start is {before:?}")
    })?;
    rt.start(id).map_err(|e| format!("start: {e}"))?;
    let after = state(rt)?;
    ensure(after == BoxState::Running, || {
        format!("an oci box after its start is {after:?}")
    })?;
    let again = rt.start(id);
    ensure(
        matches!(again, Err(Refusal::Invalid { what: "start", .. })),
        || format!("a second start: {again:?}"),
    )
}

/// Waits `d` at least, on hex-wait's timer: on macOS std's `park_timeout` comes late by the waiting thread's QoS
/// leeway, at utility QoS up to four times `d` (docs/design/deadline-waits.md), and `thread::sleep` is disallowed.
pub(crate) fn rest(d: Duration) -> Result<(), String> {
    let Some(end) = std::time::Instant::now().checked_add(d) else {
        return Ok(());
    };
    hex_wait::rest_until(end).map_err(|why| format!("a rest of {d:?}: {why}"))
}

/// Reads with `look` until it gives its fact, as [`ENDS`] waits (decision 9): within W*, and past it, its run-out told
/// once, to the census and to the test ([`Fixture::told`], naming `what`), for as long as the runtime under test lives
/// ([`Fixture::party`]); at its end, once a last reading hasn't given the fact, refused with `last`'s words for the
/// reading that stood. A box's state has no event to wait on, so it is read again after waiting as long as the last
/// read took: at most half the time reading, at this platform's cost.
fn read_until<T, R: std::fmt::Debug>(
    fx: &dyn Fixture,
    what: &str,
    mut look: impl FnMut() -> Result<Result<T, R>, String>,
    last: impl Fn(&R) -> String,
) -> Result<T, String> {
    let asked = std::time::Instant::now();
    let bound = ENDS.deadline();
    let mut past = false;
    loop {
        let ended = fx.party() == Party::Ended;
        let read = std::time::Instant::now();
        let reading = look()?;
        let took = read.elapsed();
        let stood = match reading {
            Ok(fact) => {
                ENDS.answered(asked.elapsed());
                return Ok(fact);
            }
            Err(stood) => stood,
        };
        if ended {
            return Err(last(&stood));
        }
        if !past
            && let Some(within) = bound
            && asked.elapsed() >= within
        {
            // Past W*: no sample (Karn's point), the run-out told once, and the wait goes on while the runtime lives.
            ENDS.ran_out(within);
            fx.told(&format!(
                "{what}: past {within:?}, the runtime showing {stood:?}; following it"
            ));
            past = true;
        }
        rest(took)?;
    }
}

/// Reads `inspect` until the box reports an exit ([`read_until`]): the fact the exit cases wait on, never a guessed
/// delay.
fn exit_of(rt: &dyn Runtime, fx: &dyn Fixture, id: &BoxId) -> Result<crate::BoxInfo, String> {
    read_until(
        fx,
        &format!("an exit of {id}"),
        || {
            let info = rt.inspect(id).map_err(|e| format!("inspect: {e}"))?;
            Ok(if info.exit.is_some() {
                Ok(info)
            } else {
                Err(info)
            })
        },
        |info| format!("no exit reported, its runtime ended: {info:?}"),
    )
}

/// Reads `inspect` until box `id` is gone ([`read_until`]): the fact a run's removal is waited on, as [`exit_of`]
/// reads an exit.
fn gone_within(rt: &dyn Runtime, fx: &dyn Fixture, id: &BoxId) -> Result<(), String> {
    read_until(
        fx,
        &format!("{id} gone"),
        || match rt.inspect(id) {
            Err(Refusal::NoSuchBox(_)) => Ok(Ok(())),
            Ok(info) => Ok(Err(info)),
            Err(e) => Err(format!("inspect of a run's box being removed: {e}")),
        },
        |info| format!("a let-go run's box still there after its program ended: {info:?}"),
    )
}

/// A box whose init does `act`, made and started; `None` for a `process` box, after checking it
/// reports no exit.
fn made_init(
    rt: &dyn Runtime,
    fx: &dyn Fixture,
    name: &str,
    act: &Act,
) -> Result<Option<BoxId>, String> {
    let id = id(name)?;
    let _ = rt.delete(&id);
    let spec = fx.init(id.clone(), act);
    let class = spec.class.class();
    let info = rt.create(spec).map_err(|e| format!("create: {e}"))?;
    if class == Class::Process {
        let outcome = ensure(info.exit.is_none(), || {
            format!("a process box reports an exit: {info:?}")
        });
        rt.delete(&id).map_err(|e| e.to_string())?;
        return outcome.map(|()| None);
    }
    if let Err(e) = rt.start(&id) {
        let _ = rt.delete(&id);
        return Err(format!("start: {e}"));
    }
    Ok(Some(id))
}

fn init_exit(rt: &dyn Runtime, fx: &dyn Fixture) -> Result<(), String> {
    let Some(id) = made_init(rt, fx, "init-exit", &Act::Fail(3))? else {
        return Ok(());
    };
    let info = exit_of(rt, fx, &id);
    rt.delete(&id).map_err(|e| e.to_string())?;
    let info = info?;
    ensure(info.exit == Some(Exit::Code(3)), || {
        format!("exit {:?}", info.exit)
    })?;
    ensure(info.state == BoxState::Empty, || {
        format!("a box whose init ended is {:?}", info.state)
    })
}

fn init_killed(rt: &dyn Runtime, fx: &dyn Fixture) -> Result<(), String> {
    let Some(id) = made_init(rt, fx, "init-killed", &Act::Wait)? else {
        return Ok(());
    };
    let outcome = (|| {
        let running = rt.inspect(&id).map_err(|e| format!("inspect: {e}"))?;
        ensure(
            running.exit.is_none() && running.state == BoxState::Running,
            || format!("a box whose init waits: {running:?}"),
        )?;
        rt.signal(&id, Signal::KILL)
            .map_err(|e| format!("signal: {e}"))?;
        let info = exit_of(rt, fx, &id)?;
        ensure(info.exit == Some(Exit::Signal(9)), || {
            format!("exit {:?}", info.exit)
        })
    })();
    rt.delete(&id).map_err(|e| e.to_string())?;
    outcome
}

/// A change that breaks one value of a spec.
type Break = fn(&mut OciSpec);

/// HEX-RT-13: each spec breaks one value no runtime can render; create refuses it before it makes
/// anything, naming the field (CLAUDE.md §3). Use the fixture's backend so a VM-only
/// endpoint cannot pass by skipping structured-workload validation.
fn unrenderable(rt: &dyn Runtime, fx: &dyn Fixture) -> Result<(), String> {
    let id = id("unrenderable")?;
    let class = fx.spec(id.clone(), Bounds::default()).class.class();
    let make: fn(Box<OciSpec>) -> ClassSpec = match class {
        Class::Process if !rt.classes().contains(&Class::Oci) => return Ok(()),
        Class::Process | Class::Oci => ClassSpec::Oci,
        Class::Firecracker => ClassSpec::Firecracker,
        Class::Shards => ClassSpec::Shards,
    };
    let _ = rt.delete(&id);
    let base = OciSpec {
        rootfs: Rootfs::Dir("/hex-conformance/rootfs".into()),
        init: fx.program(&Act::Wait),
        mounts: vec![Mount {
            source: "/hex-conformance/data".into(),
            destination: "/data".to_owned(),
            read_only: true,
        }],
        hostname: Some("conformance".to_owned()),
        network: Network::Private,
        hosts: vec![("db".to_owned(), "10.0.0.2".to_owned())],
    };
    let broken: [(&str, Break); 5] = [
        ("mounts[0].destination", |s| {
            s.mounts = vec![Mount {
                source: "/hex-conformance/data".into(),
                destination: "data".to_owned(),
                read_only: true,
            }];
        }),
        ("rootfs.layers", |s| s.rootfs = Rootfs::Layers(Vec::new())),
        ("hosts[0].address", |s| {
            s.hosts = vec![("db".to_owned(), "db.example".to_owned())];
        }),
        ("hostname", |s| s.hostname = Some("two words".to_owned())),
        ("init.cwd", |s| s.init.cwd = Some("relative".to_owned())),
    ];
    for (field, break_it) in broken {
        let mut spec = base.clone();
        break_it(&mut spec);
        let got = rt.create(BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id: id.clone(),
            bounds: Bounds::default(),
            class: make(Box::new(spec)),
            owner: None,
            band: 0,
        });
        ensure(
            matches!(&got, Err(Refusal::Invalid { what: "oci spec", why }) if why.contains(field)),
            || format!("create with a broken {field}: {got:?}"),
        )?;
        let left = rt.inspect(&id);
        ensure(left == Err(Refusal::NoSuchBox(id.clone())), || {
            format!("a refused create (broken {field}) made {left:?}")
        })?;
    }
    Ok(())
}

/// HEX-RT-18 (goal 5 extended, step 6): an owner whose name isn't well formed is refused before
/// anything is made, by the field's name. Boxes of one well-formed owner, made and deleted in turn, are
/// made as any box is and run what any box runs: a node hands the later box the cgroup the earlier one
/// left, which the box sees only in its usage.
fn owned(rt: &dyn Runtime, fx: &dyn Fixture) -> Result<(), String> {
    let id = id("owned")?;
    let _ = rt.delete(&id);
    let long = "o".repeat(BoxOwner::MAX_NAME + 1);
    for name in ["", "two words", long.as_str()] {
        let mut spec = fx.spec(id.clone(), Bounds::default());
        spec.owner = Some(BoxOwner {
            name: name.to_owned(),
            reuse: true,
        });
        let got = rt.create(spec);
        ensure(
            matches!(&got, Err(Refusal::Invalid { what: "box owner", why }) if why.contains("owner.name")),
            || format!("create with owner {name:?}: {got:?}"),
        )?;
        let left = rt.inspect(&id);
        ensure(left == Err(Refusal::NoSuchBox(id.clone())), || {
            format!("a refused create (owner {name:?}) made {left:?}")
        })?;
    }
    let owner = BoxOwner::named("hex-conformance-owner").map_err(|e| e.to_string())?;
    for round in ["first", "second"] {
        let mut spec = fx.spec(id.clone(), Bounds::default());
        spec.owner = Some(owner.clone());
        let class = spec.class.class();
        rt.create(spec)
            .map_err(|e| format!("create of the {round} owned box: {e}"))?;
        if class != Class::Process {
            rt.start(&id)
                .map_err(|e| format!("start of the {round} owned box: {e}"))?;
        }
        let (mut r, w) = piped()?;
        let mut p = rt
            .exec(
                &id,
                fx.program(&Act::Say("an owned box")),
                Stdio {
                    stdout: Some(w),
                    ..Stdio::null()
                },
            )
            .map_err(|e| format!("exec in the {round} owned box: {e}"))?;
        let exit = ended(&mut p)?;
        let mut out = String::new();
        r.read_to_string(&mut out)
            .map_err(|e| format!("read: {e}"))?;
        rt.delete(&id)
            .map_err(|e| format!("delete of the {round} owned box: {e}"))?;
        ensure(exit == Exit::Code(0) && out == "an owned box\n", || {
            format!("the {round} owned box's program: exit {exit:?}, stdout {out:?}")
        })?;
    }
    Ok(())
}

/// HEX-RT-15 (runtime design goal 7, decision 7): a run is a box for one program's life. Once the caller is told
/// it ended, the box is gone. While it runs it is a box like any other, its id taken. A run the caller let go of is
/// still removed when its program ends, with what it left there, and the suite waits for that fact, bounded by its
/// learned patience ([`gone_within`]). Every OS's `process` class runs one: Linux's, macOS's and Windows' (Ada,
/// 2026-11-09). An `oci` box has an init to create and start, so its spec's run is refused, and a refused run
/// makes nothing.
fn run_removes(rt: &dyn Runtime, fx: &dyn Fixture) -> Result<(), String> {
    let id = id("run")?;
    let _ = rt.delete(&id);
    let spec = fx.spec(id.clone(), Bounds::default());
    let gone = Err(Refusal::NoSuchBox(id.clone()));
    let has_init = spec.class.class() != Class::Process;
    if has_init {
        let got = rt.run(spec, fx.program(&Act::Say("no")), Stdio::null());
        let refused = matches!(&got, Err(Refusal::Invalid { what: "run", .. }));
        ensure(refused, || {
            format!("run of an oci spec: {:?}", got.as_ref().err())
        })?;
        let left = rt.inspect(&id);
        return ensure(left == gone, || format!("a refused run made {left:?}"));
    }
    let (mut r, w) = piped()?;
    let mut p = rt
        .run(
            spec,
            fx.program(&Act::Say("hello from a run")),
            Stdio {
                stdout: Some(w),
                ..Stdio::null()
            },
        )
        .map_err(|e| format!("run: {e}"))?;
    let exit = ended(&mut p)?;
    let mut out = String::new();
    r.read_to_string(&mut out)
        .map_err(|e| format!("read: {e}"))?;
    ensure(exit == Exit::Code(0), || format!("exit {exit:?}"))?;
    ensure(out == "hello from a run\n", || format!("stdout {out:?}"))?;
    let left = rt.inspect(&id);
    ensure(left == gone, || {
        format!("a run whose end was reported left {left:?}")
    })?;
    let listed = rt.list().map_err(|e| e.to_string())?;
    ensure(!listed.iter().any(|b| b.id == id), || {
        format!("{id} still listed after its run ended")
    })?;

    // A run its caller lets go of: a program that ends when its stdin closes, and a second program in its box that
    // waits, which the run's removal ends with whatever else is left there.
    let (in_r, in_w) = piped()?;
    let p = rt
        .run(
            fx.spec(id.clone(), Bounds::default()),
            fx.program(&Act::Echo),
            Stdio {
                stdin: Some(in_r),
                ..Stdio::null()
            },
        )
        .map_err(|e| format!("run of a program reading stdin: {e}"))?;
    let state = rt.inspect(&id).map_err(|e| e.to_string())?.state;
    ensure(state == BoxState::Running, || {
        format!("a running run's box is {state:?}")
    })?;
    let again = rt.create(fx.spec(id.clone(), Bounds::default()));
    ensure(again == Err(Refusal::Exists(id.clone())), || {
        format!("create beside a run: {again:?}")
    })?;
    let mut left_behind = rt
        .exec(&id, fx.program(&Act::Wait), Stdio::null())
        .map_err(|e| format!("exec in a run's box: {e}"))?;
    drop(p);
    drop(in_w);
    let ended_by = ended(&mut left_behind)?;
    ensure(matches!(ended_by, Exit::Signal(_)), || {
        format!("what a let-go run left in its box ended {ended_by:?}")
    })?;
    gone_within(rt, fx, &id)
}

/// A Shards image VM `name`, made and not started.
fn image_made(rt: &dyn Runtime, fx: &dyn Fixture, name: &str) -> Result<BoxId, String> {
    let id = id(name)?;
    let _ = rt.delete(&id);
    rt.create(fx.spec(id.clone(), Bounds::default()))
        .map_err(|e| format!("create: {e}"))?;
    Ok(id)
}

/// Runs `case` on box `id`, then deletes it whatever the case found: a failed case leaves no VM.
fn deleting(
    rt: &dyn Runtime,
    id: &BoxId,
    case: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    let outcome = case();
    let deleted = rt.delete(id).map_err(|e| format!("delete: {e}"));
    outcome.and(deleted)
}

fn image_lifecycle(rt: &dyn Runtime, fx: &dyn Fixture) -> Result<(), String> {
    let id = image_made(rt, fx, "image-life")?;
    deleting(rt, &id, || {
        let listed = rt.list().map_err(|e| e.to_string())?;
        ensure(listed.iter().any(|b| b.id == id), || {
            format!("{id} isn't in {listed:?}")
        })?;
        let info = rt.inspect(&id).map_err(|e| e.to_string())?;
        ensure(
            (info.class, info.state, info.exit) == (Class::Shards, BoxState::Created, None),
            || format!("a new VM: {info:?}"),
        )?;
        let again = rt.create(fx.spec(id.clone(), Bounds::default()));
        ensure(again == Err(Refusal::Exists(id.clone())), || {
            format!("second create: {again:?}")
        })?;
        rt.start(&id).map_err(|e| format!("start: {e}"))?;
        let info = rt.inspect(&id).map_err(|e| e.to_string())?;
        ensure((info.state, info.exit) == (BoxState::Running, None), || {
            format!("a started VM: {info:?}")
        })?;
        let again = rt.start(&id);
        ensure(
            matches!(again, Err(Refusal::Invalid { what: "start", .. })),
            || format!("a second start: {again:?}"),
        )
    })?;
    let gone = Refusal::NoSuchBox(id.clone());
    for (call, got) in [
        ("inspect", rt.inspect(&id).err()),
        ("usage", rt.usage(&id).err()),
        ("signal", rt.signal(&id, Signal::KILL).err()),
        ("start", rt.start(&id).err()),
        ("delete", rt.delete(&id).err()),
    ] {
        ensure(got.as_ref() == Some(&gone), || {
            format!("{call} after delete: {got:?}")
        })?;
    }
    Ok(())
}

fn image_stop(rt: &dyn Runtime, fx: &dyn Fixture) -> Result<(), String> {
    let id = image_made(rt, fx, "image-stop")?;
    deleting(rt, &id, || {
        rt.start(&id).map_err(|e| format!("start: {e}"))?;
        rt.signal(&id, Signal::KILL)
            .map_err(|e| format!("stop: {e}"))?;
        let info = exit_of(rt, fx, &id)?;
        ensure(
            (info.state, info.exit) == (BoxState::Empty, Some(Exit::Signal(9))),
            || format!("a stopped VM: {info:?}"),
        )?;
        let again = rt.signal(&id, Signal::KILL);
        ensure(matches!(again, Err(Refusal::NotRunning(..))), || {
            format!("a signal to a stopped VM: {again:?}")
        })?;
        let kept = rt.inspect(&id).map(|info| info.id);
        ensure(kept.as_ref() == Ok(&id), || {
            format!("a stopped VM before its delete: {kept:?}")
        })
    })
}

fn image_command_exit(rt: &dyn Runtime, fx: &dyn Fixture) -> Result<(), String> {
    let id = id("image-exit")?;
    let _ = rt.delete(&id);
    rt.create(fx.init(id.clone(), &Act::Fail(3)))
        .map_err(|e| format!("create: {e}"))?;
    deleting(rt, &id, || {
        rt.start(&id).map_err(|e| format!("start: {e}"))?;
        let info = exit_of(rt, fx, &id)?;
        ensure(
            (info.state, info.exit) == (BoxState::Empty, Some(Exit::Code(3))),
            // Where its run failed, its runtime's words first (joint plan step 12).
            || match &info.ended {
                Some(words) => format!(
                    "a VM whose command exited 3 ended {:?}, its runtime saying: {words}; {info:?}",
                    info.exit
                ),
                None => format!("a VM whose command exited 3: {info:?}"),
            },
        )
    })
}

/// A usage's guest time per vCPU, or why a case can't hold it to the contract.
fn per_vcpu(usage: crate::Usage) -> Result<Vec<u64>, String> {
    match usage.guest {
        Some(GuestTime::PerVcpuNs(ns)) => Ok(ns),
        Some(GuestTime::Awaits(needs)) => Err(format!("guest time per vCPU awaits {needs}")),
        None => Err("usage reports no guest time per vCPU".to_owned()),
    }
}

fn image_guest_time(rt: &dyn Runtime, fx: &dyn Fixture) -> Result<(), String> {
    let id = image_made(rt, fx, "image-guest")?;
    let vcpus = fx
        .spec(id.clone(), Bounds::default())
        .allocation
        .ok_or("the fixture's VM has no allocation")?
        .vm_size()
        .map_err(|e| e.to_string())?
        .vcpus;
    deleting(rt, &id, || {
        let early = rt.usage(&id);
        ensure(matches!(early, Err(Refusal::NotRunning(..))), || {
            format!("usage before its start: {early:?}")
        })?;
        let before = std::time::Instant::now();
        rt.start(&id).map_err(|e| format!("start: {e}"))?;
        let first = per_vcpu(rt.usage(&id).map_err(|e| format!("usage: {e}"))?)?;
        let second = per_vcpu(rt.usage(&id).map_err(|e| format!("usage: {e}"))?)?;
        let wall = u64::try_from(before.elapsed().as_nanos()).unwrap_or(u64::MAX);
        let count = usize::try_from(vcpus).map_err(|e| e.to_string())?;
        ensure(first.len() == count && second.len() == count, || {
            format!(
                "{} and {} entries for {vcpus} vCPUs",
                first.len(),
                second.len()
            )
        })?;
        ensure(first.iter().zip(&second).all(|(a, b)| a <= b), || {
            format!("guest time fell between reads: {first:?} then {second:?}")
        })?;
        ensure(second.iter().all(|ns| *ns <= wall), || {
            format!("guest time {second:?} past the {wall} ns since its start")
        })?;
        rt.signal(&id, Signal::KILL)
            .map_err(|e| format!("stop: {e}"))?;
        exit_of(rt, fx, &id)?;
        let late = rt.usage(&id);
        ensure(matches!(late, Err(Refusal::NotRunning(..))), || {
            format!("usage after its end: {late:?}")
        })
    })
}

fn image_members(rt: &dyn Runtime, fx: &dyn Fixture) -> Result<(), String> {
    let id = image_made(rt, fx, "image-members")?;
    deleting(rt, &id, || {
        rt.start(&id).map_err(|e| format!("start: {e}"))?;
        let exec = rt.exec(&id, fx.program(&Act::Say("never")), Stdio::null());
        ensure(
            matches!(
                exec.as_ref().err(),
                Some(Refusal::Awaits {
                    needs: Upstream::MemberExec
                })
            ),
            || format!("exec in a VM: {:?}", exec.as_ref().err()),
        )?;
        let term = rt.signal(&id, Signal::TERM);
        ensure(
            term == Err(Refusal::Awaits {
                needs: Upstream::MemberSignal,
            }),
            || format!("TERM to a VM: {term:?}"),
        )?;
        let info = rt.inspect(&id).map_err(|e| e.to_string())?;
        ensure((info.state, info.exit) == (BoxState::Running, None), || {
            format!("a VM after its members were refused: {info:?}")
        })
    })
}

/// A change that leaves a Shards image VM's spec unbootable, and the field its refusal names.
type Unbootable = (&'static str, fn(&mut BoxSpec));

fn image_unbootable(rt: &dyn Runtime, fx: &dyn Fixture) -> Result<(), String> {
    let id = id("image-unbootable")?;
    let _ = rt.delete(&id);
    let cases: [Unbootable; 5] = [
        ("image", |spec| {
            if let ClassSpec::ShardsImage(image) = &mut spec.class {
                image.image = "hex-conformance/harness:latest".to_owned();
            }
        }),
        ("digest", |spec| {
            if let ClassSpec::ShardsImage(image) = &mut spec.class {
                image.image = "hex-conformance/harness@sha256:abc".to_owned();
            }
        }),
        ("command", |spec| {
            if let ClassSpec::ShardsImage(image) = &mut spec.class {
                image.command = Some(Vec::new());
            }
        }),
        ("allocation", |spec| spec.allocation = None),
        ("devices", |spec| {
            spec.devices = vec![DeviceId {
                kind: DeviceKind::Gpu,
                key: 0,
            }]
        }),
    ];
    for (field, unboot) in cases {
        let mut spec = fx.spec(id.clone(), Bounds::default());
        ensure(matches!(spec.class, ClassSpec::ShardsImage(_)), || {
            "the fixture's spec isn't a Shards image VM".to_owned()
        })?;
        unboot(&mut spec);
        let got = rt.create(spec);
        ensure(
            matches!(&got, Err(Refusal::Invalid { what: "shards image", why }) if why.contains(field)),
            || format!("create with a broken {field}: {got:?}"),
        )?;
        let left = rt.inspect(&id);
        ensure(left == Err(Refusal::NoSuchBox(id.clone())), || {
            format!("a refused create (broken {field}) made {left:?}")
        })?;
    }
    Ok(())
}

/// HEX-IMG-7 (joint plan step 15, AC-T7.7.2; docs/design/network-grants.md N4): what Shards' destinations (D236) can't
/// express at all is refused typed, naming what it awaits, before anything is made; the VM's host named as an egress
/// destination is invalid, as is a malformed grant. A destination by TCP is kept, its member held to it alone as the
/// VM boots, and a publish on a run of the members alone and on one with a command of its own, held to the receiver
/// Shards names as it boots (N6); inspect shows every grant as the record holds it.
fn image_grants(rt: &dyn Runtime, fx: &dyn Fixture) -> Result<(), String> {
    let id = id("image-grants")?;
    let _ = rt.delete(&id);
    let granted = |grants: Vec<Grant>, command: Option<Vec<String>>| -> Result<BoxSpec, String> {
        let mut spec = fx.spec(id.clone(), Bounds::default());
        let ClassSpec::ShardsImage(image) = &mut spec.class else {
            return Err("the fixture's spec isn't a Shards image VM".to_owned());
        };
        image.grants = grants;
        image.command = command;
        Ok(spec)
    };
    let codex = Member {
        kind: MemberKind::Harness,
        name: "codex".to_owned(),
    };
    let tcp = |port| Ports {
        proto: Proto::Tcp,
        first: port,
        last: port,
    };
    let udp = |port| Ports {
        proto: Proto::Udp,
        first: port,
        last: port,
    };
    let egress = |to: Destination, ports| Grant::Egress {
        member: codex.clone(),
        to,
        ports,
    };
    let unheld = [
        (
            egress(Destination::Name("api.openai.com".to_owned()), udp(443)),
            Upstream::MemberGrantsByName,
        ),
        (
            egress(
                Destination::Address(IpAddr::from([0x2001, 0xdb8, 0, 0, 0, 0, 0, 5])),
                tcp(7070),
            ),
            Upstream::MemberGrantsByAddress,
        ),
        (
            Grant::Host {
                member: codex.clone(),
                ports: udp(7070),
            },
            Upstream::MemberHostService,
        ),
    ];
    for (grant, needs) in unheld {
        let got = rt.create(granted(vec![grant], None)?);
        ensure(got == Err(Refusal::Awaits { needs }), || {
            format!("create granted what awaits {needs:?}: {got:?}")
        })?;
        let left = rt.inspect(&id);
        ensure(left == Err(Refusal::NoSuchBox(id.clone())), || {
            format!("a refused create ({needs:?}) made {left:?}")
        })?;
    }
    let invalid = [
        egress(
            Destination::Name("host.docker.internal".to_owned()),
            tcp(7070),
        ),
        Grant::Host {
            member: codex.clone(),
            ports: tcp(0),
        },
    ];
    for grant in invalid {
        let got = rt.create(granted(vec![grant.clone()], None)?);
        // Refused naming the grants: by the class, or before it by a client, whose wire writes no request whose grants
        // its check refuses (hex-core's wire; ROOT's r558 through a real node).
        let named = match &got {
            Err(Refusal::Invalid { what: "grants", .. }) => true,
            Err(Refusal::Invalid {
                what: "message",
                why,
            }) => why.starts_with("grants isn't valid"),
            _ => false,
        };
        ensure(named, || format!("create granted {grant:?}: {got:?}"))?;
        let left = rt.inspect(&id);
        ensure(left == Err(Refusal::NoSuchBox(id.clone())), || {
            format!("a refused create ({grant:?}) made {left:?}")
        })?;
    }
    let made = [
        egress(Destination::Name("api.openai.com".to_owned()), tcp(443)),
        egress(Destination::Address(IpAddr::from([10, 0, 0, 5])), tcp(7070)),
        Grant::Host {
            member: codex.clone(),
            ports: tcp(7070),
        },
        Grant::Publish {
            member: codex,
            proto: Proto::Tcp,
            port: 1455,
            host: IpAddr::from([127, 0, 0, 1]),
            host_port: None,
        },
    ];
    for grant in made {
        for command in [None, Some(vec!["serve".to_owned()])] {
            let commanded = command.is_some();
            rt.create(granted(vec![grant.clone()], command)?)
                .map_err(|e| {
                    format!("create granted {grant:?} (a command of its own: {commanded}): {e}")
                })?;
            let shown = rt.inspect(&id).map_err(|e| format!("inspect: {e}"));
            rt.delete(&id).map_err(|e| format!("delete: {e}"))?;
            let shown = shown?.grants;
            ensure(shown == vec![grant.clone()], || {
                format!("inspect showed the grants {shown:?} (a command of its own: {commanded})")
            })?;
        }
    }
    Ok(())
}

/// HEX-IMG-8 (joint plan step 33, AC-T7.7.2; Shards' D242; docs/design/credential-grants.md C2, C4): a credential grant
/// grants no egress and names exactly whose flows its value is filled into, so one that could never be filled in as it
/// says is refused, naming credentials, before anything is made. One that can is made where its node leases credentials,
/// and inspect shows it as the record holds it, or refused as its node's configuration where it leases none.
fn image_credentials(rt: &dyn Runtime, fx: &dyn Fixture) -> Result<(), String> {
    let id = id("image-credentials")?;
    let _ = rt.delete(&id);
    let claude = Member {
        kind: MemberKind::Harness,
        name: "claude".to_owned(),
    };
    let helper = Member {
        kind: MemberKind::Agent,
        name: "helper".to_owned(),
    };
    let egress = |member: &Member, host: &str| Grant::Egress {
        member: member.clone(),
        to: Destination::Name(host.to_owned()),
        ports: Ports {
            proto: Proto::Tcp,
            first: 443,
            last: 443,
        },
    };
    let credential = |member: &Member, reference: &str, toward: &[&str]| CredentialGrant {
        member: member.clone(),
        reference: reference.to_owned(),
        toward: toward.iter().map(|h| (*h).to_owned()).collect(),
    };
    let granted =
        |grants: Vec<Grant>, credentials: Vec<CredentialGrant>| -> Result<BoxSpec, String> {
            let mut spec = fx.spec(id.clone(), Bounds::default());
            let ClassSpec::ShardsImage(image) = &mut spec.class else {
                return Err("the fixture's spec isn't a Shards image VM".to_owned());
            };
            image.grants = grants;
            image.credentials = credentials;
            Ok(spec)
        };
    let providers = || {
        vec![
            egress(&claude, "api.anthropic.com"),
            egress(&claude, "platform.claude.com"),
        ]
    };
    let never = [
        (
            Vec::new(),
            vec![credential(&claude, "anthropic/ada", &["api.anthropic.com"])],
        ),
        (providers(), vec![credential(&claude, "anthropic/ada", &[])]),
        (
            providers(),
            vec![credential(&claude, "anthropic ada", &["api.anthropic.com"])],
        ),
        (
            providers(),
            vec![
                credential(&claude, "anthropic/ada", &["api.anthropic.com"]),
                credential(&claude, "anthropic/ada", &["platform.claude.com"]),
            ],
        ),
        (
            providers(),
            vec![
                credential(&claude, "anthropic/ada", &["api.anthropic.com"]),
                credential(&claude, "anthropic/team", &["api.anthropic.com"]),
            ],
        ),
        (
            [providers(), vec![egress(&helper, "api.anthropic.com")]].concat(),
            vec![credential(&claude, "anthropic/ada", &["api.anthropic.com"])],
        ),
    ];
    for (grants, credentials) in never {
        let got = rt.create(granted(grants, credentials.clone())?);
        // Refused naming the credentials: by the class, or before it by a client, whose wire writes no request whose
        // credentials its check refuses (hex-core's wire).
        let named = match &got {
            Err(Refusal::Invalid {
                what: "credentials",
                ..
            }) => true,
            Err(Refusal::Invalid {
                what: "message",
                why,
            }) => why.starts_with("credentials isn't valid"),
            _ => false,
        };
        ensure(named, || format!("create granted {credentials:?}: {got:?}"))?;
        let left = rt.inspect(&id);
        ensure(left == Err(Refusal::NoSuchBox(id.clone())), || {
            format!("a refused create ({credentials:?}) made {left:?}")
        })?;
    }
    let credentials = vec![credential(
        &claude,
        "anthropic/ada",
        &["api.anthropic.com", "platform.claude.com"],
    )];
    match rt.create(granted(providers(), credentials.clone())?) {
        Ok(_) => {
            let shown = rt.inspect(&id).map_err(|e| format!("inspect: {e}"));
            rt.delete(&id).map_err(|e| format!("delete: {e}"))?;
            let shown = shown?.credentials;
            ensure(shown == credentials, || {
                format!("inspect showed the credential grants {shown:?}")
            })
        }
        Err(Refusal::Unconfigured { .. }) => {
            let left = rt.inspect(&id);
            ensure(left == Err(Refusal::NoSuchBox(id.clone())), || {
                format!("a create its node can't lease made {left:?}")
            })
        }
        Err(e) => Err(format!("create granting {credentials:?}: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::sim::{SimFixture, SimRuntime};
    use crate::{BoxInfo, Proc, Usage};

    /// The simulator, whose `inspect` shows a box's exit only from its `shown`th look on, as a node that sees an
    /// init's end some looks after a case asks; `looks` counts them.
    struct Late {
        sim: SimRuntime,
        looks: AtomicUsize,
        shown: usize,
    }

    impl Late {
        /// Box `id`, an `oci` box whose init exits with 3 once started (the simulator's `fail 3`), made and started.
        fn with_exiting_box(id: &BoxId, shown: usize) -> Self {
            let late = Self {
                sim: SimRuntime::new(),
                looks: AtomicUsize::new(0),
                shown,
            };
            late.sim
                .create(BoxSpec {
                    route: None,
                    allocation: None,
                    devices: Vec::new(),
                    id: id.clone(),
                    bounds: Bounds::default(),
                    class: ClassSpec::Oci(Box::new(OciSpec {
                        rootfs: Rootfs::Dir("/rootfs".into()),
                        init: Program::of(&["fail", "3"]),
                        mounts: Vec::new(),
                        hostname: None,
                        network: Network::Private,
                        hosts: Vec::new(),
                    })),
                    owner: None,
                    band: 0,
                })
                .unwrap();
            late.sim.start(id).unwrap();
            late
        }
    }

    impl Runtime for Late {
        fn classes(&self) -> Vec<Class> {
            self.sim.classes()
        }
        fn create(&self, spec: BoxSpec) -> Result<BoxInfo, Refusal> {
            self.sim.create(spec)
        }
        fn start(&self, id: &BoxId) -> Result<(), Refusal> {
            self.sim.start(id)
        }
        fn exec(
            &self,
            id: &BoxId,
            program: Program,
            stdio: Stdio,
        ) -> Result<Box<dyn Proc>, Refusal> {
            self.sim.exec(id, program, stdio)
        }
        fn run(
            &self,
            spec: BoxSpec,
            program: Program,
            stdio: Stdio,
        ) -> Result<Box<dyn Proc>, Refusal> {
            self.sim.run(spec, program, stdio)
        }
        fn signal(&self, id: &BoxId, signal: Signal) -> Result<(), Refusal> {
            self.sim.signal(id, signal)
        }
        fn inspect(&self, id: &BoxId) -> Result<BoxInfo, Refusal> {
            let mut info = self.sim.inspect(id)?;
            if self.looks.fetch_add(1, Ordering::Relaxed) + 1 < self.shown {
                info.exit = None;
            }
            Ok(info)
        }
        fn usage(&self, id: &BoxId) -> Result<Usage, Refusal> {
            self.sim.usage(id)
        }
        fn list(&self) -> Result<Vec<BoxInfo>, Refusal> {
            self.sim.list()
        }
        fn delete(&self, id: &BoxId) -> Result<(), Refusal> {
            self.sim.delete(id)
        }
    }

    /// Past the slowest end on this host class's record until 2026-11-13 (Linux 957 ms, macOS 939 ms, Windows 6,239 ms;
    /// docs/research/runtime/learned-waits.md, "Owed" 2): how late [`Slow`]'s programs end, as the program HEX-RT-2
    /// met on the Mac Studio ended later than its class's record.
    const SLOWER: Duration = Duration::from_millis(if cfg!(windows) { 6_240 } else { 958 });

    /// The simulator, its programs ending [`SLOWER`] after they start: each answers as the simulator's does, only
    /// that late.
    struct Slow {
        sim: SimRuntime,
    }

    /// A program of [`Slow`]'s: the simulator's, its end shown from `ends_at` on.
    struct SlowProc {
        inner: Box<dyn Proc>,
        ends_at: std::time::Instant,
    }

    impl Proc for SlowProc {
        fn pid(&self) -> u32 {
            self.inner.pid()
        }
        fn try_wait(&mut self) -> Result<Option<Exit>, Refusal> {
            if std::time::Instant::now() < self.ends_at {
                return Ok(None);
            }
            self.inner.try_wait()
        }
        fn wait_for(&mut self, timeout: Duration) -> Result<Option<Exit>, Refusal> {
            let left = self
                .ends_at
                .saturating_duration_since(std::time::Instant::now());
            if timeout < left {
                rest(timeout).map_err(Refusal::Broken)?;
                return Ok(None);
            }
            rest(left).map_err(Refusal::Broken)?;
            self.inner.wait_for(timeout.saturating_sub(left))
        }
        fn kill(&mut self) -> Result<(), Refusal> {
            self.inner.kill()
        }
        fn streams(&mut self) -> Result<crate::Given, Refusal> {
            self.inner.streams()
        }
    }

    impl Runtime for Slow {
        fn classes(&self) -> Vec<Class> {
            self.sim.classes()
        }
        fn create(&self, spec: BoxSpec) -> Result<BoxInfo, Refusal> {
            self.sim.create(spec)
        }
        fn start(&self, id: &BoxId) -> Result<(), Refusal> {
            self.sim.start(id)
        }
        fn exec(
            &self,
            id: &BoxId,
            program: Program,
            stdio: Stdio,
        ) -> Result<Box<dyn Proc>, Refusal> {
            let inner = self.sim.exec(id, program, stdio)?;
            let ends_at = std::time::Instant::now().checked_add(SLOWER).unwrap();
            Ok(Box::new(SlowProc { inner, ends_at }))
        }
        fn run(
            &self,
            spec: BoxSpec,
            program: Program,
            stdio: Stdio,
        ) -> Result<Box<dyn Proc>, Refusal> {
            self.sim.run(spec, program, stdio)
        }
        fn signal(&self, id: &BoxId, signal: Signal) -> Result<(), Refusal> {
            self.sim.signal(id, signal)
        }
        fn inspect(&self, id: &BoxId) -> Result<BoxInfo, Refusal> {
            self.sim.inspect(id)
        }
        fn usage(&self, id: &BoxId) -> Result<Usage, Refusal> {
            self.sim.usage(id)
        }
        fn list(&self) -> Result<Vec<BoxInfo>, Refusal> {
            self.sim.list()
        }
        fn delete(&self, id: &BoxId) -> Result<(), Refusal> {
            self.sim.delete(id)
        }
    }

    /// Decision 9 (original; the person's ruling of 2026-11-13): a case's wait for a program's end was bounded at its
    /// host class's recorded end, a record seed, and refused a program slower than it: goal 12's macOS checks (r20 on
    /// the Mac Studio) failed hex_quic_20's conformance over QUIC, HEX-RT-2 "still running after 939ms". Do: run
    /// HEX-RT-2 against the simulator, its program ending [`SLOWER`] after its start, past this class's record.
    /// Expect: it passes, the program's end waited for however late it came.
    #[test]
    fn original_hex_rt_2_passes_with_a_program_slower_than_the_recorded_end() {
        let case = CASES.iter().find(|c| c.id == "HEX-RT-2").unwrap();
        let slow = Slow {
            sim: SimRuntime::new(),
        };
        let began = std::time::Instant::now();
        let got = (case.run)(&slow, &SimFixture::Process);
        assert_eq!(
            got,
            Ok(()),
            "HEX-RT-2 refused a program slower than its class's record"
        );
        assert!(began.elapsed() >= SLOWER, "its program ended before SLOWER");
    }

    /// The simulator's fixture, its runtime's life ended as the wait looks at it: a double whose answers can't change.
    struct EndedFixture(SimFixture);

    impl Fixture for EndedFixture {
        fn spec(&self, id: BoxId, bounds: Bounds) -> BoxSpec {
            self.0.spec(id, bounds)
        }
        fn init(&self, id: BoxId, act: &Act) -> BoxSpec {
            self.0.init(id, act)
        }
        fn program(&self, act: &Act) -> Program {
            self.0.program(act)
        }
        fn party(&self) -> Party {
            Party::Ended
        }
    }

    /// Decision 9 (the person's ruling of 2026-11-13): an exit case waits on its fact, a box's reported exit, while its
    /// runtime lives, however many looks it takes. Do: an `oci` box whose init exits with 3, its exit reported from the
    /// third look; wait for it, the runtime alive. Expect: Code(3) at the third look, never "no exit reported" at the
    /// first.
    #[test]
    fn an_exit_case_waits_for_the_exit_past_the_first_look() {
        let id = BoxId::new("late").unwrap();
        let late = Late::with_exiting_box(&id, 3);
        assert_eq!(
            exit_of(&late, &SimFixture::Process, &id).map(|info| info.exit),
            Ok(Some(Exit::Code(3)))
        );
        assert_eq!(late.looks.load(Ordering::Relaxed), 3);
    }

    /// A wait whose runtime has ended fails at once, once a last look hasn't found the fact. Do: the same box, its exit
    /// never reported, waited for with the runtime's life ended. Expect: refused at its first look, saying so.
    #[test]
    fn an_exit_never_reported_fails_the_wait_at_its_runtimes_end() {
        let id = BoxId::new("never").unwrap();
        let late = Late::with_exiting_box(&id, usize::MAX);
        let refused = exit_of(&late, &EndedFixture(SimFixture::Process), &id).unwrap_err();
        assert!(
            refused.starts_with("no exit reported, its runtime ended"),
            "{refused}"
        );
        assert_eq!(late.looks.load(Ordering::Relaxed), 1);
    }

    /// HEX-RT-4 checks that a waiting program runs at the moment it signals the box: its Signal(15) end
    /// then proves it hadn't ended on its own (that would be Code(0)), so the case waits out no watch (the
    /// lead, 2026-11-07). Do: once an end of a second has been seen, so a watch lasts that long at least, run HEX-RT-4
    /// against the simulator, timed. Expect: it passes in less than one [`still_running`] watch.
    #[test]
    fn the_signal_case_waits_out_no_watch() {
        ENDS.answered(Duration::from_secs(1));
        let case = CASES.iter().find(|c| c.id == "HEX-RT-4").unwrap();
        let t = std::time::Instant::now();
        (case.run)(&SimRuntime::new(), &SimFixture::Process).unwrap();
        let took = t.elapsed();
        assert!(
            still_running().is_some_and(|watch| took < watch),
            "HEX-RT-4 took {took:?}"
        );
    }
}
