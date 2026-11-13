//! [`SimLessors`]: the lease seam's simulator (CLAUDE.md §9). Values in memory; each run's lessor its own, made as its
//! run boots; each lease held until its run gives it back, by its lessor alone.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use super::{Lease, LeaseRefused, Lessor, Lessors, Value};
use crate::{BoxId, Refusal};

/// Most leases a [`SimLessors`] holds at once, every run's together; past it a lease is refused `Unavailable`, as a
/// broker out of room says it (the simulator's own bound, as [`crate::sim::SIM_BOXES`] is the runtime simulator's).
pub const SIM_LEASES: usize = 10_000;

/// Lessors in memory, over values the test holds ([`SimLessors::hold`]).
#[derive(Clone, Default)]
pub struct SimLessors {
    state: Arc<Mutex<State>>,
}

#[derive(Default)]
struct State {
    values: BTreeMap<String, Value>,
    /// Each lease held, by its name: the run it was given to, and the credential it leases.
    held: BTreeMap<String, (u64, String)>,
    /// Leases given so far: each one's number is in its name.
    made: u64,
    /// Runs given a lessor so far: each one's number names it.
    runs: u64,
    /// Leases given back so far.
    given_back: u64,
}

impl SimLessors {
    pub fn new() -> Self {
        Self::default()
    }

    fn state(&self) -> Result<MutexGuard<'_, State>, Refusal> {
        self.state
            .lock()
            .map_err(|_| Refusal::Broken("the lease simulator's lock is poisoned".to_owned()))
    }

    /// Credential `reference` holds `value` from now, with no end of its own.
    pub fn hold(&self, reference: &str, value: &[u8]) -> Result<(), Refusal> {
        self.state()?
            .values
            .insert(reference.to_owned(), Value::new(value.to_vec()));
        Ok(())
    }

    /// Whether lease `lease` is held: given, and not given back.
    pub fn holds(&self, lease: &str) -> Result<bool, Refusal> {
        Ok(self.state()?.held.contains_key(lease))
    }

    /// How many leases are held, every run's together.
    pub fn held(&self) -> Result<usize, Refusal> {
        Ok(self.state()?.held.len())
    }

    /// How many leases were given back so far.
    pub fn given_back(&self) -> Result<u64, Refusal> {
        Ok(self.state()?.given_back)
    }
}

impl Lessors for SimLessors {
    fn lessor(&self, _id: &BoxId) -> Result<Arc<dyn Lessor>, Refusal> {
        let mut state = self.state()?;
        state.runs = state.runs.checked_add(1).ok_or(Refusal::Exhausted {
            what: "simulated runs",
            bound: usize::MAX,
        })?;
        Ok(Arc::new(SimLessor {
            state: Arc::clone(&self.state),
            run: state.runs,
        }))
    }
}

/// One run's lessor in the simulator. A key the run tells changes nothing here: the simulator's leases are its run's
/// alone, by its lessor.
struct SimLessor {
    state: Arc<Mutex<State>>,
    run: u64,
}

impl Lessor for SimLessor {
    fn lease(&self, reference: &str, _key: Option<[u8; 32]>) -> Result<Lease, LeaseRefused> {
        let mut state = self.state.lock().map_err(|_| {
            LeaseRefused::Unavailable("the lease simulator's lock is poisoned".to_owned())
        })?;
        let value = state
            .values
            .get(reference)
            .map(|v| v.expose().to_vec())
            .ok_or_else(|| LeaseRefused::Denied(format!("the simulator holds no {reference:?}")))?;
        let value = Value::new(value);
        if state.held.len() >= SIM_LEASES {
            return Err(LeaseRefused::Unavailable(format!(
                "the simulator holds its most leases ({SIM_LEASES})"
            )));
        }
        state.made = state.made.checked_add(1).ok_or_else(|| {
            LeaseRefused::Unavailable("the simulator has given its most leases".to_owned())
        })?;
        let lease = format!("sim-lease-{}", state.made);
        state
            .held
            .insert(lease.clone(), (self.run, reference.to_owned()));
        Ok(Lease {
            lease,
            value,
            life: None,
        })
    }

    fn release(&self, lease: &str) {
        // A poisoned lock leaves the lease held: the simulator's state is gone with the panic that poisoned it.
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if state
            .held
            .get(lease)
            .is_some_and(|(run, _)| *run == self.run)
        {
            state.held.remove(lease);
            // Never past the leases made, themselves a u64.
            state.given_back = state.given_back.saturating_add(1);
        }
    }
}
