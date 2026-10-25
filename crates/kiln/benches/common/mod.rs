//! What the sends benches (send_path, send_e2e) record beside their numbers: the master plan's
//! bar for a recording (the hardware and the drive, the date, the load discipline), and their
//! percentiles.

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// The most a recording's machine may be loaded (its 1-minute load average).
pub const QUIET: f64 = 2.0;

/// The 1-, 5- and 15-minute load averages, where the system says.
pub fn load() -> Option<[f64; 3]> {
    let s = std::fs::read_to_string("/proc/loadavg").ok()?;
    let mut f = s.split_whitespace().map(|x| x.parse::<f64>().ok());
    Some([f.next()??, f.next()??, f.next()??])
}

/// The load discipline: waits up to `wait_quiet` seconds for the machine to be quiet, then the
/// load as it was and whether it's loaded (or its load can't be read: no `/proc`, as on a Mac).
/// Refused then unless `any_load` (a smoke run, marked so: never evidence), saying so.
pub fn quiet(bench: &str, wait_quiet: u64, any_load: bool) -> anyhow::Result<(Option<[f64; 3]>, bool)> {
    let deadline = Instant::now().checked_add(Duration::from_secs(wait_quiet));
    let mut before = load();
    while !any_load && before.is_some_and(|l| l[0] >= QUIET) && deadline.is_some_and(|d| Instant::now() < d) {
        std::thread::sleep(Duration::from_secs(5));
        before = load();
    }
    let loaded = before.is_none_or(|l| l[0] >= QUIET);
    if loaded && !any_load {
        let why = match before {
            Some(l) => format!("the load average is {l:?}, at least {QUIET}: not recorded"),
            None => "this machine's load can't be read (no /proc/loadavg): not recorded".to_owned(),
        };
        println!("{}", json!({"bench": bench, "refused": why, "date": now()}));
        anyhow::bail!("refused: the machine is loaded, or its load can't be read");
    }
    Ok((before, loaded))
}

/// Now, as a recording says it.
pub fn now() -> Option<String> {
    time::OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339).ok()
}

/// What the machine is: its name, CPU, cores, memory and kernel.
pub fn machine() -> Value {
    let cpu = std::fs::read_to_string("/proc/cpuinfo").ok().and_then(|s| s.lines().find(|l| l.starts_with("model name")).and_then(|l| l.split_once(':')).map(|(_, m)| m.trim().to_owned()));
    let mem = std::fs::read_to_string("/proc/meminfo").ok().and_then(|s| s.lines().find(|l| l.starts_with("MemTotal")).map(|l| l.split_whitespace().nth(1).unwrap_or_default().to_owned()));
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease").ok().map(|k| k.trim().to_owned());
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname").ok().map(|h| h.trim().to_owned());
    json!({"hostname": host, "cpu": cpu, "cores": std::thread::available_parallelism().map(std::num::NonZero::get).ok(), "mem_kb": mem, "kernel": kernel, "os": std::env::consts::OS, "arch": std::env::consts::ARCH})
}

/// The filesystem `dir` is on, and its device, as the mounts say.
pub fn disk(dir: &Path) -> Value {
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
    let best = mounts
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let (dev, at, fs) = (f.next()?, f.next()?, f.next()?);
            dir.starts_with(at).then(|| (at.len(), dev.to_owned(), at.to_owned(), fs.to_owned()))
        })
        .max_by_key(|(n, ..)| *n);
    match best {
        Some((_, dev, at, fs)) => json!({"dir": dir, "device": dev, "mount": at, "fs": fs, "drive": drive(&dev)}),
        None => json!({"dir": dir}),
    }
}

/// The drive a block device (`/dev/nvme0n1p3`) is on, as the system says: its model, whether it
/// caches writes, and whether it takes a write through its cache (FUA).
pub fn drive(dev: &str) -> Value {
    let name = dev.rsplit('/').next().unwrap_or(dev);
    // A partition's drive is the directory it sits in.
    let real = std::fs::canonicalize(format!("/sys/class/block/{name}")).ok();
    let whole = match real {
        Some(r) if std::path::Path::new(&format!("/sys/class/block/{name}/partition")).exists() => r.parent().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()),
        _ => Some(name.to_owned()),
    };
    let Some(whole) = whole else { return Value::Null };
    let read = |f: &str| std::fs::read_to_string(format!("/sys/class/block/{whole}/{f}")).ok().map(|v| v.trim().to_owned());
    json!({"name": whole, "model": read("device/model"), "write_cache": read("queue/write_cache"), "fua": read("queue/fua")})
}

/// Percentiles of `xs` (sorted here), in milliseconds.
pub fn summary(xs: Vec<Duration>) -> Value {
    summary_ms(xs.into_iter().map(|d| d.as_secs_f64() * 1000.0).collect())
}

/// Percentiles of `xs`, milliseconds (sorted here; signed: a difference may be below 0).
pub fn summary_ms(mut xs: Vec<f64>) -> Value {
    xs.sort_by(f64::total_cmp);
    let n = xs.len();
    let at = |p: f64| {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
        let i = ((p * n as f64).ceil() as usize).saturating_sub(1);
        xs.get(i.min(n.saturating_sub(1))).copied()
    };
    #[allow(clippy::cast_precision_loss)]
    let mean = if n == 0 { None } else { Some(xs.iter().sum::<f64>() / n as f64) };
    json!({"count": n, "mean_ms": mean, "p50_ms": at(0.50), "p90_ms": at(0.90), "p99_ms": at(0.99), "p999_ms": at(0.999), "max_ms": xs.last()})
}
