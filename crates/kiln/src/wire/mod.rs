//! What delivery every message kiln's processes exchange is held to (docs/wire.md).
//!
//! Each message is in one [`Class`], and its class says what its sender may count on: a command is
//! answered by the process that applied it (an ACK, or a NACK saying why not) and sent again under
//! its id until it is; a stream's items are numbered, acknowledged cumulatively and sent again
//! after a reconnect from where the receiver has them; a state is the newest of something,
//! acknowledged by version; and so on. Every enum on a wire is matched here without a catch-all
//! arm, so a message can't be added to one without being given a class, and docs/wire.md says the
//! same as these tables do (`contract::docs_say_what_the_tables_say` holds them together).

pub mod channel;
pub mod commands;
pub mod keep;
pub mod link;
pub mod sums;
pub mod uplink;

use crate::harness::core::Update;
use crate::harness::model::Command;
use crate::proto::{AgentMsg, ServerMsg};

/// What a command does while its receiver is away (its link down, its machine asleep).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Away {
    /// Kept by its sender and delivered, in order, once the receiver is back.
    Park,
    /// Refused at once as unreachable: done later it would surprise (a keystroke, an interrupt),
    /// so it's never replayed.
    Refuse,
    /// As the [`Command`] it carries says ([`command_class`]).
    AsItsCommand,
}

/// The delivery a message is held to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// Part of connecting: answered by the connection being made, or refused.
    Handshake,
    /// Something to do. It carries an id; the process that applied it answers (an ACK, or a NACK
    /// saying why not); its sender sends it again under the same id until it's answered; it's taken
    /// once however often it comes.
    Command(Away),
    /// A question asked under a `req`: answered by an [`Class::Answer`]. Its asker waits with a
    /// deadline, and a lost link answers it at once with a typed error.
    Ask,
    /// The answer to an ask or a command.
    Answer,
    /// An acknowledgement, or a NACK, itself.
    Ack,
    /// An item of an ordered stream (a session's timeline, a terminal's output, a job's events):
    /// numbered, acknowledged cumulatively, sent again after a reconnect from where the receiver
    /// has it; a snapshot only when the sender no longer has what's missing.
    Stream,
    /// The newest of something that changes (a machine's inventory, a session's state): the newest
    /// wins, sent again on every connect.
    State,
    /// A protocol of its own carried on the link, which acknowledges and resends by itself.
    Carried(&'static str),
    /// Measurements sent every few seconds: the next one stands in for a lost one.
    Telemetry,
    /// Live pictures and sound: a lost frame is overtaken by the next, never sent again.
    Media,
}

impl Class {
    /// How docs/wire.md writes it.
    pub fn label(self) -> String {
        match self {
            Class::Handshake => "handshake".into(),
            Class::Command(Away::Park) => "command, parked".into(),
            Class::Command(Away::Refuse) => "command, refused while away".into(),
            Class::Command(Away::AsItsCommand) => "command, as its `Command` says".into(),
            Class::Ask => "ask".into(),
            Class::Answer => "answer".into(),
            Class::Ack => "ack".into(),
            Class::Stream => "stream".into(),
            Class::State => "state".into(),
            Class::Carried(what) => format!("carried ({what})"),
            Class::Telemetry => "telemetry".into(),
            Class::Media => "media".into(),
        }
    }
}

/// The conversation feed (`crate::convo::feed`): batches numbered by its store, acknowledged and
/// resent by number.
const FEED: Class = Class::Carried("conversation feed");
/// Repo sync's Raft (`crate::sync`): hyper-raft acknowledges and resends its own messages.
const RAFT: Class = Class::Carried("repo sync's Raft");
/// The cluster group (`crate::store::learner`, docs/p2p.md P2): its Raft is hyper-raft's, which
/// acknowledges and resends; a runner's learner is told to join on every connect, and a read that
/// can't learn where the server is in time goes to the server.
const CLUSTER: Class = Class::Carried("the cluster group");
/// Reach's links (`crate::reach`): a link's two runners frame it end to end; the server relays.
const REACH: Class = Class::Carried("reach link");
/// Ports opened through the server (`crate::browser::ports`): a byte stream with its own window.
const PORTS: Class = Class::Carried("port window");
/// The channel itself ([`channel`]): its items carry other messages, each with its own class.
const CHANNEL: Class = Class::Carried("the channel");

const PARKED: Class = Class::Command(Away::Park);
const REFUSED: Class = Class::Command(Away::Refuse);

/// A function giving every variant of an enum its class, matched without a catch-all arm, and a
/// table of the variants' names and classes in the same order (what docs/wire.md is checked
/// against).
macro_rules! classes {
    ($(#[$doc:meta])* fn $f:ident($e:ty) -> $table:ident, $named:ident { $($p:pat => $c:expr,)+ }) => {
        $(#[$doc])*
        pub fn $f(m: &$e) -> Class {
            match m {
                $($p => $c,)+
            }
        }

        /// A message's variant name, as the table has it: what a log says of it (never what it
        /// carries).
        pub fn $named(m: &$e) -> &'static str {
            match m {
                $($p => variant(stringify!($p)),)+
            }
        }

        /// Every variant's name and class, in the order the match has them.
        pub fn $table() -> Vec<(&'static str, Class)> {
            vec![$((variant(stringify!($p)), $c),)+]
        }
    };
}

/// A variant's name from its pattern as written (`AgentMsg::Hello(..)`: `Hello`).
fn variant(pattern: &'static str) -> &'static str {
    let tail = pattern.rsplit("::").next().unwrap_or(pattern).trim_start();
    tail.split(['(', '{', ' ']).next().unwrap_or(tail).trim()
}

classes! {
    /// A runner's messages to the server.
    fn agent_class(AgentMsg) -> agent_classes, agent_name {
        AgentMsg::Hello(..) => Class::Handshake,
        AgentMsg::Heartbeat { .. } => Class::Telemetry,
        AgentMsg::Sample(..) => Class::Telemetry,
        AgentMsg::JobStarted { .. } => Class::Answer,
        AgentMsg::StepStarted { .. } => Class::Stream,
        AgentMsg::StepFinished { .. } => Class::Stream,
        AgentMsg::JobFinished(..) => Class::Stream,
        AgentMsg::JobSample { .. } => Class::Telemetry,
        AgentMsg::Reject { .. } => Class::Answer,
        AgentMsg::TermOutput { .. } => Class::Stream,
        AgentMsg::TermExit { .. } => Class::Stream,
        AgentMsg::TermList { .. } => Class::State,
        AgentMsg::TermReplay { .. } => Class::Answer,
        AgentMsg::AiSessions { .. } => Class::State,
        AgentMsg::AiTranscript { .. } => Class::Answer,
        AgentMsg::Harness { .. } => Class::Stream,
        AgentMsg::HarnessInventory { .. } => Class::State,
        AgentMsg::AiView { .. } => Class::Answer,
        AgentMsg::Instance(..) => Class::Handshake,
        AgentMsg::Build { .. } => Class::Handshake,
        AgentMsg::UpgradeLog { .. } => Class::Stream,
        AgentMsg::UpgradeDone { .. } => Class::Answer,
        AgentMsg::HarnessTaskOutput { .. } => Class::Answer,
        AgentMsg::DirList { .. } => Class::Answer,
        AgentMsg::HarnessHistory { .. } => Class::Answer,
        AgentMsg::Sync { .. } => RAFT,
        AgentMsg::FrontDoorStatus { .. } => Class::State,
        AgentMsg::JobEvent { .. } => Class::Stream,
        AgentMsg::Facts { .. } => Class::State,
        AgentMsg::Reach { .. } => REACH,
        AgentMsg::ReachHello { .. } => Class::State,
        AgentMsg::ReachAsk { .. } => Class::Ask,
        AgentMsg::Convo { .. } => FEED,
        AgentMsg::UpdatePrefs { .. } => Class::State,
        AgentMsg::Power { .. } => Class::State,
        AgentMsg::Answer { .. } => Class::Answer,
        AgentMsg::HarnessFileData { .. } => Class::Answer,
        AgentMsg::StreamFrame { .. } => Class::Media,
        AgentMsg::StreamEvent { .. } => Class::Stream,
        AgentMsg::PortOpened { .. } => Class::Answer,
        AgentMsg::PortData { .. } => PORTS,
        AgentMsg::PortAck { .. } => Class::Ack,
        AgentMsg::PortClose { .. } => PORTS,
        AgentMsg::Executor { .. } => Class::Answer,
        AgentMsg::Undeployed { .. } => Class::Answer,
        AgentMsg::Caps { .. } => Class::Handshake,
        AgentMsg::Wire(..) => CHANNEL,
        AgentMsg::Cluster { .. } => CLUSTER,
        AgentMsg::NetDrops { .. } => Class::Telemetry,
        AgentMsg::HarnessHandOver { .. } => Class::Stream,
        AgentMsg::HarnessAdopted { .. } => Class::Answer,
        AgentMsg::WorkState { .. } => Class::State,
        AgentMsg::Identity { .. } => Class::Ask,
    }
}

classes! {
    /// The server's messages to a runner.
    fn server_class(ServerMsg) -> server_classes, server_name {
        ServerMsg::Welcome { .. } => Class::Handshake,
        ServerMsg::Refused { .. } => Class::Handshake,
        ServerMsg::Assign(..) => REFUSED,
        ServerMsg::Cancel { .. } => PARKED,
        ServerMsg::Drain => PARKED,
        ServerMsg::TermOpen { .. } => REFUSED,
        ServerMsg::TermInput { .. } => REFUSED,
        ServerMsg::TermResize { .. } => Class::State,
        ServerMsg::TermClose { .. } => PARKED,
        ServerMsg::TermAttach { .. } => Class::Ask,
        ServerMsg::AiTranscript { .. } => Class::Ask,
        ServerMsg::HarnessOpen { .. } => REFUSED,
        ServerMsg::HarnessCmd { .. } => Class::Command(Away::AsItsCommand),
        ServerMsg::HarnessClose { .. } => PARKED,
        ServerMsg::AiView { .. } => Class::Ask,
        ServerMsg::HarnessFile { .. } => PARKED,
        ServerMsg::Retire { .. } => PARKED,
        ServerMsg::Upgrade { .. } => REFUSED,
        ServerMsg::HarnessTaskOutput { .. } => Class::Ask,
        ServerMsg::DirList { .. } => Class::Ask,
        ServerMsg::HarnessHistory { .. } => Class::Ask,
        ServerMsg::Sync { .. } => RAFT,
        ServerMsg::FrontDoor { .. } => Class::State,
        ServerMsg::AssignV2 { .. } => REFUSED,
        ServerMsg::SecretsFor { .. } => PARKED,
        ServerMsg::JobTerminalOpen { .. } => REFUSED,
        ServerMsg::KeepRelease { .. } => PARKED,
        ServerMsg::ProbeFacts => Class::Ask,
        ServerMsg::Reach { .. } => REACH,
        ServerMsg::ReachAnswer { .. } => Class::Answer,
        ServerMsg::KeepExtend { .. } => PARKED,
        ServerMsg::JobRerunStep { .. } => REFUSED,
        ServerMsg::JobExec { .. } => REFUSED,
        ServerMsg::Convo { .. } => FEED,
        ServerMsg::HarnessPackage { .. } => REFUSED,
        ServerMsg::HarnessRelease { .. } => PARKED,
        ServerMsg::UpdateAvailable { .. } => Class::State,
        ServerMsg::Diagnose { .. } => Class::Ask,
        ServerMsg::Repair { .. } => Class::Ask,
        ServerMsg::HarnessFileRead { .. } => Class::Ask,
        ServerMsg::StreamOpen { .. } => REFUSED,
        ServerMsg::StreamControl { .. } => REFUSED,
        ServerMsg::StreamInput { .. } => REFUSED,
        ServerMsg::StreamClose { .. } => PARKED,
        ServerMsg::Preview { .. } => Class::Ask,
        ServerMsg::PortOpen { .. } => Class::Ask,
        ServerMsg::PortData { .. } => PORTS,
        ServerMsg::PortAck { .. } => Class::Ack,
        ServerMsg::PortClose { .. } => PORTS,
        ServerMsg::Workspace { .. } => Class::Ask,
        ServerMsg::Executor { .. } => Class::Ask,
        ServerMsg::Received { .. } => Class::Ack,
        ServerMsg::Undeploy { .. } => PARKED,
        ServerMsg::Caps { .. } => Class::Handshake,
        ServerMsg::Wire(..) => CHANNEL,
        ServerMsg::Cluster { .. } => CLUSTER,
        ServerMsg::Kept { .. } => Class::State,
        ServerMsg::HarnessSend { .. } => PARKED,
        ServerMsg::Identity { .. } => Class::Answer,
    }
}

classes! {
    /// A person's (or kiln's own) commands to an agent session: the UI's socket, the API, kiln's
    /// MCP operations, carried to its machine in [`ServerMsg::HarnessCmd`]. Whether one waits for a
    /// machine that's away is its own to say, not `HarnessCmd`'s.
    fn command_class(Command) -> command_classes, command_name {
        Command::Send { .. } => PARKED,
        Command::QueueEdit { .. } => PARKED,
        Command::QueueRemove { .. } => PARKED,
        Command::QueueMove { .. } => PARKED,
        Command::QueueSendNow { .. } => REFUSED,
        Command::QueueSteer { .. } => REFUSED,
        Command::Approve { .. } => PARKED,
        Command::Interrupt => REFUSED,
        Command::ForceStop => REFUSED,
        Command::SetMode { .. } => REFUSED,
        Command::SetModel { .. } => REFUSED,
        Command::SetEffort { .. } => REFUSED,
        Command::SetSpeed { .. } => REFUSED,
        Command::MoveTo { .. } => REFUSED,
        Command::Moving { .. } => REFUSED,
        Command::HandedOver { .. } => PARKED,
        Command::Control { .. } => REFUSED,
        Command::StopTask { .. } => REFUSED,
        Command::Restart => REFUSED,
        Command::Suggest { .. } => Class::Ask,
        Command::Snapshot => Class::Ask,
        Command::McpEnable { .. } => REFUSED,
        Command::McpReconnect { .. } => REFUSED,
        Command::McpRefresh => REFUSED,
        Command::McpAuth { .. } => Class::Ask,
        Command::McpAdd { .. } => REFUSED,
        Command::McpRemove { .. } => REFUSED,
        Command::SkillView { .. } => Class::Ask,
        Command::SkillRun { .. } => PARKED,
        Command::SkillsReload => REFUSED,
        Command::StopTasks => REFUSED,
        Command::Resume => REFUSED,
        Command::AutoResume { .. } => REFUSED,
        Command::PauseAt { .. } => REFUSED,
        Command::MaxSubagents { .. } => REFUSED,
        Command::MoveOnLimit { .. } => REFUSED,
        Command::LimitHold { .. } => REFUSED,
    }
}

classes! {
    /// A session's updates, from wherever it runs to the server (carried in [`AgentMsg::Harness`])
    /// and on to its viewers.
    fn update_class(Update) -> update_classes, update_name {
        Update::Open { .. } => Class::Stream,
        Update::Closed => Class::Stream,
        Update::State { .. } => Class::State,
        Update::Meta { .. } => Class::State,
        Update::Events { .. } => Class::Stream,
        Update::Changes { .. } => Class::State,
        Update::Stream { .. } => Class::Stream,
        Update::Reply { .. } => Class::Answer,
        Update::Inbox { .. } => Class::Answer,
        Update::Reset { .. } => Class::Stream,
    }
}

/// A message of one of kiln's wires.
pub trait Classed {
    /// Its class.
    fn class(&self) -> Class;
}

impl Classed for AgentMsg {
    fn class(&self) -> Class {
        agent_class(self)
    }
}

impl Classed for ServerMsg {
    fn class(&self) -> Class {
        server_class(self)
    }
}

impl Classed for Command {
    fn class(&self) -> Class {
        command_class(self)
    }
}

impl Classed for Update {
    fn class(&self) -> Class {
        update_class(self)
    }
}

/// The class of any message on kiln's wires.
pub fn class_of<M: Classed>(m: &M) -> Class {
    m.class()
}

/// Which way a message on one of the UI's sockets goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Way {
    /// From the browser to the server.
    Up,
    /// From the server to the browser.
    Down,
}

/// A kind of message on one of the UI's sockets (their frames are JSON tagged by `t`, or bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SocketMsg {
    /// The socket's route.
    pub socket: &'static str,
    pub way: Way,
    /// The frame's `t`, or what it is when it has none.
    pub name: &'static str,
    pub class: Class,
}

const fn msg(socket: &'static str, way: Way, name: &'static str, class: Class) -> SocketMsg {
    SocketMsg { socket, way, name, class }
}

const SESSION: &str = "/api/harness/sessions/:id/ws";
const TERMINAL: &str = "/api/terminal/ws";
const LEG_TERMINAL: &str = "/api/ci/legs/:id/terminal";

/// Every kind of message on the UI's sockets. A session socket's commands up are [`Command`]s,
/// each classed by [`command_class`]: under an id their client made (`cmd`), each answered `ack`
/// or `nack`, or bare, as clients from before ids send them.
pub const SOCKETS: &[SocketMsg] = &[
    msg(SESSION, Way::Up, "cmd", Class::Command(Away::AsItsCommand)),
    msg(SESSION, Way::Down, "ack", Class::Ack),
    msg(SESSION, Way::Down, "nack", Class::Ack),
    msg(SESSION, Way::Down, "open", Class::Stream),
    msg(SESSION, Way::Down, "resumed", Class::Stream),
    msg(SESSION, Way::Down, "meta", Class::State),
    msg(SESSION, Way::Down, "state", Class::State),
    msg(SESSION, Way::Down, "state_patch", Class::State),
    msg(SESSION, Way::Down, "changes", Class::State),
    msg(SESSION, Way::Down, "files", Class::State),
    msg(SESSION, Way::Down, "events", Class::Stream),
    msg(SESSION, Way::Down, "synced", Class::Stream),
    msg(SESSION, Way::Down, "closed", Class::Stream),
    msg(SESSION, Way::Down, "error", Class::Answer),
    msg("/api/events", Way::Down, "event", Class::Stream),
    msg(TERMINAL, Way::Up, "input", REFUSED),
    msg(TERMINAL, Way::Up, "resize", Class::State),
    msg(TERMINAL, Way::Down, "output", Class::Stream),
    msg(LEG_TERMINAL, Way::Up, "input", REFUSED),
    msg(LEG_TERMINAL, Way::Up, "resize", Class::State),
    msg(LEG_TERMINAL, Way::Down, "output", Class::Stream),
    msg("/api/jobs/:id/logs/ws", Way::Down, "log", Class::Stream),
    msg("/api/harness/sessions/:id/streams/:sid/ws", Way::Up, "input", REFUSED),
    msg("/api/harness/sessions/:id/streams/:sid/ws", Way::Down, "frame", Class::Media),
    msg("/api/harness/sessions/:id/streams/:sid/ws", Way::Down, "event", Class::Stream),
    msg("/api/deploy/apps/:app/logs/follow", Way::Down, "log", Class::Stream),
    msg("/api/tunnel", Way::Up, "datagram", Class::Carried("tunnel")),
    msg("/api/tunnel", Way::Down, "datagram", Class::Carried("tunnel")),
];

#[cfg(test)]
mod contract {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;

    /// docs/wire.md's rows: in each section whose heading names `what`, `| `Name` | class | ...`.
    fn rows(doc: &str, what: &str) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        let mut inside = false;
        for line in doc.lines() {
            if line.starts_with("## ") {
                inside = line.contains(what);
                continue;
            }
            if !inside {
                continue;
            }
            let cells: Vec<&str> = line.split('|').map(str::trim).collect();
            if let [_, name, class, ..] = cells.as_slice()
                && let Some(name) = name.strip_prefix('`').and_then(|n| n.strip_suffix('`'))
            {
                out.insert(name.to_owned(), (*class).to_owned());
            }
        }
        out
    }

    fn doc() -> anyhow::Result<String> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/wire.md");
        Ok(std::fs::read_to_string(path)?)
    }

    fn agree(what: &str, table: &[(&'static str, Class)], doc: &str) -> Vec<String> {
        let said = rows(doc, what);
        let mut wrong = Vec::new();
        let names: BTreeSet<&str> = table.iter().map(|(n, _)| *n).collect();
        for (name, class) in table {
            match said.get(*name) {
                None => wrong.push(format!("{what}::{name} is not in docs/wire.md")),
                Some(c) if *c != class.label() => wrong.push(format!("{what}::{name}: docs/wire.md says {c:?}, the table {:?}", class.label())),
                Some(_) => {}
            }
        }
        for name in said.keys() {
            if !names.contains(name.as_str()) {
                wrong.push(format!("docs/wire.md names {what}::{name}, which isn't one"));
            }
        }
        wrong
    }

    #[test]
    fn docs_say_what_the_tables_say() -> anyhow::Result<()> {
        let doc = doc()?;
        let mut wrong = agree("`AgentMsg`", &agent_classes(), &doc);
        wrong.extend(agree("`ServerMsg`", &server_classes(), &doc));
        wrong.extend(agree("`Command`", &command_classes(), &doc));
        wrong.extend(agree("`Update`", &update_classes(), &doc));
        assert!(wrong.is_empty(), "docs/wire.md and crate::wire disagree:\n{}", wrong.join("\n"));
        Ok(())
    }

    #[test]
    fn docs_list_every_socket_message() -> anyhow::Result<()> {
        let doc = doc()?;
        let mut wrong = Vec::new();
        for m in SOCKETS {
            let way = match m.way {
                Way::Up => "up",
                Way::Down => "down",
            };
            let row = format!("| `{}` | {way} | `{}` | {} |", m.socket, m.name, m.class.label());
            if !doc.lines().any(|l| l.starts_with(&row)) {
                wrong.push(row);
            }
        }
        assert!(wrong.is_empty(), "docs/wire.md lacks these socket rows:\n{}", wrong.join("\n"));
        Ok(())
    }

    #[test]
    fn a_page_that_drifts_is_caught() {
        let table = [("Hello", Class::Handshake), ("Harness", Class::Stream), ("Facts", Class::State)];
        let page = "## Runner to server: `AgentMsg`\n\n| Message | Class | Why | Today |\n|---|---|---|---|\n| `Hello` | handshake | a | b |\n| `Harness` | state | a | b |\n| `Ghost` | ack | a | b |\n";
        let wrong = agree("`AgentMsg`", &table, page);
        assert!(wrong.iter().any(|w| w.contains("Harness") && w.contains("\"state\"")), "{wrong:?}");
        assert!(wrong.iter().any(|w| w.contains("Facts is not in docs/wire.md")), "{wrong:?}");
        assert!(wrong.iter().any(|w| w.contains("Ghost, which isn't one")), "{wrong:?}");
        assert_eq!(wrong.len(), 3, "{wrong:?}");
    }

    #[test]
    fn class_of_gives_the_plans_examples() {
        assert_eq!(class_of(&ServerMsg::HarnessClose { session: "s".into() }), Class::Command(Away::Park));
        assert_eq!(class_of(&ServerMsg::TermInput { session: "s".into(), data: vec![b'y'] }), Class::Command(Away::Refuse));
        assert_eq!(class_of(&AgentMsg::Harness { session: "s".into(), json: "{}".into() }), Class::Stream);
        assert_eq!(class_of(&AgentMsg::FrontDoorStatus { json: "{}".into() }), Class::State);
        assert_eq!(class_of(&ServerMsg::HarnessCmd { session: "s".into(), json: "{}".into() }), Class::Command(Away::AsItsCommand));
        assert_eq!(class_of(&Command::Interrupt), Class::Command(Away::Refuse));
        assert_eq!(agent_classes().into_iter().find(|(n, _)| *n == "Sample").map(|(_, c)| c), Some(Class::Telemetry));
        assert_eq!(class_of(&Command::Approve { id: "a".into(), decision: crate::harness::model::Decision::Allow, message: None, always: None, answers: None, choices: None }), Class::Command(Away::Park));
    }

    #[test]
    fn a_variant_is_named_from_its_pattern() {
        assert_eq!(variant("AgentMsg::Hello(..)"), "Hello");
        assert_eq!(variant("ServerMsg::Welcome { .. }"), "Welcome");
        assert_eq!(variant("ServerMsg::Drain"), "Drain");
    }

    #[test]
    fn every_variant_is_in_its_table_once() {
        for (what, table) in [("AgentMsg", agent_classes()), ("ServerMsg", server_classes()), ("Command", command_classes()), ("Update", update_classes())] {
            let names: BTreeSet<&str> = table.iter().map(|(n, _)| *n).collect();
            assert_eq!(names.len(), table.len(), "{what} names a variant twice");
        }
    }
}
