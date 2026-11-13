use super::conformance::{self, Fixture};
use super::sim::SimLessors;
use super::*;

/// The simulator's fixture: its own store.
struct Sim(SimLessors);

impl Fixture for Sim {
    fn lessors(&self) -> &dyn Lessors {
        &self.0
    }
    fn hold(&self, reference: &str, value: &[u8]) -> Result<(), String> {
        self.0.hold(reference, value).map_err(|e| e.to_string())
    }
    fn holds(&self, lease: &str) -> Result<bool, String> {
        self.0.holds(lease).map_err(|e| e.to_string())
    }
}

/// CLAUDE.md §9, joint plan step 33 (C3): do run the lease contract against the simulator. Expect every case to pass,
/// and every lease the cases gave given back.
#[test]
fn w33_the_simulated_lessors_pass_the_lease_contract() {
    let sim = SimLessors::new();
    let failures = conformance::run(&Sim(sim.clone()));
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(sim.held().unwrap(), 0, "a case left a lease held");
    assert_eq!(conformance::CASES.len(), 4);
}

/// A simulator whose leases are never given back and whose refusals are all Unavailable: the contract isn't vacuous.
struct Keeps(SimLessors);

struct KeepsRun(Arc<dyn Lessor>);

impl Lessor for KeepsRun {
    fn lease(&self, reference: &str, key: Option<[u8; 32]>) -> Result<Lease, LeaseRefused> {
        self.0.lease(reference, key).map_err(|e| match e {
            LeaseRefused::Denied(why) => LeaseRefused::Unavailable(why),
            other => other,
        })
    }
    fn release(&self, _: &str) {}
}

impl Lessors for Keeps {
    fn lessor(&self, id: &BoxId) -> Result<Arc<dyn Lessor>, Refusal> {
        Ok(Arc::new(KeepsRun(self.0.lessor(id)?)))
    }
}

impl Fixture for Keeps {
    fn lessors(&self) -> &dyn Lessors {
        self
    }
    fn hold(&self, reference: &str, value: &[u8]) -> Result<(), String> {
        self.0.hold(reference, value).map_err(|e| e.to_string())
    }
    fn holds(&self, lease: &str) -> Result<bool, String> {
        self.0.holds(lease).map_err(|e| e.to_string())
    }
}

/// C3: do run the contract against lessors that give nothing back and refuse a missing credential as Unavailable.
/// Expect exactly the give-back and denial clauses to fail.
#[test]
fn w33_lessors_that_give_nothing_back_fail_those_clauses() {
    let failed: Vec<&str> = conformance::run(&Keeps(SimLessors::new()))
        .iter()
        .map(|f| f.case)
        .collect();
    assert_eq!(failed, ["HEX-LEASE-2", "HEX-LEASE-3"]);
}

/// D242: do print a value and a refusal. Expect the value's length alone, and the refusal's words.
#[test]
fn w33_a_value_prints_its_length_alone_and_is_handed_on_whole() {
    let value = Value::new(b"sk-ant-api03-real".to_vec());
    assert_eq!(format!("{value:?}"), "Value(17 bytes)");
    assert_eq!(value.into_bytes(), b"sk-ant-api03-real");
    assert_eq!(
        LeaseRefused::KeyReplaced {
            asked: "a1".repeat(32),
            current: None
        }
        .to_string(),
        format!(
            "{} is the key its run had before it moved: its key is none",
            "a1".repeat(32)
        )
    );
}
