//! Amphora: where the versioned storage's memory comes from (G7 step 2, `AC-NS.2.1`;
//! `docs/design/engine.md`, decision 13). The storage holds no share of its own: it asks a
//! [`Source`] for each piece it grows by (a run of pool frames, a step of its row versions'
//! reservation) and gives it back as it lets it go. The shell's source is its process memory
//! budget (`amphora::memory`, MB.1), which every database of the process shares and which grants a
//! use what the rest of the process leaves it, by measured use: no fraction is picked. Without
//! one ([`ConnectOpts::with_memory`](crate::ConnectOpts::with_memory)) every storage of the process
//! shares [`machine`]: the machine's memory less what those storages hold.
//!
//! The system's interfaces here need `unsafe` (CLAUDE.md §12 names this file): `sysconf` on Unix,
//! `GlobalMemoryStatusEx` on Windows; elsewhere a typed refusal naming the platform.

use crate::error::LimboError;
use crate::sync::atomic::{AtomicU64, Ordering};
use crate::sync::{Arc, OnceLock};
use crate::Result;

/// Where a versioned storage takes its memory from, shared by every storage given it.
pub trait Source: Send + Sync {
    /// `bytes` more held for `what`, refused typed ([`LimboError::MemoryBudget`]) when the room
    /// the source has for the storage now is less.
    fn take(&self, what: &'static str, bytes: u64) -> Result<()>;
    /// `bytes` taken given back.
    fn give(&self, bytes: u64);
}

/// The source every storage opened without one shares: the machine's memory, read again at every
/// take, less what the storages hold. A process with a budget of its own gives that instead.
pub struct Machine {
    held: AtomicU64,
}

impl Machine {
    /// A source over the machine's memory with nothing held.
    pub fn new() -> Self {
        Self {
            held: AtomicU64::new(0),
        }
    }

    /// The bytes held now.
    pub fn held(&self) -> u64 {
        self.held.load(Ordering::Acquire)
    }
}

impl Default for Machine {
    fn default() -> Self {
        Self::new()
    }
}

impl Source for Machine {
    fn take(&self, what: &'static str, bytes: u64) -> Result<()> {
        let limit = machine_bytes()?;
        self.held
            .try_update(Ordering::AcqRel, Ordering::Acquire, |held| {
                held.checked_add(bytes).filter(|after| *after <= limit)
            })
            .map(|_| ())
            .map_err(|held| LimboError::MemoryBudget {
                what,
                asked: bytes,
                held,
                limit,
            })
    }

    fn give(&self, bytes: u64) {
        let _ = self
            .held
            .try_update(Ordering::AcqRel, Ordering::Acquire, |held| {
                Some(held.saturating_sub(bytes))
            });
    }
}

/// The source every storage of this process opened without one shares.
pub fn machine() -> Arc<dyn Source> {
    static MACHINE: OnceLock<Arc<Machine>> = OnceLock::new();
    let m: Arc<Machine> = MACHINE.get_or_init(|| Arc::new(Machine::new())).clone();
    m
}

/// The machine's physical memory in bytes: its pages times their size.
#[cfg(unix)]
#[expect(
    unsafe_code,
    reason = "sysconf is libc's; CLAUDE.md §12 names this file"
)]
pub fn machine_bytes() -> Result<u64> {
    // SAFETY: sysconf reads a system value by name; it takes no pointer and touches no memory of
    // ours. A name the system doesn't know returns -1, which the conversion below refuses.
    let (pages, size) = unsafe {
        (
            libc::sysconf(libc::_SC_PHYS_PAGES),
            libc::sysconf(libc::_SC_PAGESIZE),
        )
    };
    let pages = u64::try_from(pages).ok();
    let size = u64::try_from(size).ok();
    pages
        .zip(size)
        .and_then(|(p, s)| p.checked_mul(s))
        .filter(|&n| n > 0)
        .ok_or_else(|| {
            LimboError::InternalError(
                "versioned storage: the system doesn't say its memory (sysconf)".into(),
            )
        })
}

/// The machine's physical memory in bytes.
#[cfg(windows)]
#[expect(
    unsafe_code,
    reason = "GlobalMemoryStatusEx is Win32's; CLAUDE.md §12 names this file"
)]
pub fn machine_bytes() -> Result<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut status = MEMORYSTATUSEX {
        dwLength: u32::try_from(std::mem::size_of::<MEMORYSTATUSEX>())
            .map_err(|_| LimboError::IntegerOverflow)?,
        dwMemoryLoad: 0,
        ullTotalPhys: 0,
        ullAvailPhys: 0,
        ullTotalPageFile: 0,
        ullAvailPageFile: 0,
        ullTotalVirtual: 0,
        ullAvailVirtual: 0,
        ullAvailExtendedVirtual: 0,
    };
    // SAFETY: `status` is a live MEMORYSTATUSEX whose dwLength is its size, as the call requires;
    // the call writes only within it and keeps no pointer to it.
    let ok = unsafe { GlobalMemoryStatusEx(&mut status) };
    if ok == 0 || status.ullTotalPhys == 0 {
        return Err(LimboError::InternalError(
            "versioned storage: the system doesn't say its memory (GlobalMemoryStatusEx)".into(),
        ));
    }
    Ok(status.ullTotalPhys)
}

/// Neither Unix nor Windows: the shell gives the budget.
#[cfg(not(any(unix, windows)))]
pub fn machine_bytes() -> Result<u64> {
    Err(LimboError::InvalidArgument(format!(
        "versioned storage: the machine's memory isn't known on {}; give a budget (ConnectOpts::with_memory_budget)",
        std::env::consts::OS
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T-NS.2.10: take from the machine's source up to its memory, past it, and give back.
    /// Expect: the machine's memory read; a take within it held, one past what is left refused
    /// typed naming the use, and what is given back taken again (no share picked).
    #[test]
    fn ns2_the_machines_source_grants_what_the_storages_leave() {
        let machine = machine_bytes();
        assert!(matches!(machine, Ok(n) if n > 0));
        let total = machine.unwrap_or(0);
        let source = Machine::new();
        source.take("a test's frames", total / 2).unwrap();
        assert_eq!(source.held(), total / 2);
        let refused = source.take("a test's versions", total - total / 2 + 1);
        assert!(
            matches!(refused, Err(LimboError::MemoryBudget { what, .. }) if what == "a test's versions")
        );
        source.give(total / 2);
        assert_eq!(source.held(), 0);
        source.take("a test's versions", total).unwrap();
        source.give(total);
    }
}
