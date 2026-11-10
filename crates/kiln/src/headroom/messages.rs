//! Whether messages get through to agents (docs/headroom.md, "Agent messages"): a target kiln adds
//! itself on every runner, for every harness alike (Claude Code, Codex, ACP agents, Antigravity,
//! whatever comes next), measured on what the harnesses are handed rather than probed. The harness
//! layer says, after everything a harness says or is told, what of each session waits to be
//! delivered (`crate::harness::kinds::Adapter`): a message written to its harness and not yet taken
//! (its delivery `Sent`), or one in kiln's queue while its harness is starting or idle. Each is timed
//! from when it began waiting to when it stops; what still waits counts as a probe out, so a harness
//! that's stopped, starved or never came up alarms while it is, not once it recovers.
//!
//! Nothing here is a number of its own: the target's tolerance is a person's flow of thought
//! (`super::settings::DEFAULT_TOLERANCE_MS`), as for every target that declares none, and what's calm and what's an alarm the detector learns from this
//! machine's own record, against the runner's false-alarm budget.
//!
//! And headroom itself never stands in the way: a session with something waiting is a person's for
//! as long as it waits (never held, never paced), and the pacer is woken at once, so a harness
//! stopped mid-period is let go the moment something waits for it.
//!
//! Why: on 2026-11-05 a Mac's runner held all its agents' processes stopped, their harnesses
//! included, so nothing anyone sent them arrived; and once something outside kiln took the CPU,
//! harnesses sat in "starting" for minutes, waiting on their own credential lookups.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;
use std::sync::atomic::Ordering;

use parking_lot::Mutex;

/// The target's name, in the status and the logs.
pub const TARGET: &str = "agent messages";

#[derive(Default)]
struct State {
    /// What each session's harness was handed and hasn't taken, by key: when (µs since the epoch).
    waiting: HashMap<String, HashMap<String, u64>>,
    /// How long each thing taken since the engine last looked took (µs).
    took: Vec<f64>,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| Mutex::new(State::default()));

// W20-R01: adapter tests observe completions while a running headroom engine drains samples.
// Their counter must survive that drain; the production samples still cover only one look.
#[cfg(test)]
static ENDED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Whether this process runs a runner's headroom (else nothing is kept).
fn on() -> bool {
    cfg!(test) || super::SHARED.running.load(Ordering::Acquire)
}

/// What of session `session` waits to be delivered now, by message id (all of it, each time): what
/// begins waiting is timed from now, what stops waiting is measured, and the session runs unpaced
/// while anything waits. Cheap when nothing changed.
pub fn waiting(session: &str, ids: &[&str]) {
    if !on() || session.is_empty() {
        return;
    }
    let mut began = false;
    {
        let mut s = STATE.lock();
        let now = super::now_us();
        let State { waiting, took } = &mut *s;
        match waiting.get_mut(session) {
            None if ids.is_empty() => return,
            None => {
                waiting.insert(session.to_owned(), ids.iter().map(|id| ((*id).to_owned(), now)).collect());
                began = true;
            }
            Some(w) => {
                w.retain(|id, at| {
                    let still = ids.contains(&id.as_str());
                    if !still {
                        took.push(now.saturating_sub(*at) as f64);
                        #[cfg(test)]
                        ENDED.fetch_add(1, Ordering::Relaxed);
                    }
                    still
                });
                for id in ids {
                    if !w.contains_key(*id) {
                        w.insert((*id).to_owned(), now);
                    }
                }
                if w.is_empty() {
                    waiting.remove(session);
                }
            }
        }
    }
    if began {
        // Let it go at once if the pacer holds it stopped.
        super::kick();
    }
}

/// Session `session`'s harness is gone (it exited, or the session ended): what waited for it isn't
/// measured (it waits for the next harness, or for nothing).
pub fn gone(session: &str) {
    if on() {
        STATE.lock().waiting.remove(session);
    }
}

/// What the engine reads at a look (at `now`, µs): how long each thing taken since the last look
/// took, how long the oldest thing still untaken has waited (none: nothing waits), and the sessions
/// with something waiting.
pub(super) fn drain(now: u64) -> (Vec<f64>, Option<f64>, HashSet<String>) {
    let mut s = STATE.lock();
    let took = std::mem::take(&mut s.took);
    let oldest = s.waiting.values().flat_map(|w| w.values()).min().map(|at| now.saturating_sub(*at) as f64);
    let sessions = s.waiting.keys().cloned().collect();
    (took, oldest, sessions)
}

/// What of session `session` waits now, by id (for tests).
#[cfg(test)]
pub fn waits(session: &str) -> Vec<String> {
    let mut w: Vec<String> = STATE.lock().waiting.get(session).map(|w| w.keys().cloned().collect()).unwrap_or_default();
    w.sort();
    w
}

/// How many waits have ended in this test process, including samples the engine drained.
#[cfg(test)]
pub fn ended() -> usize {
    ENDED.load(Ordering::Relaxed)
}
