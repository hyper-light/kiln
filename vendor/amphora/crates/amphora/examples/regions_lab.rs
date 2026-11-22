//! AC-T1.11.2: the simulated 3-region lab (docs/design/regions.md). Three regions of three
//! members, region-local groups (three voters each) under a global root that survives a region,
//! a corpus of regional and global puts and gets from clients in every region; prints the
//! regional and global commit-time quantiles. The round-trip times are the lab's input, not
//! Amphora's: the 60 ms run is
//!
//! `cargo run --release -p amphora --example regions_lab -- <within-region RTT µs> 60000 <ops> <keys> <groups> <seed>`
//!
//! with the within-region round trip measured on the lab's network; `keys` (the corpus's key
//! space) and `groups` (each directory's) are the corpus's and the lab's shape, also inputs.

use std::io::Write;
use std::process::ExitCode;
use std::time::Duration;

use amphora::regions::Region;
use amphora::regions::lab::{Lab, Latency, Op, Shape, quantile};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            // A closed stderr leaves only the status to say it failed.
            let _ = writeln!(std::io::stderr(), "regions_lab: {why}");
            ExitCode::from(1)
        }
    }
}

const USAGE: &str = "usage: regions_lab <within-region RTT µs> <between-region RTT µs> <ops> <keys> <groups> <seed>";

fn arg(n: usize) -> Result<u64, String> {
    std::env::args_os()
        .nth(n)
        .ok_or(USAGE)?
        .to_str()
        .ok_or(USAGE)?
        .parse()
        .map_err(|_| USAGE.to_owned())
}

/// SplitMix64 (Steele, Lea and Flood, OOPSLA 2014): the corpus's ops from its seed.
fn splitmix(s: &mut u64) -> u64 {
    *s = s.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *s;
    z = (z ^ z.wrapping_shr(30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ z.wrapping_shr(27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ z.wrapping_shr(31)
}

fn run() -> Result<(), String> {
    let within = Duration::from_micros(arg(1)?);
    let between = Duration::from_micros(arg(2)?);
    let ops = usize::try_from(arg(3)?).map_err(|_| USAGE.to_owned())?;
    let keys = arg(4)?.max(1);
    let groups = usize::try_from(arg(5)?).map_err(|_| USAGE.to_owned())?;
    let mut seed = arg(6)?;
    let regions: Vec<Region> = ["a", "b", "c"].map(|r| Region(r.to_owned())).to_vec();
    let shape = Shape {
        members: regions.iter().map(|r| (r.clone(), 3)).collect(),
        homes: regions.clone(),
        groups,
        survive: 1,
        survive_regions: 1,
    };
    let mut lab = Lab::new(&shape, Latency { within, between }).map_err(|e| e.to_string())?;
    let corpus: Vec<Op> = (0..ops)
        .map(|_| {
            let x = splitmix(&mut seed);
            let client = usize::try_from(x % 9).unwrap_or(0);
            let key = format!("k{}", x.wrapping_shr(8).checked_rem(keys).unwrap_or(0));
            let global = x.wrapping_shr(16).is_multiple_of(5);
            if x.wrapping_shr(24).is_multiple_of(3) {
                Op::Get {
                    client,
                    key,
                    global,
                }
            } else {
                Op::Put {
                    client,
                    key,
                    value: x.wrapping_shr(32),
                    global,
                }
            }
        })
        .collect();
    let out = lab.run(&corpus).map_err(|e| e.to_string())?;
    let mut stdout = std::io::stdout();
    for (class, samples) in [
        ("regional", &out.regional_commits),
        ("global", &out.global_commits),
    ] {
        let q = |q| quantile(samples, q).map_or(f64::NAN, |d| d.as_secs_f64() * 1e3);
        writeln!(
            stdout,
            "{class} commits={} p50_ms={:.3} p99_ms={:.3} p999_ms={:.3}",
            samples.len(),
            q(500),
            q(990),
            q(999)
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}
