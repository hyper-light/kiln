//! One change of a box at a time. A change takes the box's turn and holds it until it's done: another
//! change of the same box waits for it, a change of another box never does (goal 5 plan step 2), a
//! wait is bounded by a learned patience and follows the holder past it, and the turns taken at once stop
//! at a stated bound, refused by type past it (CLAUDE.md §2). The record store takes a turn for each change
//! of a record; the node takes one for each create and delete of a box, which has no record while it's
//! being made (goal 5 extended, step 5).
//!
//! The patience is the longest a turn has been held in this process (decision 9; Ada's rules of 2026-11-06 and
//! 2026-11-13; [`hex_core::patience`]): each turn let go is a sample of what a wait for one waits on. Before the first
//! is let go, and from a hold that outlasts it (a run-out, told to the census, no sample), a wait follows the holder:
//! it lasts until the turn is let go, the holder being this process's own thread, alive while the node is. A run-out
//! refuses nothing: a live holder is waited for, and only a stall of its measured progress may refuse a wait, typed
//! (a hold's progress is owed: `docs/research/runtime/learned-waits.md`'s sites). No record is read.

use std::sync::PoisonError;
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(test)]
use std::time::Duration;
use std::time::Instant;

use hex_api::{BoxId, DeviceId, Refusal};
use hex_core::patience::{Patience, Piece};
use hex_core::record::{OwnerKey, OwnerRef};
use hex_wait::{Condvar, Mutex};

/// The boxes whose turns are taken now, and what a refusal of one names.
#[derive(Debug)]
pub struct Turns {
    held: Mutex<Held>,
    /// Told whenever a box is let go.
    free: Condvar,
    /// Turns that waited for one of the same box to end.
    waited: AtomicU64,
    /// What a turn is for, as a refusal of an overlapping one names it.
    op: &'static str,
    /// The turns taken at once, as a refusal past the bound names them.
    what: &'static str,
    bound: usize,
}

/// The turns taken, and the wait for one learned from how long turns were held, under one lock.
#[derive(Debug)]
struct Held {
    /// Each box whose turn is taken, with when its holder took it.
    /// Sorted by identity; each addition fallibly reserves before inserting. BTreeMap::insert
    /// cannot report allocation failure on stable Rust (R01).
    boxes: Vec<HeldBox>,
    patience: Patience,
}

#[derive(Debug)]
struct HeldBox {
    id: OwnerKey,
    since: Instant,
    /// Ephemeral exclusion in this same method owner, never a physical holder ledger.
    devices: Vec<DeviceId>,
}

impl Held {
    fn position(&self, id: OwnerRef<'_>) -> Result<usize, usize> {
        self.boxes
            .binary_search_by(|held| held.id.as_ref().cmp(&id))
    }

    fn insert(
        &mut self,
        id: OwnerRef<'_>,
        devices: Vec<DeviceId>,
        since: Instant,
        what: &'static str,
        bound: usize,
    ) -> Result<OwnerKey, Refusal> {
        let at = match self.position(id) {
            Err(at) => at,
            Ok(_) => {
                return Err(Refusal::Broken(
                    "a turn already owns this identity".to_owned(),
                ));
            }
        };
        if at > self.boxes.len() {
            return Err(Refusal::Broken(
                "a turn's insertion position is invalid".to_owned(),
            ));
        }
        let entry = id.try_owned()?;
        let receipt = id.try_owned()?;
        self.boxes
            .try_reserve(1)
            .map_err(|_| Refusal::Exhausted { what, bound })?;
        // binary_search's vacant position is checked above; the reservation covers this one entry.
        self.boxes.insert(
            at,
            HeldBox {
                id: entry,
                since,
                devices,
            },
        );
        Ok(receipt)
    }
}

/// Box `id`'s turn, given up when dropped.
pub struct Turn<'a> {
    turns: &'a Turns,
    id: OwnerKey,
    since: Instant,
}

/// A non-clonable method receipt held by an asynchronous caller's existing Arc owner.
pub(crate) struct Hold {
    id: OwnerKey,
    since: Instant,
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        self.turns.release_parts(self.id.as_ref(), self.since);
    }
}

impl Turns {
    pub(crate) fn release(&self, hold: Hold) {
        self.release_parts(hold.id.as_ref(), hold.since);
    }

    fn release_parts(&self, id: OwnerRef<'_>, since: Instant) {
        // The set stays whole whoever held its lock (nothing panics holding it: CLAUDE.md §1), so a
        // poisoned lock still lets the box go.
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        held.boxes
            .retain(|held| held.id.as_ref() != id || held.since != since);
        held.patience.answered(since.elapsed());
        drop(held);
        self.free.notify_all();
    }
}

impl Turns {
    /// Takes method-lifetime ownership without waiting. A native operation can complete on a
    /// callback that needs this box again, so its callers must refuse overlap rather than wait on
    /// themselves or retain a map lock across the native call.
    #[cfg(test)]
    pub(crate) fn try_take(&self, id: &BoxId) -> Result<Turn<'_>, Refusal> {
        self.try_take_devices(id, &[])
    }

    pub(crate) fn try_take_devices(
        &self,
        id: &BoxId,
        devices: &[DeviceId],
    ) -> Result<Turn<'_>, Refusal> {
        let hold = self.try_hold_devices(id, devices)?;
        Ok(Turn {
            turns: self,
            id: hold.id,
            since: hold.since,
        })
    }

    pub(crate) fn try_hold_devices(
        &self,
        id: &BoxId,
        devices: &[DeviceId],
    ) -> Result<Hold, Refusal> {
        self.try_hold_owner(OwnerRef::Box(id), devices, || match id.try_clone() {
            Ok(id) => Refusal::Lifecycle {
                id,
                cause: hex_api::LifecycleCause::Busy,
                why: None,
            },
            Err(error) => error,
        })
    }

    pub(crate) fn try_take_pool(
        &self,
        key: &hex_api::BackingKey,
        devices: &[DeviceId],
    ) -> Result<Turn<'_>, Refusal> {
        let hold =
            self.try_hold_owner(OwnerRef::Pool(&key.pool), devices, || Refusal::Backing {
                key: *key,
                cause: hex_api::LifecycleCause::Busy,
                why: None,
            })?;
        Ok(Turn {
            turns: self,
            id: hold.id,
            since: hold.since,
        })
    }

    /// One handoff owns the source configuration before the consuming Box and all full-P
    /// conflict keys. Only the Box turn carries device exclusions, avoiding a self-conflict.
    /// Nonblocking acquisition drops the source turn on every failed second acquisition.
    pub(crate) fn try_take_handoff(
        &self,
        id: &BoxId,
        source: &hex_api::BackingKey,
        full_conflicts: &[DeviceId],
    ) -> Result<(Turn<'_>, Turn<'_>), Refusal> {
        let pool = self.try_take_pool(source, &[])?;
        let boxed = self.try_take_devices(id, full_conflicts)?;
        Ok((pool, boxed))
    }

    pub(crate) fn try_take_pool_configuration(
        &self,
        pool: &[u8; 32],
        devices: &[DeviceId],
    ) -> Result<Turn<'_>, Refusal> {
        let hold = self.try_hold_owner(OwnerRef::Pool(pool), devices, || {
            let message = "native backing preparation conflicts with an active owner";
            let mut why = String::new();
            if why.try_reserve_exact(message.len()).is_err() {
                return Refusal::Exhausted {
                    what: "backing owner refusal bytes",
                    bound: message.len(),
                };
            }
            why.push_str(message);
            Refusal::Os { op: self.op, why }
        })?;
        Ok(Turn {
            turns: self,
            id: hold.id,
            since: hold.since,
        })
    }

    fn try_hold_owner(
        &self,
        id: OwnerRef<'_>,
        devices: &[DeviceId],
        busy: impl FnOnce() -> Refusal,
    ) -> Result<Hold, Refusal> {
        if devices.len() > hex_api::MAX_DEVICES {
            return Err(Refusal::Exhausted {
                what: "method device exclusions",
                bound: hex_api::MAX_DEVICES,
            });
        }
        let mut selected = Vec::new();
        selected
            .try_reserve_exact(devices.len())
            .map_err(|_| Refusal::Exhausted {
                what: "method device exclusions",
                bound: hex_api::MAX_DEVICES,
            })?;
        for device in devices {
            device.check()?;
            selected.push(*device);
        }
        selected.sort_unstable();
        selected.dedup();
        let mut held = self.held.lock().map_err(|e| self.poisoned(e))?;
        if held.position(id).is_ok()
            || held.boxes.iter().any(|active| {
                active
                    .devices
                    .iter()
                    .any(|device| selected.binary_search(device).is_ok())
            })
        {
            return Err(busy());
        }
        if held.boxes.len() >= self.bound {
            return Err(Refusal::Exhausted {
                what: self.what,
                bound: self.bound,
            });
        }
        let since = Instant::now();
        let owned = held.insert(id, selected, since, self.what, self.bound)?;
        Ok(Hold { id: owned, since })
    }

    /// Turns for `op`, a wait for one seeded as `patience` and learned from the turns held, at most `bound` of
    /// them (`what`) taken at once.
    pub fn new(op: &'static str, what: &'static str, bound: usize, patience: Patience) -> Self {
        Self {
            held: Mutex::new(Held {
                boxes: Vec::new(),
                patience,
            }),
            free: Condvar::new(),
            waited: AtomicU64::new(0),
            op,
            what,
            bound,
        }
    }

    /// Box `id`'s turn: waits for a turn of the same box under way, never for another box's, and is
    /// refused past the bound of turns taken at once. The wait is for the turn under way: it is bounded by
    /// the patience past the time its holder took it, and starts again for each holder after, since each
    /// turn let go is the answer it waited on (several changes of one box at once each wait for the one
    /// before theirs). A hold past the patience is in a tail no hold seen bounds (decision 9's run-out): the
    /// census is told, no sample, and the wait follows the holder until it lets go, as before any hold is seen.
    pub fn take(&self, id: &BoxId) -> Result<Turn<'_>, Refusal> {
        self.take_owner(OwnerRef::Box(id))
    }

    /// The pool's turn, waited for as [`Turns::take`] waits.
    pub(crate) fn take_pool(&self, key: &hex_api::BackingKey) -> Result<Turn<'_>, Refusal> {
        self.take_owner(OwnerRef::Pool(&key.pool))
    }

    /// The pool's turn by its configuration's identity, before its first preparation gives it a generation (no
    /// BackingKey yet), waited for as [`Turns::take`] waits.
    pub(crate) fn take_pool_configuration(&self, pool: &[u8; 32]) -> Result<Turn<'_>, Refusal> {
        self.take_owner(OwnerRef::Pool(pool))
    }

    fn take_owner(&self, id: OwnerRef<'_>) -> Result<Turn<'_>, Refusal> {
        let mut held = self.held.lock().map_err(|e| self.poisoned(e))?;
        if held.position(id).is_ok() {
            self.waited.fetch_add(1, Ordering::Relaxed);
        }
        // The hold this wait follows since it outlasted the patience, by when its holder took it: its own, whatever
        // another box's answer meanwhile tells the shared patience.
        let mut following = None;
        while let Some(since) = held
            .position(id)
            .ok()
            .and_then(|at| held.boxes.get(at).map(|held| held.since))
        {
            let patience = if following == Some(since) {
                None
            } else {
                held.patience.deadline()
            };
            held = match patience.and_then(|p| since.checked_add(p)) {
                // Following the holder, this process's own thread, until it lets go: no turn has been let go yet, or
                // this hold, or one before it since the last let-go, outlasted the patience.
                None => self.free.wait(held).map_err(|e| self.wait_refused(e))?,
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        // The hold outlasted the slowest seen: no sample (Karn's point), but the census is told, and
                        // the holder is followed to its let-go, the uncensored answer the patience then rises to
                        // (decision 9: a live party is waited for; its hold's progress, to refuse a stall, is owed).
                        if let Some(bound) = patience {
                            let pool: String;
                            let (kind, name) = match id {
                                OwnerRef::Box(id) => ("box ", id.as_str()),
                                OwnerRef::Pool(key) => {
                                    pool = key.iter().map(|byte| format!("{byte:02x}")).collect();
                                    ("pool ", pool.as_str())
                                }
                            };
                            held.patience.ran_out(
                                bound,
                                since.elapsed(),
                                &[
                                    Piece::Kind("the holder of "),
                                    Piece::Kind(kind),
                                    Piece::Name(name),
                                    Piece::Kind("'s turn"),
                                ],
                            );
                        }
                        following = Some(since);
                        continue;
                    }
                    self.free
                        .wait_timeout(held, left)
                        .map_err(|e| self.wait_refused(e))?
                        .0
                }
            };
        }
        if held.boxes.len() >= self.bound {
            return Err(Refusal::Exhausted {
                what: self.what,
                bound: self.bound,
            });
        }
        let since = Instant::now();
        let owned = held.insert(id, Vec::new(), since, self.what, self.bound)?;
        Ok(Turn {
            turns: self,
            id: owned,
            since,
        })
    }

    /// How many turns have waited for one of the same box to end.
    pub fn waited(&self) -> u64 {
        self.waited.load(Ordering::Relaxed)
    }

    /// How long a wait for a turn lasts now, as learned (`None` before any sample): what the tests read.
    #[cfg(test)]
    fn patience(&self) -> Option<Duration> {
        self.held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .patience
            .deadline()
    }

    fn poisoned<T>(&self, _: PoisonError<T>) -> Refusal {
        Refusal::Broken(format!("the lock on {} was poisoned", self.what))
    }

    /// A wait for a turn that couldn't wait: its lock poisoned, or the OS refused it (macOS's kqueue).
    fn wait_refused(&self, why: hex_wait::Failed) -> Refusal {
        match why {
            hex_wait::Failed::Poisoned => {
                Refusal::Broken(format!("the lock on {} was poisoned", self.what))
            }
            hex_wait::Failed::Os(why) => Refusal::Os {
                op: self.op,
                why: format!("waiting for a turn: {why}"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hex_testkit::waits::follow::{self, Party};

    /// A thread a test spawned, as the party of a wait on what it makes: ended once it has finished.
    fn spawned<T>(thread: &std::thread::ScopedJoinHandle<'_, T>) -> Party {
        if thread.is_finished() {
            Party::Ended
        } else {
            Party::Live
        }
    }

    fn id(n: &str) -> BoxId {
        BoxId::new(n).unwrap()
    }

    /// AC-T7.4.1: pool generations and ordinary boxes share one physical exclusion owner.
    #[test]
    fn pool_and_box_turns_exclude_the_same_native_devices_and_generations() {
        let turns = Turns::new("method", "methods", 3, Patience::measuring("test.pool"));
        let key = hex_api::BackingKey {
            pool: [1; 32],
            generation: [2; 16],
        };
        let next = hex_api::BackingKey {
            pool: key.pool,
            generation: [3; 16],
        };
        let gpu = DeviceId {
            kind: hex_api::DeviceKind::Gpu,
            key: 7,
        };
        let held = turns.try_take_pool(&key, &[gpu]).unwrap();
        assert!(turns.try_take_pool(&next, &[]).is_err());
        assert!(turns.try_take_devices(&id("box"), &[gpu]).is_err());
        let unrelated = turns.try_take_devices(&id("cpu"), &[]).unwrap();
        drop(held);
        let boxed = turns.try_take_devices(&id("box"), &[gpu]).unwrap();
        assert!(turns.try_take_pool(&next, &[gpu]).is_err());
        drop(boxed);
        assert!(turns.try_take_pool(&next, &[gpu]).is_ok());
        drop(unrelated);
    }

    /// CLAUDE.md §2 (T-T7.1.2): the turns taken at once stop at their bound, refused by type past it. Do: turns
    /// bounded at two; take two boxes' turns, then a third's; let one go and take the third's again. Expect:
    /// `Exhausted` naming the bound, then the third's turn given.
    #[test]
    fn turns_past_the_bound_are_refused_by_type() {
        let turns = Turns::new(
            "op",
            "turns under way",
            2,
            Patience::measuring("test.turns"),
        );
        let a = turns.take(&id("a")).unwrap();
        let _b = turns.take(&id("b")).unwrap();
        assert_eq!(
            turns.take(&id("c")).err(),
            Some(Refusal::Exhausted {
                what: "turns under way",
                bound: 2
            })
        );
        drop(a);
        assert!(turns.take(&id("c")).is_ok());
    }

    /// CLAUDE.md §2 (T-T7.1.2), decision 9: a wait for the same box's turn past the slowest hold seen refuses nothing:
    /// the hold is a run-out (no sample, Karn's point; the run-out told), and the wait follows the holder, this
    /// process's own thread, until it lets go, that hold the uncensored answer the patience rises to (HEX-SOCK-6: the
    /// changes of one box made at once all land). Do: take box a's turn and let it go, so the turns have seen that hold;
    /// hold it again while another thread takes it, until that take has run out (the turns following, with no bound);
    /// then let go. Expect: the take given the turn, the wait counted once, and a bound after it of at least the hold it
    /// followed.
    #[test]
    fn a_wait_past_the_slowest_hold_follows_the_holder_and_learns_its_hold() {
        let turns = Turns::new(
            "op",
            "turns under way",
            2,
            Patience::measuring("test.turns"),
        );
        drop(turns.take(&id("a")).unwrap());
        let held = turns.take(&id("a")).unwrap();
        let took = Instant::now();
        let hold = std::thread::scope(|s| {
            let waiter = s.spawn(|| turns.take(&id("a")).map(drop));
            // A look: the turns tell no event as a take runs out, their patience read.
            let waits = Patience::measuring("test.turns.take_runs_out");
            follow::until(
                &waits,
                "the second take running out",
                || spawned(&waiter),
                || turns.patience().is_none(),
            )
            .unwrap();
            let hold = took.elapsed();
            drop(held);
            waiter.join().unwrap().unwrap();
            hold
        });
        assert_eq!(turns.waited(), 1);
        let bound = turns.patience();
        assert!(
            bound.is_some_and(|b| b >= hold),
            "the hold followed bounds the next wait: {bound:?}, held at least {hold:?}"
        );
    }

    /// C-1 (decision 9; AC-T7.1.3): a hold past W* is told where a person sees it, the node's log (its stderr), once,
    /// naming the wait, its W*, how long it waited and its party, the holder it follows, with no census installed, as a
    /// node runs. Do: in a process of its own, take box a's turn and let it go (W*, that hold, learned), take it again
    /// and hold it while another thread's take waits until it has run out, then let go. Expect: one `hex.wait` line on
    /// the process's stderr for the wait, its party the holder of box a's turn.
    #[test]
    fn a_hold_past_w_star_is_told_once_on_the_nodes_log_naming_its_holder() {
        const CHILD: &str = "HEX_TEST_TURNS_TOLD_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let turns = Turns::new(
                "op",
                "turns under way",
                2,
                Patience::measuring("test.turns.told"),
            );
            drop(turns.take(&id("a")).unwrap());
            let held = turns.take(&id("a")).unwrap();
            std::thread::scope(|s| {
                let waiter = s.spawn(|| turns.take(&id("a")).map(drop));
                let waits = Patience::measuring("test.turns.told.runs_out");
                follow::until(
                    &waits,
                    "the second take running out",
                    || spawned(&waiter),
                    || turns.patience().is_none(),
                )
                .unwrap();
                drop(held);
                waiter.join().unwrap().unwrap();
            });
            return;
        }
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "turns::tests::a_hold_past_w_star_is_told_once_on_the_nodes_log_naming_its_holder",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "the child failed: {stderr}");
        let told: Vec<&str> = stderr
            .lines()
            .filter(|line| line.contains("wait=test.turns.told "))
            .collect();
        assert!(
            matches!(
                told.as_slice(),
                [line] if line.starts_with("hex.wait event=ran_out wait=test.turns.told w_star_ns=")
                    && line.contains(" waited_ns=")
                    && line.ends_with(" party=\"the holder of box a's turn\"")
            ),
            "the run-out told once on stderr, naming its holder: {told:?}; stderr: {stderr}"
        );
    }

    /// AC-T7.4.1/T-T7.1.2: overlap refuses without waiting or taking another box's ownership.
    #[test]
    fn nonblocking_method_ownership_refuses_overlap_and_releases_exact_owner() {
        let turns = Turns::new(
            "method",
            "methods",
            2,
            Patience::seeded("test.method", Duration::ZERO),
        );
        let a = turns.try_take(&id("a")).unwrap();
        assert!(matches!(
            turns.try_take(&id("a")),
            Err(Refusal::Lifecycle {
                cause: hex_api::LifecycleCause::Busy,
                ..
            })
        ));
        let b = turns.try_take(&id("b")).unwrap();
        assert!(matches!(
            turns.try_take(&id("c")),
            Err(Refusal::Exhausted { bound: 2, .. })
        ));
        assert_eq!(turns.waited(), 0);
        drop(a);
        let a = turns.try_take(&id("a")).unwrap();
        assert!(turns.try_take(&id("b")).is_err());
        drop((a, b));
        assert!(turns.try_take(&id("c")).is_ok());
    }

    /// AC-T7.4.1: host and guest methods exclude the same physical keys in the same owner.
    #[test]
    fn device_exclusions_share_the_existing_method_owner_and_async_receipts() {
        use hex_api::DeviceKind;
        let a = DeviceId {
            kind: DeviceKind::Gpu,
            key: 1,
        };
        let b = DeviceId {
            kind: DeviceKind::Gpu,
            key: 2,
        };
        let turns = Turns::new("method", "methods", 3, Patience::measuring("test.devices"));
        let first = turns.try_take_devices(&id("host"), &[b, a, a]).unwrap();
        assert!(matches!(
            turns.try_take_devices(&id("guest"), &[a]),
            Err(Refusal::Lifecycle {
                cause: hex_api::LifecycleCause::Busy,
                ..
            })
        ));
        let independent = turns.try_take(&id("cpu")).unwrap();
        drop(first);
        let callback = turns.try_hold_devices(&id("guest"), &[a]).unwrap();
        assert!(turns.try_take_devices(&id("host"), &[a]).is_err());
        assert!(turns.try_take(&id("guest")).is_err());
        turns.release(callback);
        assert!(turns.try_take_devices(&id("host"), &[a]).is_ok());
        drop(independent);
        assert_eq!(turns.waited(), 0);
    }

    /// Decision 9's cold start (T-T7.1.2): before any turn has been let go, a wait for a turn has no bound and follows
    /// its holder until it lets go, and the hold it waited on then bounds the next. Do: on new turns, hold box a's turn
    /// while another thread takes it, until that take waits, then let go. Expect: no bound before; the waiting take given
    /// the turn; then a bound of at least the hold, the slowest seen.
    #[test]
    fn a_cold_wait_follows_the_holder_until_it_lets_go_and_learns_the_hold() {
        let turns = Turns::new(
            "op",
            "turns under way",
            2,
            Patience::measuring("test.turns"),
        );
        assert_eq!(turns.patience(), None);
        let held = turns.take(&id("a")).unwrap();
        let took = Instant::now();
        let hold = std::thread::scope(|s| {
            let waiter = s.spawn(|| turns.take(&id("a")).map(drop));
            // A look: the turns tell no event as a take starts waiting, their count read.
            let waits = Patience::measuring("test.turns.second_take_waits");
            follow::until(
                &waits,
                "the second take waiting",
                || spawned(&waiter),
                || turns.waited() > 0,
            )
            .unwrap();
            let hold = took.elapsed();
            drop(held);
            waiter.join().unwrap().unwrap();
            hold
        });
        let bound = turns.patience();
        assert!(
            bound.is_some_and(|b| b >= hold),
            "the hold waited on bounds the next wait: {bound:?}, held at least {hold:?}"
        );
    }
}

#[cfg(test)]
mod handoff_controls {
    use super::*;
    use hex_api::{BackingKey, DeviceKind};

    /// AC-T7.4.1: the selected S may be smaller, but a handoff holds the entire canonical P and
    /// source configuration. A failed acquisition cannot leave either half owned.
    #[test]
    fn handoff_turn_holds_full_source_and_releases_both_on_failure_or_completion() {
        let turns = Turns::new(
            "handoff",
            "test owners",
            8,
            Patience::measuring("test.handoff"),
        );
        let source = BackingKey {
            pool: [1; 32],
            generation: [2; 16],
        };
        let id = BoxId::new("consumer").unwrap();
        let sibling = BoxId::new("sibling").unwrap();
        let p = [
            DeviceId {
                kind: DeviceKind::Gpu,
                key: 3,
            },
            DeviceId {
                kind: DeviceKind::Gpu,
                key: 5,
            },
        ];
        let held = turns.try_take_handoff(&id, &source, &p).unwrap();
        assert!(turns.try_take_pool(&source, &[]).is_err());
        assert!(turns.try_take_devices(&sibling, &p[1..]).is_err());
        assert!(turns.try_take_devices(&id, &[]).is_err());
        drop(held);
        let sibling_turn = turns.try_take_devices(&sibling, &p[1..]).unwrap();
        assert!(turns.try_take_handoff(&id, &source, &p).is_err());
        let source_turn = turns.try_take_pool(&source, &[]).unwrap();
        drop(source_turn);
        drop(sibling_turn);
        drop(turns.try_take_handoff(&id, &source, &p).unwrap());
        assert!(turns.held.lock().unwrap().boxes.is_empty());
        let bounded = Turns::new(
            "handoff",
            "one test owner",
            1,
            Patience::measuring("test.handoff"),
        );
        assert!(bounded.try_take_handoff(&id, &source, &p).is_err());
        assert!(bounded.held.lock().unwrap().boxes.is_empty());
    }
}
