//! The records kiln's live topics carry (docs/browser-protocol.md, "Topics"): each a struct of
//! the protocol, sent field by field, so a change sends only the fields that changed. kiln's
//! server adapters (`crates/kiln/src/bp/`) fill them from its own types; the TypeScript codec reads
//! them (`fromFields<Name>` in `ui/src/api/bp/gen.ts`). Times are Unix milliseconds; what would be
//! a float is an integer in stated units. Each grows only by fields appended.

use crate::{bp_enum, bp_struct};

bp_enum! {
    /// A run's, a leg's or a step's status, as kiln's store has it.
    pub enum Status {
        Pending = 0,
        Queued = 1,
        Running = 2,
        Succeeded = 3,
        Failed = 4,
        Cancelled = 5,
        Skipped = 6,
        TimedOut = 7,
    }
}

bp_struct! {
    /// A run (topic `runs`, keyed by its id).
    pub struct Run {
        pub repo: String,
        pub repo_name: String,
        pub number: i64,
        pub pipeline: String,
        pub event: String,
        pub branch: String,
        pub sha: String,
        pub message: String,
        pub author: String,
        pub status: Status,
        pub created_at: i64,
        pub started_at: Option<i64>,
        pub finished_at: Option<i64>,
        pub error: Option<String>,
    }
}

bp_struct! {
    /// A leg, without its steps (topic `legs`, keyed by its id).
    pub struct Leg {
        pub run: String,
        pub key: String,
        pub display: String,
        pub status: Status,
        pub labels: Vec<String>,
        pub agent: Option<String>,
        pub executor: Option<String>,
        pub attempt: u32,
        pub queued_at: Option<i64>,
        pub started_at: Option<i64>,
        pub finished_at: Option<i64>,
        pub exit_code: Option<i64>,
        pub error: Option<String>,
    }
}

bp_struct! {
    /// A step of a leg (topic `steps`, keyed `<leg>.<idx>`).
    pub struct Step {
        pub idx: u32,
        pub name: String,
        pub status: Status,
        pub started_at: Option<i64>,
        pub finished_at: Option<i64>,
        pub exit_code: Option<i64>,
    }
}

bp_struct! {
    /// A runner (topic `runners`, keyed by its name); its sample goes in `samples`.
    pub struct Runner {
        pub label: Option<String>,
        pub hostname: String,
        pub os: String,
        pub arch: String,
        pub version: String,
        pub labels: Vec<String>,
        pub slots: u32,
        pub busy: u32,
        pub connected: bool,
        pub draining: bool,
        pub ephemeral: bool,
        pub terminal: bool,
        pub last_seen: i64,
        pub enrolled_at: i64,
        pub running: Vec<String>,
    }
}

bp_struct! {
    /// A machine's host sample (topic `samples`, keyed by the machine; `server` for the server's).
    pub struct Sample {
        pub ts_ms: i64,
        /// CPU use, per mille of all its cores.
        pub cpu_pm: u32,
        pub cpus: u32,
        /// The one-minute load, in hundredths.
        pub load1_c: u32,
        pub mem_total: u64,
        pub mem_used: u64,
        pub disk_total: u64,
        pub disk_used: u64,
        pub net_rx: u64,
        pub net_tx: u64,
        pub procs: u32,
    }
}

bp_struct! {
    /// The scheduler's counts (topic `queue`, one record).
    pub struct QueueCounts {
        pub queued: i64,
        pub running: i64,
    }
}

bp_struct! {
    /// A kiln event the protocol carries as its JSON (topic `events`, keyed by number): its tag
    /// and the event as the events socket sends it.
    pub struct Event {
        pub tag: String,
        pub json: crate::wire::Bytes,
    }
}

bp_struct! {
    /// That a resource changed and its readers should read it again (topic `changed`, keyed by
    /// the resource): how many times, so a page sees each, and only the newest waits.
    pub struct Changed {
        pub times: u64,
    }
}

bp_struct! {
    /// An item of a session topic (`session`, keyed `info`, `meta`, `state.<field>`,
    /// `change.<file>`, `files`, or an event's number): its JSON, and for an event the text
    /// streamed into it since.
    pub struct SessionItem {
        pub json: crate::wire::Bytes,
        pub streamed: String,
    }
}

/// Every record type, for the schema registry (and so the TypeScript codec).
pub fn visit(reg: &mut crate::schema::Registry) {
    use crate::wire::Wire;
    Run::visit(reg);
    Leg::visit(reg);
    Step::visit(reg);
    Runner::visit(reg);
    Sample::visit(reg);
    QueueCounts::visit(reg);
    Event::visit(reg);
    Changed::visit(reg);
    SessionItem::visit(reg);
}

/// The record types' names: the ones the TypeScript codec gets `fromFields` readers for.
pub const RECORDS: &[&str] = &["Run", "Leg", "Step", "Runner", "Sample", "QueueCounts", "Event", "Changed", "SessionItem"];
