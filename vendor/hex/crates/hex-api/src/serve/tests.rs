use super::conformance::{self, Fixture, Served};
use super::sim::SimServers;
use super::*;

/// The simulator's fixture: its probe reads back the ask that made a stream.
struct Sim(SimServers);

impl Fixture for Sim {
    fn servers(&self) -> &dyn Servers {
        &self.0
    }
    fn serve(&self, source: &str) -> Result<(), String> {
        self.0.serve(source).map_err(|e| e.to_string())
    }
    fn probe(&self, stream: &Stream) -> Result<Served, String> {
        let probed = self.0.probe(stream).map_err(|e| e.to_string())?;
        Ok(Served {
            source: probed.source,
            read_only: probed.read_only,
        })
    }
}

fn share(source: &str, read_only: bool) -> ServedMount {
    ServedMount {
        source: source.to_owned(),
        destination: "/project".to_owned(),
        read_only,
        members: Vec::new(),
    }
}

/// CLAUDE.md §9, Shards' D286: do run the serve contract against the simulator. Expect every case to pass, and a stream
/// made for each ask the cases made that a source was served for (HEX-SERVE-1 and 2), none for the one refused.
#[test]
fn d286_the_simulated_servers_pass_the_serve_contract() {
    let sim = SimServers::new();
    let failures = conformance::run(&Sim(sim.clone()));
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(conformance::CASES.len(), 3);
    assert_eq!(
        sim.made().unwrap(),
        2,
        "a stream made for an ask the contract refused, or none for one it served"
    );
}

/// A server that serves every source writable, whatever is asked, and gives a refused source a stream too: the contract
/// isn't vacuous.
struct Careless(SimServers);

impl Servers for Careless {
    fn stream(&self, id: &BoxId, share: &ServedMount) -> Result<Stream, Refusal> {
        self.0.serve(&share.source)?;
        self.0.stream(
            id,
            &ServedMount {
                read_only: false,
                ..share.clone()
            },
        )
    }
}

struct CarelessFixture(Careless);

impl Fixture for CarelessFixture {
    fn servers(&self) -> &dyn Servers {
        &self.0
    }
    fn serve(&self, source: &str) -> Result<(), String> {
        self.0.0.serve(source).map_err(|e| e.to_string())
    }
    fn probe(&self, stream: &Stream) -> Result<Served, String> {
        Sim(self.0.0.clone()).probe(stream)
    }
}

/// Do run the contract against a server that ignores a share's read-only ask and serves a source it was never told
/// to. Expect exactly HEX-SERVE-2 and HEX-SERVE-3 to fail: the contract tells a careless server from a careful one.
#[test]
fn d286_the_serve_contract_fails_a_server_that_ignores_read_only_and_unknown_sources() {
    let failures = conformance::run(&CarelessFixture(Careless(SimServers::new())));
    let failed: Vec<&str> = failures.iter().map(|f| f.case).collect();
    assert_eq!(failed, ["HEX-SERVE-2", "HEX-SERVE-3"], "{failures:?}");
}

/// Shards' D286: each ask is given a stream of its own. Do ask two streams of one source, for two boxes, and close the
/// first. Expect two streams, each the ask that made it by the simulator's word, the second read after the first is
/// closed.
#[test]
fn d286_each_ask_is_given_a_stream_of_its_own() {
    let sim = SimServers::new();
    sim.serve("project").unwrap();
    let (a, b) = (
        BoxId::new("serve-a").unwrap(),
        BoxId::new("serve-b").unwrap(),
    );
    let first = sim.stream(&a, &share("project", false)).unwrap();
    let second = sim.stream(&b, &share("project", true)).unwrap();
    let first_probed = sim.probe(&first).unwrap();
    drop(first);
    let second_probed = sim.probe(&second).unwrap();
    assert_eq!(
        (first_probed.id.as_str(), first_probed.read_only),
        ("serve-a", false)
    );
    assert_eq!(
        (second_probed.id.as_str(), second_probed.read_only),
        ("serve-b", true)
    );
    assert_eq!(sim.made().unwrap(), 2);
}

/// Hex's CLAUDE.md §2, its bound: do ask past the simulator's most streams. Expect the ask past it refused `Exhausted`,
/// naming the bound, and nothing more made.
#[test]
fn d286_an_ask_past_the_simulators_most_streams_is_refused_exhausted() {
    let sim = SimServers::new();
    sim.serve("project").unwrap();
    let id = BoxId::new("serve-bound").unwrap();
    // Each stream made is let go of at once; its record stays.
    for _ in 0..sim::SIM_STREAMS {
        drop(sim.stream(&id, &share("project", false)).unwrap());
    }
    assert!(matches!(
        sim.stream(&id, &share("project", false)),
        Err(Refusal::Exhausted { bound, .. }) if bound == sim::SIM_STREAMS
    ));
    assert_eq!(sim.made().unwrap(), sim::SIM_STREAMS);
}
