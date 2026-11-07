//! The lab's stand-ins for the two streams a lab kiln has nothing to fill (plan 01a1a832e9cc step
//! 3; research f4): a terminal's shell and a CI job's step. Each writes lines stamped with the
//! microsecond it wrote them (`@lab:<µs>`, as the agents' stand-in stamps its events), so the page
//! can time each from its writing to its parsing, at a fixed pace: `per_s` lines a second, each
//! `bytes` long with its line end.
//!
//! - `kiln-lab-term` (the lab kiln's `[terminal] shell`): as long as its terminal is open, at
//!   `KILN_LAB_TERM` (`per_s:bytes`, 10 lines of 96 bytes a second unless set);
//! - `kiln-lab-log <secs> <per_s> <bytes>` (the lab repository's job's step).

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::standin::{STAMP, now_us};

/// A stamped line `bytes` long, ending `end`.
pub fn line(n: u64, bytes: usize, end: &str) -> String {
    let mut l = format!("{STAMP}{:016} line {n} ", now_us());
    let fill = bytes.saturating_sub(l.len() + end.len());
    l.extend(std::iter::repeat_n('x', fill));
    l.push_str(end);
    l
}

fn write_lines(secs: Option<u64>, per_s: f64, bytes: usize, end: &str) -> anyhow::Result<()> {
    let start = Instant::now();
    let mut out = std::io::stdout().lock();
    for n in 0u64.. {
        #[allow(clippy::cast_precision_loss)]
        let due = start + Duration::from_secs_f64(n as f64 / per_s.max(0.01));
        if secs.is_some_and(|s| due > start + Duration::from_secs(s)) {
            break;
        }
        let now = Instant::now();
        if due > now {
            std::thread::sleep(due - now);
        }
        // A closed terminal or log ends it.
        if out.write_all(line(n, bytes, end).as_bytes()).and_then(|()| out.flush()).is_err() {
            break;
        }
    }
    Ok(())
}

/// Run as one of the stand-ins (by the name it was started as): whether it was one.
pub fn main(me: &str, args: &[String]) -> Option<anyhow::Result<()>> {
    match me {
        "kiln-lab-term" => {
            let spec = std::env::var("KILN_LAB_TERM").unwrap_or_default();
            let (per_s, bytes) = spec.split_once(':').map_or((10.0, 96), |(a, b)| (a.parse().unwrap_or(10.0), b.parse().unwrap_or(96)));
            Some(write_lines(None, per_s, bytes, "\r\n"))
        }
        "kiln-lab-log" => {
            let n = |i: usize, d: f64| args.get(i).and_then(|a| a.parse::<f64>().ok()).unwrap_or(d);
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            Some(write_lines(Some(n(0, 600.0) as u64), n(1, 10.0), n(2, 96.0) as usize, "\n"))
        }
        _ => None,
    }
}

/// The stand-ins in `bin` (copies of this program under their names), and the agents' `claude`.
pub fn install(bin: &Path) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(bin)?;
    let me = std::env::current_exe()?;
    for name in ["claude", "kiln-lab-term", "kiln-lab-log"] {
        let to = bin.join(name);
        if !to.exists() {
            std::fs::copy(&me, &to)?;
        }
    }
    Ok(bin.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn g1_s3_a_stamped_line_is_its_size() {
        let l = line(7, 96, "\r\n");
        assert_eq!(l.len(), 96);
        assert!(l.starts_with("@lab:") && l.ends_with("\r\n"));
        assert_eq!(l.get(5..21).map(|d| d.bytes().all(|b| b.is_ascii_digit())), Some(true));
    }
}
