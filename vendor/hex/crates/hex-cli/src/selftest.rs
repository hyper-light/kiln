//! `hex selftest-box <act> [args]`: small real programs a node agent runs inside a box to prove the
//! box holds its bounds — a program that uses memory is killed in a box bounded below it, one that
//! starts processes is refused past the box's process bound. The runtime conformance suite
//! (`hex-api`) drives the same acts, so the real classes are checked against the same contract as
//! the simulator. Every act returns an exit status; none panics.

use std::io::{Read, Write};
use std::process::{Command, Stdio};

#[cfg(target_os = "linux")]
mod tree;

/// Memory taken per chunk while growing, so a box's bound kills the act partway, not after a single
/// huge allocation the allocator might refuse first.
const CHUNK: usize = 1 << 20;

/// Fills `buf` with an incompressible xorshift stream, so the memory the act takes shows in the box's
/// footprint on every OS. Linux's cgroup counts the resident pages whatever they hold, but macOS's
/// physical-footprint ledger — against which a jetsam per-process cap (the native `process` class's
/// `_ext` backstop) and the watcher's rule both read — counts a page at its *compressed* size, so a box
/// filled with zeros (or any compressible pattern) stays far under its bound and is never bounded. An
/// incompressible fill can't be shrunk, so the footprint is the allocation's true size and the bound
/// bites. Writing a word at a time (not byte) keeps it cheap; a trailing partial word is left as is.
fn fill_incompressible(buf: &mut [u8], state: &mut u64) {
    let mut x = *state;
    let mut i = 0usize;
    while i < buf.len() {
        x ^= x.wrapping_shl(13);
        x ^= x.wrapping_shr(7);
        x ^= x.wrapping_shl(17);
        if let Some(slot) = buf.get_mut(i..i.saturating_add(8)) {
            slot.copy_from_slice(&x.to_ne_bytes());
        }
        i = i.saturating_add(8);
    }
    *state = x;
}

/// Runs act `argv` (`argv[0]` the act, the rest its arguments) and returns the exit status for the
/// process. `None` means the act isn't one of these.
pub fn run(argv: &[String]) -> Option<u8> {
    let (act, args) = argv.split_first()?;
    match act.as_str() {
        "say" => {
            let line = format!("{}\n", args.join(" "));
            Some(u8::from(
                write_all(&mut std::io::stdout(), line.as_bytes()).is_err(),
            ))
        }
        "fail" => {
            let _ = std::io::stderr().write_all(b"failing\n");
            Some(args.first().and_then(|c| c.parse::<u8>().ok()).unwrap_or(1))
        }
        "wait" => loop {
            std::thread::park();
        },
        // serve: says "up" once it runs, then waits like `wait` — a start the native bench times to
        // that line, the same for Hex and its competitors (bench/native).
        "serve" => {
            if write_all(&mut std::io::stdout(), b"up\n").is_err() {
                return Some(1);
            }
            loop {
                std::thread::park();
            }
        }
        // climb <bytes> [step]: as `hog`, a step at a time (1 MiB unless given), reporting the KiB it has
        // touched after each, so a bound's overshoot is read from the program itself whatever ends it
        // (bench/native).
        "climb" => Some(climb(
            args.first()
                .and_then(|b| b.parse::<u64>().ok())
                .unwrap_or(0),
            args.get(1)
                .and_then(|b| b.parse::<usize>().ok())
                .filter(|s| *s > 0)
                .unwrap_or(CHUNK),
        )),
        "hog" => Some(hog(args
            .first()
            .and_then(|b| b.parse::<u64>().ok())
            .unwrap_or(0))),
        // hold <bytes> <millis>: takes and touches `bytes`, then sits that long — a box held near its
        // bound, for the macOS class bench to measure its watcher's cost (bench/native/macos).
        "hold" => Some(hold(
            args.first()
                .and_then(|b| b.parse::<u64>().ok())
                .unwrap_or(0),
            args.get(1).and_then(|m| m.parse::<u64>().ok()).unwrap_or(0),
        )),
        "spawn" => Some(spawn(
            args.first()
                .and_then(|n| n.parse::<u32>().ok())
                .unwrap_or(0),
        )),
        "echo" => Some(echo()),
        // open <r|w>:<path>...: opens each path to read or to write, creating nothing, and prints a line
        // for each: "<path> ok", or "<path> errno <n>": which devices a box's device program lets it
        // open, and which it refuses (EPERM) rather than finding no device (ENXIO).
        "open" => Some(open_each(args)),
        // tree <dir> | tree --layers <lowest>:...: a manifest of everything under `dir`, or in the overlay
        // of a box's layers, for HEX-GUEST-21's comparison of a staged root with containerd's unpack of
        // the same image (selftest/tree.rs).
        #[cfg(target_os = "linux")]
        "tree" => Some(tree::tree(args)),
        // room <dir>: the bytes free to an unprivileged writer on `dir`'s file system (statvfs(3),
        // `f_bavail` × `f_frsize`), for HEX-GUEST-21 to size its guest by what the guest's staged folder
        // really holds, not by half its memory.
        #[cfg(target_os = "linux")]
        "room" => Some(room(args.first().map(String::as_str).unwrap_or(""))),
        // fsync-probe <file> <iters>: the noisy-neighbour victim — times fsync over small writes
        // and prints "p50_us=<n> p99_us=<n>" (AC-T7.1.2).
        "fsync-probe" => Some(fsync_probe(
            args.first().map(String::as_str).unwrap_or(""),
            args.get(1).and_then(|n| n.parse().ok()).unwrap_or(200),
        )),
        // disk-hog <file> <seconds>: the noisy-neighbour builder — writes and fsyncs a large buffer
        // in a tight loop to saturate the disk for that long.
        "disk-hog" => Some(disk_hog(
            args.first().map(String::as_str).unwrap_or(""),
            args.get(1).and_then(|n| n.parse().ok()).unwrap_or(10),
        )),
        // escape <mode> <marker>: a root-escape attempt from inside a box, for goal 5 plan step 2's
        // security criteria (the pruned-seed root). Exits 0 when the box held (secure). Linux only, as
        // the runtime is (the whole `oci` class is behind `cfg(linux)`); elsewhere it isn't an act.
        #[cfg(target_os = "linux")]
        "escape" => Some(hex_runtime::linux::oci::escape::attempt(
            args.first().map(String::as_str).unwrap_or(""),
            args.get(1).map(String::as_str).unwrap_or(""),
        )),
        // netlink-to <portid>: sends one rtnetlink request from a socket of the box's to the netlink socket
        // <portid> names (0, the kernel), and exits 0 if it went, else the send's errno: whether a box reaches
        // the socket the node keeps in its network namespace (goal 5 extended, step 6).
        "netlink-to" => Some(netlink_to(args.first().map(String::as_str).unwrap_or(""))),
        // win-breakaway: tries to start a child outside the box's job (CREATE_BREAKAWAY_FROM_JOB).
        // Prints whether Windows refused it and exits 0 when refused, 1 when the child escaped — the
        // Windows process class forbids breakaway, so a member asking for it is refused.
        #[cfg(windows)]
        "win-breakaway" => Some(win_breakaway()),
        // win-grandchild: starts a grandchild (a child that outlives this member's own start) and
        // prints its process id, then waits — so a box delete can be shown to end the grandchild too.
        #[cfg(windows)]
        "win-grandchild" => Some(win_grandchild()),
        // win-devices <class GUID>: prints how many present devices implement that interface class where
        // this runs and how many of them open ("<listed> <reachable>"), exits 0 — inside an oci box, a class
        // not granted is listed (the host's PnP view) but none opens.
        #[cfg(windows)]
        "win-devices" => Some(
            match hex_runtime::windows::oci::devices(args.first().map(String::as_str).unwrap_or(""))
            {
                Ok(v) => u8::from(
                    write_all(
                        &mut std::io::stdout(),
                        format!("{} {}\n", v.listed, v.reachable).as_bytes(),
                    )
                    .is_err(),
                ),
                Err(e) => {
                    let _ = write_all(&mut std::io::stderr(), format!("{e}\n").as_bytes());
                    2
                }
            },
        ),
        // write <path>: creates `path` and writes to it; prints "wrote", or "refused <os error>" and exits
        // 1 — a read-only mount refuses it.
        "write" => Some(write_probe(args.first().map(String::as_str).unwrap_or(""))),
        // exists <path>: prints "yes" or "no", exits 0 — a box sees another box's file only if they share a
        // volume.
        "exists" => {
            let seen =
                std::path::Path::new(args.first().map(String::as_str).unwrap_or("")).exists();
            Some(u8::from(
                write_all(
                    &mut std::io::stdout(),
                    if seen { b"yes\n" } else { b"no\n" },
                )
                .is_err(),
            ))
        }
        _ => None,
    }
}

/// Creates `path` and writes a line to it: 0 and "wrote" when it could, 1 and "refused <os error code>"
/// when it couldn't (a read-only mount's refusal).
fn write_probe(path: &str) -> u8 {
    let wrote = std::fs::File::create(path).and_then(|mut f| f.write_all(b"written in a box\n"));
    match wrote {
        Ok(()) => u8::from(write_all(&mut std::io::stdout(), b"wrote\n").is_err()),
        Err(e) => {
            let line = format!("refused {}\n", e.raw_os_error().unwrap_or(-1));
            let _ = write_all(&mut std::io::stdout(), line.as_bytes());
            1
        }
    }
}

/// Prints `bytes=<n>`, what `dir`'s file system has free to an unprivileged writer; 1 when it can't be read.
#[cfg(target_os = "linux")]
fn room(dir: &str) -> u8 {
    let Ok(st) = rustix::fs::statvfs(dir) else {
        let _ = writeln!(std::io::stderr(), "room: can't read {dir}'s file system");
        return 1;
    };
    let Some(bytes) = st.f_bavail.checked_mul(st.f_frsize) else {
        let _ = writeln!(std::io::stderr(), "room: {dir}'s free bytes pass u64");
        return 1;
    };
    u8::from(
        write_all(
            &mut std::io::stdout(),
            format!("bytes={bytes}\n").as_bytes(),
        )
        .is_err(),
    )
}

/// Windows: a child started with `CREATE_BREAKAWAY_FROM_JOB` from inside a job that forbids breakaway.
/// `CreateProcess` fails (access denied), so the spawn is refused; the box's member reports it and
/// exits 0. If the child ever started, it escaped the box: end it and exit 1.
#[cfg(windows)]
fn win_breakaway() -> u8 {
    use std::os::windows::process::CommandExt;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => return 1,
    };
    match Command::new(exe)
        .arg("selftest-box")
        .arg("wait")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_BREAKAWAY_FROM_JOB)
        .spawn()
    {
        Ok(mut child) => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = writeln!(std::io::stdout(), "breakaway=escaped");
            1
        }
        Err(_) => {
            let _ = writeln!(std::io::stdout(), "breakaway=refused");
            0
        }
    }
}

/// Windows: starts a grandchild (a `wait` member) in the box, prints its process id, and waits. The
/// grandchild keeps running until the box's job ends it, so a delete can be shown to reach it.
#[cfg(windows)]
fn win_grandchild() -> u8 {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => return 1,
    };
    match Command::new(exe)
        .arg("selftest-box")
        .arg("wait")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => {
            let _ = writeln!(std::io::stdout(), "grandchild={}", child.id());
            let _ = std::io::stdout().flush();
            loop {
                std::thread::park();
            }
        }
        Err(_) => 1,
    }
}

/// The victim: writes `BLOCK` bytes and fsyncs `iters` times at `path`, printing the p50 and p99
/// fsync latency in microseconds. A box with a tight `io.latency` target keeps this p99 near idle
/// even while a sibling box saturates the disk.
fn fsync_probe(path: &str, iters: u32) -> u8 {
    use std::io::Seek;
    const BLOCK: usize = 4096;
    let mut file = match std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
    {
        Ok(f) => f,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "fsync-probe: {path}: {e}");
            return 1;
        }
    };
    let buf = [b'x'; BLOCK];
    let mut us: Vec<u128> = Vec::with_capacity(iters as usize);
    for _ in 0..iters {
        let _ = file.rewind();
        if file.write_all(&buf).is_err() {
            return 1;
        }
        let t = std::time::Instant::now();
        if file.sync_data().is_err() {
            return 1;
        }
        us.push(t.elapsed().as_micros());
    }
    us.sort_unstable();
    // Percentile by integer index (no float cast in production code): index = len*p/100, clamped.
    let pick = |p: usize| {
        let i = us.len().saturating_mul(p).checked_div(100).unwrap_or(0);
        us.get(i.min(us.len().saturating_sub(1)))
            .copied()
            .unwrap_or(0)
    };
    let _ = writeln!(std::io::stdout(), "p50_us={} p99_us={}", pick(50), pick(99));
    0
}

/// The builder: writes and fsyncs a 1 MiB buffer in a tight loop for `seconds`, saturating the
/// disk — the load the victim must be protected from.
fn disk_hog(path: &str, seconds: u64) -> u8 {
    let mut file = match std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
    {
        Ok(f) => f,
        Err(_) => return 1,
    };
    let buf = vec![b'z'; 1 << 20];
    let deadline = std::time::Instant::now().checked_add(std::time::Duration::from_secs(seconds));
    let mut offset = 0u64;
    while deadline.is_none_or(|d| std::time::Instant::now() < d) {
        use std::io::{Seek, SeekFrom};
        let _ = file.seek(SeekFrom::Start(offset));
        if file.write_all(&buf).is_err() {
            return 1;
        }
        let _ = file.sync_data();
        offset = offset
            .checked_add(buf.len() as u64)
            .filter(|o| *o < (1 << 30))
            .unwrap_or(0);
    }
    0
}

/// Takes and touches `bytes` of memory, a chunk at a time, then holds it briefly. In a box bounded
/// below `bytes` the kernel kills this partway (the caller sees a signal and counts the kill); on an
/// unbounded box it reaches `bytes` and exits 0.
fn hog(bytes: u64) -> u8 {
    let target = usize::try_from(bytes).unwrap_or(usize::MAX);
    let mut held: Vec<Vec<u8>> = Vec::new();
    let mut taken = 0usize;
    let mut rng = 0x9e37_79b9_7f4a_7c15u64;
    while taken < target {
        let want = CHUNK.min(target.saturating_sub(taken));
        let mut chunk = vec![0u8; want];
        fill_incompressible(&mut chunk, &mut rng);
        held.push(chunk);
        taken = taken.saturating_add(want);
    }
    // Hold it long enough that a bound's kill lands before exit; park briefly, then let go.
    let keep = held.len();
    std::thread::park_timeout(std::time::Duration::from_millis(200));
    u8::from(keep == usize::MAX)
}

/// Takes and touches `bytes` a `step` at a time, writing the KiB touched so far as a line after each;
/// exits 0 once all are touched, 1 when stdout is gone. The step is the instrument's resolution: an
/// overshoot reads to within one step.
fn climb(bytes: u64, step: usize) -> u8 {
    let target = usize::try_from(bytes).unwrap_or(usize::MAX);
    let mut held: Vec<Vec<u8>> = Vec::new();
    let mut taken = 0usize;
    let mut rng = 0x9e37_79b9_7f4a_7c15u64;
    let mut out = std::io::stdout();
    while taken < target {
        let want = step.min(target.saturating_sub(taken));
        let mut chunk = vec![0u8; want];
        fill_incompressible(&mut chunk, &mut rng);
        held.push(chunk);
        taken = taken.saturating_add(want);
        let line = format!("{}\n", taken.div_ceil(1024));
        if write_all(&mut out, line.as_bytes()).is_err() {
            return 1;
        }
    }
    drop(held);
    0
}

/// Takes and touches `bytes`, then sits for `millis`, holding the memory: a box kept near its bound for
/// its watcher's cost to be measured. Exits 0 unless the allocation fails.
fn hold(bytes: u64, millis: u64) -> u8 {
    let target = usize::try_from(bytes).unwrap_or(usize::MAX);
    let mut held: Vec<Vec<u8>> = Vec::new();
    let mut taken = 0usize;
    let mut rng = 0x9e37_79b9_7f4a_7c15u64;
    while taken < target {
        let want = CHUNK.min(target.saturating_sub(taken));
        let mut chunk = vec![0u8; want];
        fill_incompressible(&mut chunk, &mut rng);
        held.push(chunk);
        taken = taken.saturating_add(want);
    }
    std::thread::park_timeout(std::time::Duration::from_millis(millis));
    u8::from(held.len() == usize::MAX)
}

/// Starts `n` processes at once (each a `wait` act). Exits 0 when all start, 2 when one is refused
/// (a box's process bound): the caller sees the non-zero exit and counts the refused fork.
fn spawn(n: u32) -> u8 {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => return 1,
    };
    let mut children = Vec::new();
    let mut refused = false;
    for _ in 0..n {
        match Command::new(&exe)
            .arg("selftest-box")
            .arg("wait")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => children.push(child),
            Err(_) => {
                refused = true;
                break;
            }
        }
    }
    for mut child in children {
        let _ = child.kill();
        let _ = child.wait();
    }
    u8::from(refused).saturating_mul(2)
}

/// Opens each `<r|w>:<path>` of `args`, to read or to write and without creating it, and prints each
/// outcome on a line of its own; exits 2 for an argument of neither form.
fn open_each(args: &[String]) -> u8 {
    let mut out = String::new();
    for arg in args {
        let (write, path) = match arg.split_once(':') {
            Some(("r", path)) => (false, path),
            Some(("w", path)) => (true, path),
            _ => return 2,
        };
        match std::fs::OpenOptions::new()
            .read(!write)
            .write(write)
            .open(path)
        {
            Ok(_) => out.push_str(&format!("{path} ok\n")),
            Err(e) => out.push_str(&format!("{path} errno {}\n", e.raw_os_error().unwrap_or(0))),
        }
    }
    u8::from(write_all(&mut std::io::stdout(), out.as_bytes()).is_err())
}

/// `netlink-to`: an `RTM_GETLINK` request (`struct nlmsghdr` and a zeroed `struct ifinfomsg`,
/// include/uapi/linux/rtnetlink.h) sent to `portid`. Exits 0 if it went, the send's errno if it was
/// refused, and 255 for an argument that isn't a portid or a socket that couldn't be made.
#[cfg(target_os = "linux")]
fn netlink_to(portid: &str) -> u8 {
    use rustix::net::{
        AddressFamily, SendFlags, SocketFlags, SocketType, netlink::SocketAddrNetlink,
    };
    const RTM_GETLINK: u16 = 18;
    const NLM_F_REQUEST: u16 = 1;
    const LEN: usize = 32;
    let Ok(pid) = portid.parse::<u32>() else {
        return 255;
    };
    let Ok(fd) = rustix::net::socket_with(
        AddressFamily::NETLINK,
        SocketType::RAW,
        SocketFlags::CLOEXEC,
        None,
    ) else {
        return 255;
    };
    let mut request = Vec::with_capacity(LEN);
    request.extend_from_slice(&u32::try_from(LEN).unwrap_or(u32::MAX).to_ne_bytes());
    request.extend_from_slice(&RTM_GETLINK.to_ne_bytes());
    request.extend_from_slice(&NLM_F_REQUEST.to_ne_bytes());
    request.extend_from_slice(&1u32.to_ne_bytes());
    request.extend_from_slice(&0u32.to_ne_bytes());
    request.resize(LEN, 0);
    match rustix::net::sendto(
        &fd,
        &request,
        SendFlags::empty(),
        &SocketAddrNetlink::new(pid, 0),
    ) {
        Ok(_) => 0,
        Err(e) => u8::try_from(e.raw_os_error()).unwrap_or(255),
    }
}

/// `netlink-to` on a platform with no netlink: refused, 255.
#[cfg(not(target_os = "linux"))]
fn netlink_to(_portid: &str) -> u8 {
    255
}

/// Copies stdin to stdout until stdin ends.
fn echo() -> u8 {
    let mut stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut buf = [0u8; 1 << 16];
    loop {
        match stdin.read(&mut buf) {
            Ok(0) => return 0,
            Ok(n) => {
                if write_all(&mut stdout, buf.get(..n).unwrap_or_default()).is_err() {
                    return 1;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return 1,
        }
    }
}

fn write_all<W: Write>(w: &mut W, bytes: &[u8]) -> std::io::Result<()> {
    w.write_all(bytes)?;
    w.flush()
}
