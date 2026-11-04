//! A task's turn on its shard, by time, with no hand-placed yields (kiln's goal 6, plan 01a1a59c2abe step
//! 1; ORIGIN.md, "What differs from upstream").
//!
//! A task is charged as it uses its shard. Each operation of a leaf future is one charge: a socket's read,
//! write or accept, a timer, a channel's send or receive, a notification, a join, a readiness wait, a
//! signal or stdio read. Each charge weighs the time the task has held the shard since its poll began, or
//! since the shard last let it go on (`ShardContext::checked_ns`, docs/runtime.md §3.11), against its
//! share. Past its share, and with anything else wanting the shard ([`crate::shard::ShardContext::wanted`]:
//! a task listed, an intent, a wake or control message from another thread, a timer or the driver's harvest
//! due), the charge answers no: the leaf returns `Pending` with its task woken behind the loop's next
//! reading of its inputs (`crate::queue`), and every leaf the task polls before that poll ends answers the
//! same. So the task yields at its next await, as Tokio's leaves make theirs do [Tokio, "Reducing tail
//! latencies with automatic cooperative task yielding", 2020; `tokio::task::coop`]. Where nothing else
//! wants the shard the task goes on, and its hold counts from there, as [`crate::futures::maybe_yield`]
//! lets it.
//!
//! - **The share is the step quantum** (`ShardContext::quantum_ns`): the online wake estimate while the
//!   shard tracks one, else its configured step budget. It is the quantum a task that asks `wanted` once a
//!   quantum holds a wanted shard for (docs/runtime.md §3.10) and the batch's latency budget (§3.3), so a
//!   task charged this way never holds a wanted shard past twice it: it makes no long poll (§3.11).
//! - **Each charge reads the shard's clock** (its cheap reading, carried by the counter on aarch64, §3.9),
//!   except where nothing could want the shard, which needs no time. A count of charges between readings
//!   would bound nothing in time: a loop whose items turned from cheap to dear would run a count's worth of
//!   dear ones past its share.
//! - **Long computations** call [`consume_budget`] between their pieces: the same charge, alone.
//! - **A simulated shard never yields by budget**: its clock does not move within a poll, and its schedules
//!   stay a step a yield as they were (docs/runtime.md §11).
//! - **Only the task's own waker is refused.** A leaf polled under another waker (an executor nested in the
//!   task's poll, `now_or_never`'s, a combinator's own) is charged, so the time counts, but always goes on: a
//!   refusal there can't make the task yield, since what polls it would only poll it again at once, for as
//!   long as the task's poll lasts (a nested `block_on` spun its shard so in kiln's tests), and what comes at
//!   once (a bus that answers at once, a sleep already due) must still come at once. The task yields at its
//!   next leaf under its own waker.
//! - **Each poll makes at least one operation**: its first charge under its own waker goes on whatever the
//!   time, as Tokio's budget gives each poll its 128. A poll whose own work before its first leaf outlasts the
//!   share (futures' `read_to_end` zeroes its buffer's spare room again at every poll) would otherwise be
//!   refused there at every poll, redoing that work and getting no further: a livelock that spun a shard of
//!   kiln's tests.
//!
//! Tokio's budget counts operations, 128 a poll, and reads no clock; Seastar's task quota is a time (0.5 ms
//! by default) that a timer ends, checked between tasks with two loads (`need_preempt`). Here the count is
//! kept (`Counters::coop_charges`, the operations charged) and the share is a time, read at each charge.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::registry;
use crate::shard::ShardContext;

/// Charges the polling task one operation of a leaf future: `Ready` to go on with it, or `Pending` once the
/// task has held its shard past its share while anything else wants the shard, with the waker woken, so the
/// task is polled again after the loop has read its inputs. A leaf calls it first, so an answer of `Pending`
/// leaves its operation undone. `Ready` off a shard's thread, between polls (an owner's code) and under any
/// waker but the polling task's own (the module's notes).
pub fn poll_proceed(cx: &mut Context<'_>) -> Poll<()> {
    if registry::with_current(|here| proceeds(here, cx)).unwrap_or(true) {
        return Poll::Ready(());
    }
    cx.waker().wake_by_ref();
    Poll::Pending
}

/// [`poll_proceed`] on the shard `here` a leaf has looked up already: one lookup of the thread's shard
/// serves both.
pub(crate) fn proceed_in(here: &ShardContext, cx: &mut Context<'_>) -> Poll<()> {
    if proceeds(here, cx) {
        return Poll::Ready(());
    }
    cx.waker().wake_by_ref();
    Poll::Pending
}

/// The charge, and whether the leaf goes on: it does within the task's share, at the poll's first charge, and
/// under a waker other than the polling task's own, which no refusal would make yield.
fn proceeds(here: &ShardContext, cx: &Context<'_>) -> bool {
    here.charge(crate::waker::polling_task_in(here, cx.waker()).is_some())
}

/// The yield point of a long computation: one charge with no other operation (Tokio's `consume_budget`).
/// Ready at once while the task is within its share, or while nothing else wants its shard; past its share
/// with something else waiting, the task yields once and goes on at its next poll.
pub fn consume_budget() -> ConsumeBudget {
    ConsumeBudget { _private: () }
}

/// The future of [`consume_budget`].
#[derive(Debug)]
#[must_use = "a charge does nothing unless awaited"]
pub struct ConsumeBudget {
    _private: (),
}

impl Future for ConsumeBudget {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        poll_proceed(cx)
    }
}

/// Whether the polling task has used its share on a shard something else wants: the leaves it polls before
/// its poll ends answer `Pending` ([`poll_proceed`]). `false` off a shard's thread.
pub fn spent() -> bool {
    registry::with_current(|here| here.coop_spent.get()).unwrap_or(false)
}

/// A task's class on its shard (kiln's goal 6, plan 01a1a59c2abe step 3): which of the run queue's lists it
/// waits in. The shard gives each class with tasks waiting an equal share of its time, and of the classes that
/// have run alike, the most urgent goes first (`crate::queue::LocalQueue::pop`): Seastar's scheduling groups
/// give components shares of a shard the same way (research 01a1a59c2abf, f10), and Google tags each request
/// with a criticality its servers shed and serve by (f16). Equal shares, since no class's need is known to
/// outweigh another's: urgency decides the order, the shares the time, and none is starved. A task spawned by a
/// task takes its spawner's class; any other starts [`Class::Normal`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Class {
    /// Work that may wait without harm to a person: polling, catch-ups, sync, telemetry.
    Background = 0,
    /// The default: the API.
    #[default]
    Normal = 1,
    /// What a person waits on or the platform checks: health, sign-in, a person's pages and actions.
    Critical = 2,
}

impl Class {
    /// A word for it, as a metric's label says it.
    pub fn as_str(self) -> &'static str {
        match self {
            Class::Background => "background",
            Class::Normal => "normal",
            Class::Critical => "critical",
        }
    }
}

/// Sets the class of the task being polled: from its next listing it waits in that class's list, and its polls
/// count to that class's share; tasks it spawns later take it. The class it had; `None` off a task.
pub fn set_class(class: Class) -> Option<Class> {
    registry::with_current(|here| {
        let slot = here.current_slot()?;
        Some(here.local.set_class(slot, class))
    })
    .flatten()
}

/// The class of the task being polled; `None` off a task.
pub fn class() -> Option<Class> {
    registry::with_current(|here| here.current_slot().map(|slot| here.local.class_of(slot))).flatten()
}
