//! The lease seam's contract as cases every implementation passes (CLAUDE.md §9; docs/design/credential-grants.md C3):
//! the simulator in this crate's tests, and every orchestrator's lessors (kiln's over its run-lease broker). A case
//! drives the lessors through the seam alone and its fixture's store; each reads "do X, expect Y", and [`run`] returns
//! the cases that failed and why.

use super::{LeaseRefused, Lessor, Lessors};
use crate::BoxId;

/// What a case runs against: the lessors under test, and the store they lease from as its fixture keeps it.
pub trait Fixture {
    fn lessors(&self) -> &dyn Lessors;
    /// Credential `reference` holds `value` in the store, with no end of its own.
    fn hold(&self, reference: &str, value: &[u8]) -> Result<(), String>;
    /// Whether lease `lease` is held in the store: given, and not given back.
    fn holds(&self, lease: &str) -> Result<bool, String>;
}

/// One part of the contract.
pub struct Case {
    pub id: &'static str,
    pub says: &'static str,
    run: fn(&dyn Fixture) -> Result<(), String>,
}

/// A case that failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub case: &'static str,
    pub why: String,
}

pub const CASES: &[Case] = &[
    Case {
        id: "HEX-LEASE-1",
        says: "a run's lease of a credential its store holds gives the value held, under a name of the lease's own that is not the value, living until its run ends where the store gives it no end of its own, and holds it",
        run: leases,
    },
    Case {
        id: "HEX-LEASE-2",
        says: "a lease of a credential its store holds none of is refused Denied, and holds nothing",
        run: denies,
    },
    Case {
        id: "HEX-LEASE-3",
        says: "a lease given back is held no more, alone: the run's other leases and another run's are held on, another run's lessor gives back none of it, and the run leases anew after",
        run: gives_back,
    },
    Case {
        id: "HEX-LEASE-4",
        says: "a lease and its value never show the value in their Debug",
        run: redacts,
    },
];

/// Runs every case; returns the failures.
pub fn run(fx: &dyn Fixture) -> Vec<Failure> {
    CASES
        .iter()
        .filter_map(|c| (c.run)(fx).err().map(|why| Failure { case: c.id, why }))
        .collect()
}

fn ensure(ok: bool, why: impl FnOnce() -> String) -> Result<(), String> {
    if ok { Ok(()) } else { Err(why()) }
}

/// A run's lessor, for box `name` (`hex-conformance-lease-<name>`).
fn lessor(fx: &dyn Fixture, name: &str) -> Result<std::sync::Arc<dyn Lessor>, String> {
    let id = BoxId::new(&format!("hex-conformance-lease-{name}")).map_err(|e| e.to_string())?;
    fx.lessors().lessor(&id).map_err(|e| e.to_string())
}

/// The value every case's credential holds: plainly not a lease's name.
const VALUE: &[u8] = b"conformance-value-0123456789";

/// HEX-LEASE-1 (D242): do lease a credential the store holds, with no end of its own. Expect its value, a lease name
/// that isn't the value, a life until the run ends, and the lease held.
fn leases(fx: &dyn Fixture) -> Result<(), String> {
    fx.hold("conformance/one", VALUE)?;
    let run = lessor(fx, "one")?;
    let lease = run
        .lease("conformance/one", None)
        .map_err(|e| format!("a held credential: {e}"))?;
    ensure(lease.value.expose() == VALUE, || {
        "the value leased isn't the value held".to_owned()
    })?;
    ensure(
        lease.lease.as_bytes() != VALUE && !lease.lease.is_empty(),
        || "the lease's name is its value, or none".to_owned(),
    )?;
    ensure(lease.life.is_none(), || {
        format!(
            "a value with no end of its own lives {:?}, not until its run ends",
            lease.life
        )
    })?;
    ensure(fx.holds(&lease.lease)?, || {
        "the lease given isn't held".to_owned()
    })?;
    run.release(&lease.lease);
    Ok(())
}

/// HEX-LEASE-2 (D242): do lease a credential the store holds none of. Expect `Denied`.
fn denies(fx: &dyn Fixture) -> Result<(), String> {
    let run = lessor(fx, "two")?;
    match run.lease("conformance/none", None) {
        Err(LeaseRefused::Denied(_)) => Ok(()),
        Err(other) => Err(format!(
            "a credential the store holds none of: refused {other:?}, not Denied"
        )),
        Ok(lease) => {
            run.release(&lease.lease);
            Err("a credential the store holds none of was leased".to_owned())
        }
    }
}

/// HEX-LEASE-3 (D242): do give back one of a run's two leases, and have another run's lessor give back the other.
/// Expect the first held no more, the second and another run's held on, and a new lease after.
fn gives_back(fx: &dyn Fixture) -> Result<(), String> {
    fx.hold("conformance/three", VALUE)?;
    let (run, other) = (lessor(fx, "three")?, lessor(fx, "three-other")?);
    let lease = |l: &dyn Lessor| {
        l.lease("conformance/three", None)
            .map_err(|e| format!("a held credential: {e}"))
    };
    let (one, two, theirs) = (lease(&*run)?, lease(&*run)?, lease(&*other)?);
    run.release(&one.lease);
    ensure(!fx.holds(&one.lease)?, || {
        "a lease given back is held".to_owned()
    })?;
    ensure(fx.holds(&two.lease)? && fx.holds(&theirs.lease)?, || {
        "giving one lease back gave back another".to_owned()
    })?;
    other.release(&two.lease);
    ensure(fx.holds(&two.lease)?, || {
        "another run's lessor gave this run's lease back".to_owned()
    })?;
    let again = lease(&*run)?;
    ensure(fx.holds(&again.lease)?, || {
        "the run leases no more after giving one back".to_owned()
    })?;
    for (l, given) in [(&run, &two), (&run, &again), (&other, &theirs)] {
        l.release(&given.lease);
    }
    Ok(())
}

/// HEX-LEASE-4 (D242): do lease a credential and print the lease and its value. Expect neither to show it.
fn redacts(fx: &dyn Fixture) -> Result<(), String> {
    fx.hold("conformance/four", VALUE)?;
    let run = lessor(fx, "four")?;
    let lease = run
        .lease("conformance/four", None)
        .map_err(|e| format!("a held credential: {e}"))?;
    let shown = format!("{lease:?} {:?}", lease.value);
    let text = String::from_utf8_lossy(VALUE);
    run.release(&lease.lease);
    ensure(!shown.contains(&*text), || {
        "a lease's Debug shows its value".to_owned()
    })
}
