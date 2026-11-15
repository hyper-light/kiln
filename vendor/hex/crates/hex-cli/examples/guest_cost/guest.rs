//! The Shards guest the environment configures, as its node reads it (`GuestConfig`): what a run measures (its images
//! and its size) and its envelope, alike for every OS's driver; and a stopped guest's end, followed.

use std::io::Read as _;
use std::path::Path;

use hex_api::CapacityResources;
use hex_runtime::guest::GuestConfig;
use sha2::{Digest, Sha256};

use super::seam::{Envelope, Lines};

/// The SHA-256 of the file at `path`, read in SHA-256's own block multiple.
pub fn digest(path: &Path) -> Result<String, String> {
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buffer)
            .map_err(|e| format!("reading {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hash.update(buffer.get(..n).ok_or("a read past its buffer")?);
    }
    Ok(super::run::hex(&hash.finalize()))
}

/// What a run of `config` measures: its `shards`, its kernel and its initrd, each with its SHA-256, its vCPUs and its
/// memory.
pub fn describe(config: &GuestConfig) -> Result<Lines, String> {
    let mut lines = Vec::new();
    let (kernel, initrd) = config.direct().map_err(|e| e.to_string())?;
    for (name, path) in [
        ("SHARDS_BIN", config.binary.as_path()),
        ("HEX_GUEST_KERNEL", kernel),
        ("HEX_GUEST_INITRD", initrd),
    ] {
        lines.push((
            name.to_owned(),
            format!("{} sha256={}", path.display(), digest(path)?),
        ));
    }
    lines.push(("HEX_GUEST_CPUS".to_owned(), config.vcpus.to_string()));
    lines.push((
        "HEX_GUEST_MEMORY_MIB".to_owned(),
        config.memory_mib.to_string(),
    ));
    Ok(lines)
}

/// `config`'s RAM, in bytes.
pub fn ram(config: &GuestConfig) -> Result<u64, String> {
    config
        .memory_mib
        .checked_mul(1 << 20)
        .ok_or_else(|| "the memory overflows".to_owned())
}

/// `config`'s envelope: its vCPUs' whole CPU and its RAM, its floor and its ceiling.
pub fn envelope(config: &GuestConfig) -> Result<Envelope, String> {
    Ok(Envelope {
        resources: CapacityResources {
            cpu_milli: u64::from(config.vcpus)
                .checked_mul(1000)
                .ok_or("the vCPUs overflow")?,
            mem_bytes: ram(config)?,
            disk_bytes: 0,
        },
        vcpus: config.vcpus,
        source: "HEX_GUEST_CPUS and HEX_GUEST_MEMORY_MIB; no disk: the guest's root and state are in its RAM (docs/design/shards-guest.md §4)".to_owned(),
    })
}

/// A stopped guest's end, each one's wait measuring (decision 9; the review's C-9): within W*, the slowest end this run
/// has seen, its answer; past it, its run-out told once and the wait following the guest's VM to its end, never given
/// up at a bound. No record seeds it.
static ENDED: hex_core::patience::Patience =
    hex_core::patience::Patience::measuring("guest_cost.ended");

/// Follows `vm`, stopped, to its end ([`ENDED`]): its own end answers the wait, which nothing else ends.
pub fn follow_end(vm: &dyn shards_api::Guest) -> Result<(), String> {
    hex_testkit::waits::follow::within(
        &ENDED,
        "the guest's end after its stop",
        || hex_testkit::waits::follow::Party::Live,
        |by| {
            let left = by.map_or(std::time::Duration::MAX, |at| {
                at.saturating_duration_since(std::time::Instant::now())
            });
            match vm.wait_for(left) {
                Ok(Some(_)) => Some(Ok(())),
                Ok(None) => None,
                Err(e) => Some(Err(format!("waiting for the guest's end: {e}"))),
            }
        },
    )
    .map_err(|unmet| unmet.to_string())?
}

/// The guest RAM's address range among a VMM's map entries (`(start, end, anonymous)`, in address order):
/// the one run of anonymous entries, each where the one before ends, whose span is the guest's RAM (Shards
/// maps each RAM range as one private anonymous mapping, `reserve_ram`, which XNU keeps as entries of at
/// most 128 MiB and Linux splits where its advice differs, and Windows commits as one private read/write
/// region). `None` while no run is that long (its RAM not yet mapped); an error where several are, as none is
/// then told apart.
pub fn guest_range(entries: &[(u64, u64, bool)], ram: u64) -> Result<Option<(u64, u64)>, String> {
    let mut found = Vec::new();
    for (at, &(start, _, anonymous)) in entries.iter().enumerate() {
        if !anonymous {
            continue;
        }
        let mut end = start;
        for &(next, next_end, next_anonymous) in entries.iter().skip(at) {
            if !next_anonymous || next != end {
                break;
            }
            end = next_end;
            let span = end.saturating_sub(start);
            if span == ram {
                found.push((start, end));
            }
            if span >= ram {
                break;
            }
        }
    }
    match found.as_slice() {
        [] => Ok(None),
        [range] => Ok(Some(*range)),
        runs => Err(format!(
            "{} runs of the VMM's anonymous memory span its guest's RAM ({ram} bytes), at {runs:x?}: none is told apart",
            runs.len()
        )),
    }
}
