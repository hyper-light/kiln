//! The rate mode's OS interface, and the lab's only `unsafe`: the poller each generator thread waits in
//! (`polling`'s: epoll on Linux, kqueue on macOS, IOCP on Windows), and what a generator thread asks of its OS so
//! it sends on time (Linux's timer slack, macOS's QoS class), the open-files limit its connections and the servers
//! it starts live under, and the CPU clocks it reports its own cost by.
#![allow(unsafe_code)]

use std::io;
use std::net::TcpStream;
use std::time::Duration;

use polling::{Event, Events, PollMode, Poller};

/// A generator thread's sockets, each watched by its poller under its key. A socket is never dropped while the
/// poller watches it (the one thing `Poller::add` asks of its callers): the sockets live only here, and leave
/// only through [`Sockets::take`] or this being dropped, each of which takes the socket out of the poller first.
pub struct Sockets {
    poller: Poller,
    slots: Vec<Option<TcpStream>>,
    writing: Vec<bool>,
    mode: PollMode,
}

impl Sockets {
    pub fn new() -> io::Result<Self> {
        let poller = Poller::new()?;
        // Level-triggered where the poller has it (Linux, macOS): no system call to watch a socket again after
        // each event. Elsewhere one-shot, re-armed after each event ([`Sockets::rearm`]).
        let mode = if poller.supports_level() { PollMode::Level } else { PollMode::Oneshot };
        Ok(Self { poller, slots: Vec::new(), writing: Vec::new(), mode })
    }

    fn interest(key: usize, write: bool) -> Event {
        if write { Event::all(key) } else { Event::readable(key) }
    }

    /// Puts `s` (non-blocking) in under `key`, watched for reading; a socket already there is let go first.
    pub fn put(&mut self, key: usize, s: TcpStream) -> io::Result<()> {
        drop(self.take(key));
        while self.slots.len() <= key {
            self.slots.push(None);
            self.writing.push(false);
        }
        let Some(slot) = self.slots.get_mut(key) else { return Err(io::Error::other("no slot for the socket")) };
        // SAFETY: `s` goes into `slot` right after, and leaves it only through `take` or `drop`, each of which
        // deletes it from this poller first: it is never dropped while the poller watches it. If `add_with_mode`
        // fails, nothing watches `s` and it's dropped here.
        unsafe { self.poller.add_with_mode(&s, Self::interest(key, false), self.mode)? };
        *slot = Some(s);
        if let Some(w) = self.writing.get_mut(key) {
            *w = false;
        }
        Ok(())
    }

    /// The socket under `key`, out of the poller.
    pub fn take(&mut self, key: usize) -> Option<TcpStream> {
        let s = self.slots.get_mut(key)?.take()?;
        let _ = self.poller.delete(&s);
        Some(s)
    }

    pub fn get(&self, key: usize) -> Option<&TcpStream> {
        self.slots.get(key)?.as_ref()
    }

    /// Watches `key` for room to write too (what it sent didn't all go), or no longer.
    pub fn want_write(&mut self, key: usize, write: bool) -> io::Result<()> {
        let Some(w) = self.writing.get_mut(key) else { return Ok(()) };
        if *w == write {
            return Ok(());
        }
        *w = write;
        match self.slots.get(key) {
            Some(Some(s)) => self.poller.modify_with_mode(s, Self::interest(key, write), self.mode),
            _ => Ok(()),
        }
    }

    /// Watches `key` again after an event, where the poller is one-shot.
    pub fn rearm(&self, key: usize) -> io::Result<()> {
        if self.mode != PollMode::Oneshot {
            return Ok(());
        }
        match (self.slots.get(key), self.writing.get(key)) {
            (Some(Some(s)), Some(w)) => self.poller.modify_with_mode(s, Self::interest(key, *w), self.mode),
            _ => Ok(()),
        }
    }

    /// Waits up to `timeout` for events, into `events` (emptied first).
    pub fn wait(&self, events: &mut Events, timeout: Duration) -> io::Result<usize> {
        events.clear();
        self.poller.wait(events, Some(timeout))
    }
}

impl Drop for Sockets {
    fn drop(&mut self) {
        for s in self.slots.iter_mut().filter_map(Option::take) {
            let _ = self.poller.delete(&s);
        }
    }
}

/// Makes the calling generator thread as punctual as its OS lets it, so it sends when the schedule says and its
/// lateness isn't charged to the server: on Linux, timer slack of 1 ns (the default 50 µs lets every timed wake
/// come up to that late: prctl(2), PR_SET_TIMERSLACK); on macOS, the user-interactive QoS class (the performance
/// cores, and the least timer coalescing: Apple's Energy Efficiency Guide for Mac Apps, "Prioritize Work at the
/// Task Level", as kiln's own `kiln_rt` sets its classes). What it got, in a few words.
pub fn punctual() -> &'static str {
    #[cfg(target_os = "linux")]
    {
        match rustix::thread::set_current_timer_slack(std::num::NonZeroU64::new(1)) {
            Ok(()) => "timer slack 1 ns",
            Err(_) => "timer slack refused",
        }
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: no pointer passes; the call changes only the calling thread's QoS class, to a class
        // `sys/qos.h` defines, at no relative priority (pthread/qos.h).
        let code = unsafe { libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0) };
        if code == 0 { "QoS user-interactive" } else { "QoS refused" }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        "as the OS gave"
    }
}

/// The open-files limit kiln's own service runs with: its systemd unit's `LimitNOFILE` on Linux, its launchd
/// job's `NumberOfFiles` on macOS (`crates/kiln/src/install.rs`). The lab raises its own soft limit toward it, so
/// its connections fit and the servers it starts (which inherit it) run under production's.
pub const SERVICE_FILES: u64 = if cfg!(target_os = "macos") { 10_240 } else { 1_048_576 };

/// Raises this process's soft open-files limit toward `want`, as far as its hard limit lets it: the soft limit
/// then (`u64::MAX`: none), or none where the OS has no such limit.
pub fn open_files(want: u64) -> Option<u64> {
    #[cfg(unix)]
    {
        use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
        let now = getrlimit(Resource::Nofile);
        let current = now.current.unwrap_or(u64::MAX);
        if current >= want {
            return Some(current);
        }
        let target = now.maximum.map_or(want, |m| m.min(want));
        match setrlimit(Resource::Nofile, Rlimit { current: Some(target), maximum: now.maximum }) {
            Ok(()) => Some(target),
            Err(_) => Some(current),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = want;
        None
    }
}

#[cfg(unix)]
fn ns(t: rustix::time::Timespec) -> Option<u64> {
    let secs = u64::try_from(t.tv_sec).ok()?;
    let nanos = u64::try_from(t.tv_nsec).ok()?;
    secs.checked_mul(1_000_000_000)?.checked_add(nanos)
}

/// The calling thread's time on the CPU, ns: none where the OS won't say (Windows here).
pub fn thread_cpu_ns() -> Option<u64> {
    #[cfg(unix)]
    {
        ns(rustix::time::clock_gettime(rustix::time::ClockId::ThreadCPUTime))
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// This process's time on the CPU, every thread's, ns: none where the OS won't say.
pub fn process_cpu_ns() -> Option<u64> {
    #[cfg(unix)]
    {
        ns(rustix::time::clock_gettime(rustix::time::ClockId::ProcessCPUTime))
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// The OS's clock ticks a second, which Linux's `/proc` counts CPU time in (USER_HZ).
#[cfg(unix)]
pub fn clock_ticks() -> u64 {
    rustix::param::clock_ticks_per_second()
}

/// The memory page's size, which Linux's `/proc` counts resident memory in.
#[cfg(unix)]
pub fn page_size() -> u64 {
    rustix::param::page_size() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Goal 10 (Ada's fixed-rate benchmark): a socket is watched, said readable when its peer writes, and taken
    /// back out; the CPU clocks move as a thread works.
    #[test]
    fn rate_sockets_are_watched_and_let_go() -> Result<(), String> {
        let e = |e: io::Error| e.to_string();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(e)?;
        let client = TcpStream::connect(listener.local_addr().map_err(e)?).map_err(e)?;
        let (mut server, _) = listener.accept().map_err(e)?;
        client.set_nonblocking(true).map_err(e)?;
        let mut sockets = Sockets::new().map_err(e)?;
        sockets.put(3, client).map_err(e)?;
        let mut events = Events::new();
        assert_eq!(sockets.wait(&mut events, Duration::from_millis(10)).ok(), Some(0), "nothing to read yet");
        io::Write::write_all(&mut server, b"hi").map_err(e)?;
        assert_eq!(sockets.wait(&mut events, Duration::from_secs(5)).ok(), Some(1));
        assert!(events.iter().all(|e| e.key == 3 && e.readable));
        assert!(sockets.rearm(3).is_ok() && sockets.want_write(3, true).is_ok() && sockets.want_write(3, false).is_ok());
        assert!(sockets.take(3).is_some() && sockets.take(3).is_none() && sockets.get(3).is_none());
        let before = thread_cpu_ns();
        let mut x = 0u64;
        for i in 0..2_000_000u64 {
            x = std::hint::black_box(x.wrapping_mul(31).wrapping_add(i));
        }
        assert!(cfg!(not(unix)) || thread_cpu_ns() > before, "the thread's clock moved");
        assert!(cfg!(not(unix)) || process_cpu_ns().is_some());
        let _ = punctual();
        Ok(())
    }
}
