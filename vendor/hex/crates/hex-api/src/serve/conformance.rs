//! The serve seam's contract as cases every implementation passes (CLAUDE.md §9; Shards' D286): the simulator in this
//! crate's tests, and every orchestrator's server (kiln's over its slates instance). A case drives the server through
//! the seam alone and its fixture, which says, in its server's own protocol, what a stream it was given serves; each
//! reads "do X, expect Y", and [`run`] returns the cases that failed and why.

use super::{Servers, Stream};
use crate::{BoxId, Refusal, ServedMount};

/// What a case runs against: the server under test, and its fixture's word on what it serves.
pub trait Fixture {
    fn servers(&self) -> &dyn Servers;
    /// Its server serves `source` from now.
    fn serve(&self, source: &str) -> Result<(), String>;
    /// What the server answering `stream` serves on it, as that server says it over the stream itself: its source and
    /// whether read-only. Asked once of each stream.
    fn probe(&self, stream: &Stream) -> Result<Served, String>;
}

/// What a stream serves, by its server's word ([`Fixture::probe`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Served {
    pub source: String,
    pub read_only: bool,
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
        id: "HEX-SERVE-1",
        says: "a box's stream of a source its server serves is answered by that server, for that source, writable as asked",
        run: serves,
    },
    Case {
        id: "HEX-SERVE-2",
        says: "a read-only share's stream is served read-only",
        run: read_only,
    },
    Case {
        id: "HEX-SERVE-3",
        says: "a source its server serves none of is refused Invalid, naming it, and no stream is given",
        run: refuses,
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

/// Box `name`'s id (`hex-conformance-serve-<name>`).
fn id(name: &str) -> Result<BoxId, String> {
    BoxId::new(&format!("hex-conformance-serve-{name}")).map_err(|e| e.to_string())
}

/// A served share of `source` at `/project`, read-only where `read_only` says, given to no member.
fn share(source: &str, read_only: bool) -> ServedMount {
    ServedMount {
        source: source.to_owned(),
        destination: "/project".to_owned(),
        read_only,
        members: Vec::new(),
    }
}

/// HEX-SERVE-1: do ask a stream of a source the server serves, writable. Expect one its server answers for that source,
/// writable.
fn serves(fx: &dyn Fixture) -> Result<(), String> {
    fx.serve("conformance/one")?;
    let stream = fx
        .servers()
        .stream(&id("one")?, &share("conformance/one", false))
        .map_err(|e| format!("a source it serves: {e}"))?;
    let served = fx.probe(&stream)?;
    ensure(
        served
            == Served {
                source: "conformance/one".to_owned(),
                read_only: false,
            },
        || format!("the stream serves {served:?}, not conformance/one writable"),
    )
}

/// HEX-SERVE-2: do ask a read-only stream of a source the server serves. Expect it served read-only.
fn read_only(fx: &dyn Fixture) -> Result<(), String> {
    fx.serve("conformance/two")?;
    let stream = fx
        .servers()
        .stream(&id("two")?, &share("conformance/two", true))
        .map_err(|e| format!("a source it serves, read-only: {e}"))?;
    let served = fx.probe(&stream)?;
    ensure(
        served
            == Served {
                source: "conformance/two".to_owned(),
                read_only: true,
            },
        || format!("the stream serves {served:?}, not conformance/two read-only"),
    )
}

/// HEX-SERVE-3: do ask a stream of a source the server serves none of. Expect `Invalid` naming it, and no stream.
fn refuses(fx: &dyn Fixture) -> Result<(), String> {
    match fx
        .servers()
        .stream(&id("three")?, &share("conformance/none", false))
    {
        Err(Refusal::Invalid { why, .. }) => ensure(why.contains("conformance/none"), || {
            format!("refused without naming the source: {why}")
        }),
        Err(other) => Err(format!(
            "a source it serves none of refused {other}, not Invalid"
        )),
        Ok(_) => Err("a source it serves none of was given a stream".to_owned()),
    }
}
