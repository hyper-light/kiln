//! A quiet sleep: a sleep that wakes a parked shard only after a stretch with no wake at all, its quiet (kiln's departure,
//! ORIGIN.md; kiln's goal 9, plan 01a1a87b2cec step 5: a shard's delay probe, Ada's word of 2026-11-06).
//!
//! **Why.** Kiln's shards each ran a delay probe on a timer every 17.07 ms: a parked shard woke for nothing but to time
//! its own wake, 58.6 times a second a shard, 234 of the ~457 timer wakes a second of kiln's lab (g9p16), each a park
//! and a wake the shard's busy share counts. A shard that real work woke within the span has a wake to time already.
//!
//! **What it does.** A task waits in a quiet sleep of `span`, with `quiet` its quiet: due `span` after its first poll.
//! Each account it gives is timed to the poll that takes it.
//! - Awake when it falls due (running its own work, or spinning): it ends at the next step, as a timer would, saying how
//!   late it ran from its due time ([`Came::Running`]).
//! - Parked when it falls due: the park doesn't end for it until `quiet` after the park began ([`deadline`]), so a shard
//!   that real work keeps waking is never woken for it. It ends at the step after the shard's next wake, saying how long
//!   the work that woke the shard waited to run, from its start where the shard knows it ([`Came::Woken`]: a kick's, from
//!   its sender's stamp; another timer's, from its deadline; none for a socket's or a completion's).
//! - Parked `quiet` with no wake at all: its own deadline wakes the shard, and it says how late, from that deadline
//!   ([`Came::Quiet`]).
//!
//! One per shard: a second is refused while one waits. From its first the shard times its kicks
//! ([`crate::parking::Parking::stamp_wakes`]): each sender reads its clock as it kicks a parked shard, and the shard
//! its own three times a park that waits (no system call where the OS's clock is a page's, as on Linux).

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::RtError;
use crate::registry;
use crate::waker::polling_task;

/// What ended a park ([`Wake`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum By {
    /// A kick from another thread.
    Kick,
    /// The driver's own work: a socket's readiness, a completion.
    Io,
    /// A timer's deadline.
    Timer,
    /// A quiet sleep's own deadline ([`deadline`]).
    Quiet,
}

/// A park's end as the loop saw it: when its wait returned, what ended it, and how long that waited to run where the
/// shard knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Wake {
    /// The shard's clock as its wait returned.
    pub(crate) at_ns: u64,
    pub(crate) by: By,
    /// A kick's: its sender's stamp to the wait's return ([`kick_wait`]); a timer's: its deadline to the wait's return.
    /// Both to `at_ns`, which the rest of a turn is counted from ([`to_poll`]).
    pub(crate) waited_ns: Option<u64>,
}

/// How a quiet sleep's turn came ([`sleep_quiet`]), each timed to the poll that took it, in nanoseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Came {
    /// The shard was awake when the sleep fell due: from its due time.
    Running(u64),
    /// The shard was parked when the sleep fell due and its next wake came from its own work: from that work's start,
    /// where the shard knows it (a kick's sender's stamp; another timer's deadline); none for a socket's readiness or a
    /// completion's.
    Woken(Option<u64>),
    /// The shard was parked its quiet with no wake at all, and the sleep's own deadline woke it: from that deadline.
    Quiet(u64),
}

/// A task waiting in a quiet sleep, on its shard's desk.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Quiet {
    /// When it falls due, by the shard's clock.
    pub(crate) due_ns: u64,
    /// How long a parked shard goes with no wake before the sleep wakes it.
    pub(crate) quiet_ns: u64,
    /// Its task's word.
    pub(crate) word: u64,
    /// How its turn came, once it has, timed to the step that saw it ([`came`]), and when, by the shard's clock, the rest
    /// of it is counted from to the poll that takes it ([`to_poll`]).
    pub(crate) came: Option<(Came, u64)>,
    /// Where it was asked for: what its own wake of the shard is counted against (`crate::timer_sites`).
    pub(crate) site: Option<&'static std::panic::Location<'static>>,
}

/// When a park beginning at `now_ns` ends for `quiet`, a sleep that hasn't come yet: no earlier than it falls due, nor
/// than its quiet after the park began, so only a park that lasts its quiet is ended by it.
pub(crate) fn deadline(quiet: &Quiet, now_ns: u64) -> u64 {
    quiet.due_ns.max(now_ns.saturating_add(quiet.quiet_ns))
}

/// How `quiet`'s turn came, at the step after it fell due, from the shard's last wake (`last`): parked when it fell due
/// where that wake came at or after it, timed to the wake, which the rest is counted from; else awake, the whole of it
/// counted from its due time.
pub(crate) fn came(quiet: &Quiet, last: Option<Wake>) -> (Came, u64) {
    match last {
        Some(wake) if wake.at_ns >= quiet.due_ns => match wake.by {
            By::Quiet => (Came::Quiet(wake.waited_ns.unwrap_or(0)), wake.at_ns),
            By::Kick | By::Io | By::Timer => (Came::Woken(wake.waited_ns), wake.at_ns),
        },
        _ => (Came::Running(0), quiet.due_ns),
    }
}

/// How long the kick that ended a park (`woken`, timed on the shard's clock) waited: from its sender's stamp to
/// `woke_ns`, the loop's reading as the park's wait returned ([`Wake::at_ns`]), on its driver's clock, which is the
/// shard's less the driver's epoch, `epoch_ns` ([`crate::driver::Clock::Since`]). The rest of a turn is counted from
/// that same reading ([`to_poll`]), so each stretch is counted once: the park's own reading of the wait's return
/// (`Woken::returned_ns`, which its latency runs to) comes later, and counted to it the stretch between the two was
/// counted twice. None where the park measured no kick of its own ([`crate::parking::Woken::latency_ns`]).
pub(crate) fn kick_wait(woken: &crate::parking::Woken, woke_ns: u64, epoch_ns: u64) -> Option<u64> {
    let kicked = woken.kicked_ns?;
    woken.latency_ns().map(|_| woke_ns.saturating_add(epoch_ns).saturating_sub(kicked))
}

/// `came`, its time counted on from `from_ns` to `now_ns`, the poll that takes it (an unknown one stays unknown).
pub(crate) fn to_poll(came: Came, from_ns: u64, now_ns: u64) -> Came {
    let rest = now_ns.saturating_sub(from_ns);
    match came {
        Came::Running(ns) => Came::Running(ns.saturating_add(rest)),
        Came::Woken(ns) => Came::Woken(ns.map(|ns| ns.saturating_add(rest))),
        Came::Quiet(ns) => Came::Quiet(ns.saturating_add(rest)),
    }
}

/// Sleeps `span_ns` on the shard, quietly: a parked shard is woken for it only after `quiet_ns` with no wake at all (the
/// module notes). How its turn came is what it returns. Refused off a shard's thread, and while another quiet sleep waits
/// on the shard.
#[track_caller]
pub fn sleep_quiet(span_ns: u64, quiet_ns: u64) -> SleepQuiet {
    SleepQuiet { span_ns, quiet_ns, word: None, site: std::panic::Location::caller() }
}

/// The quiet sleep's future ([`sleep_quiet`]).
#[derive(Debug)]
pub struct SleepQuiet {
    span_ns: u64,
    quiet_ns: u64,
    /// Its task's word once it waits on the desk.
    word: Option<u64>,
    /// Where it was asked for.
    site: &'static std::panic::Location<'static>,
}

impl Future for SleepQuiet {
    type Output = Result<Came, RtError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let Some(task) = polling_task(cx.waker()) else {
            return Poll::Ready(Err(RtError::NotOnShardThread));
        };
        let word = task.word();
        let (span_ns, quiet_ns, waiting, site) = (self.span_ns, self.quiet_ns, self.word, self.site);
        let polled = registry::with_current(|ctx| match (ctx.quiet.get(), waiting) {
            // Its turn came: taken, timed to this poll, and the desk free for the next.
            (Some(q), Some(_)) if q.word == word && q.came.is_some() => {
                ctx.quiet.set(None);
                Ok(q.came.map(|(came, from_ns)| to_poll(came, from_ns, ctx.now_ns())))
            }
            (Some(q), Some(_)) if q.word == word => Ok(None),
            // Another task's waits.
            (Some(_), _) => Err(RtError::BadConfig {
                what: "a second quiet sleep on one shard",
            }),
            (None, _) => {
                let due_ns = ctx.now_ns().saturating_add(span_ns);
                ctx.quiet.set(Some(Quiet { due_ns, quiet_ns, word, came: None, site: Some(site) }));
                // The loop takes its due time into its own as it applies what this poll asked.
                ctx.asked.set(true);
                if let Some(entry) = ctx.entry {
                    entry.parking.stamp_wakes();
                }
                Ok(None)
            }
        });
        match polled {
            Some(Ok(Some(came))) => {
                self.word = None;
                Poll::Ready(Ok(came))
            }
            Some(Ok(None)) => {
                self.word = Some(word);
                Poll::Pending
            }
            Some(Err(refusal)) => Poll::Ready(Err(refusal)),
            None => Poll::Ready(Err(RtError::NotOnShardThread)),
        }
    }
}

impl Drop for SleepQuiet {
    /// A sleep dropped before its turn came frees the desk for the next.
    fn drop(&mut self) {
        if let Some(word) = self.word.take() {
            let _ = registry::with_current(|ctx| {
                if ctx.quiet.get().is_some_and(|q| q.word == word) {
                    ctx.quiet.set(None);
                }
            });
        }
    }
}

#[cfg(test)]
mod tests;
