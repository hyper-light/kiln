//! The review's B-6 (kiln's HexVms adapter; goal 54's change 3): what the records answer of a box's start's end and of
//! its delete with its host use is its runtime's own, as every other call they pass on is: never the trait's defaults,
//! which answer `started` as an inspect, at once, and a delete with host use as a plain delete answering none, read as a
//! run that never started, so kiln charges nothing.
use super::*;
use hex_api::sim::SimRuntime;
use hex_api::{Bounds, BoxState, ClassSpec, Execution};
use std::time::Duration;

/// The simulated runtime, which says what of its own it answered: `started` the box Running where its inspect says it
/// isn't, and its delete with host use [`final_use`], a use the default never answers.
#[derive(Default)]
struct Answering {
    sim: SimRuntime,
}

/// The final use [`Answering`] answers its delete with host use with.
fn final_use() -> hex_api::HostUse {
    hex_api::HostUse {
        cpu_ns: 7,
        footprint_bytes: 11,
        resident_bytes: 13,
        guest_ns: vec![17],
        window: Duration::from_nanos(19),
    }
}

impl Runtime for Answering {
    fn classes(&self) -> Vec<hex_api::Class> {
        self.sim.classes()
    }
    fn create(&self, spec: BoxSpec) -> Result<BoxInfo, Refusal> {
        self.sim.create(spec)
    }
    fn start(&self, id: &BoxId) -> Result<(), Refusal> {
        self.sim.start(id)
    }
    fn exec(
        &self,
        id: &BoxId,
        program: hex_api::Program,
        stdio: hex_api::Stdio,
    ) -> Result<Box<dyn hex_api::Proc>, Refusal> {
        self.sim.exec(id, program, stdio)
    }
    fn run(
        &self,
        spec: BoxSpec,
        program: hex_api::Program,
        stdio: hex_api::Stdio,
    ) -> Result<Box<dyn hex_api::Proc>, Refusal> {
        self.sim.run(spec, program, stdio)
    }
    fn signal(&self, id: &BoxId, signal: hex_api::Signal) -> Result<(), Refusal> {
        self.sim.signal(id, signal)
    }
    fn inspect(&self, id: &BoxId) -> Result<BoxInfo, Refusal> {
        self.sim.inspect(id)
    }
    fn usage(&self, id: &BoxId) -> Result<Usage, Refusal> {
        self.sim.usage(id)
    }
    fn list(&self) -> Result<Vec<BoxInfo>, Refusal> {
        self.sim.list()
    }
    fn delete(&self, id: &BoxId) -> Result<(), Refusal> {
        self.sim.delete(id)
    }
    fn started(&self, id: &BoxId) -> Result<BoxInfo, Refusal> {
        let mut info = self.sim.inspect(id)?;
        info.state = BoxState::Running;
        Ok(info)
    }
    fn delete_with_host_use(&self, id: &BoxId) -> Result<Option<hex_api::HostUse>, Refusal> {
        self.sim.delete(id).map(|()| Some(final_use()))
    }
}

// It keeps no native metadata nor routes of its own, as the simulator it wraps keeps none.
impl Recover for Answering {
    fn project_routes(
        &self,
        _: &[PoolRecord],
        _: Option<(Execution, &[DeviceId])>,
        _: &mut hex_runtime::devices::RouteProjection,
    ) -> Result<(), Refusal> {
        Ok(())
    }
    fn preflight_retained(
        &self,
        _: &hex_runtime::devices::Records,
        _: &[PoolRecord],
    ) -> Result<(), Refusal> {
        Ok(())
    }
    fn trace(&self, _: &BoxId) -> Result<Option<ProcId>, Refusal> {
        Ok(None)
    }
    fn adopt(&self, _: &BoxSpec, _: Option<ProcId>) -> Result<(), Refusal> {
        Ok(())
    }
    fn end(&self, _: Option<ProcId>) -> Result<(), Refusal> {
        Ok(())
    }
}

fn process_box(id: &str) -> BoxSpec {
    BoxSpec {
        id: BoxId::new(id).unwrap(),
        bounds: Bounds::default(),
        class: ClassSpec::Process,
        owner: None,
        devices: Vec::new(),
        allocation: None,
        route: None,
        band: 0,
    }
}

/// The review's B-6: the records pass `started` and `delete_with_host_use` to the runtime a box's record routes to, as
/// they pass `host_use`. Do: make a box over [`Answering`]; ask `started` of it through the records, then its delete
/// with its host use. Expect: `started` the runtime's own (Running, where its inspect says it isn't); the delete its
/// final use; the box's record gone with it.
#[test]
fn b6_the_records_pass_started_and_delete_with_host_use_to_the_boxs_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let node = Recorded::open(Answering::default(), dir.path()).unwrap();
    let id = node.create(process_box("b6-recorded")).unwrap().id;
    assert_ne!(node.inspect(&id).unwrap().state, BoxState::Running);
    assert_eq!(node.started(&id).unwrap().state, BoxState::Running);
    assert_eq!(node.delete_with_host_use(&id), Ok(Some(final_use())));
    assert!(matches!(node.inspect(&id), Err(Refusal::NoSuchBox(_))));
    assert!(node.books.store.get(&id).unwrap().is_none());
}
