//! An agent's volumes (kiln plan 01a1dd2e959772a1b17bc49026448e8a, P6.2; Shards' D281): what its definition keeps
//! from one of its sessions to the next on a machine (its caches, toolchains and package stores), each a named volume
//! of that machine's Shards store, kept on its disk past every session and found by its name again by the agent's next
//! session there. Volumes stay per machine (the person, 2026-11-14: "Per machine as today, but eventually we'll be
//! using Slates"): a session on another machine has that machine's own.
//!
//! - **Declared** by its definition ([`Volume`]): its name and the most bytes it may hold.
//! - **Where its harness sees it**: `/volumes/<name>` ([`at`]), beneath the folder every isolated launch declares for
//!   folders plugged in live (`sandbox::isolated::VOLUMES`; Shards' plug roots, D271).
//! - **Whose**: one person's alone, as a person's harness homes are (`harness::isolated::home`): its store's name for it
//!   ([`store_name`]) is made from the person its session acts for, its definition and its name, so no other person's
//!   session of that definition, and no other definition's, is given it.
//! - **Given** to its session's harness by that name as its microVM starts, with its bound
//!   (`sandbox::isolated::launch_of`: the seam's `Launch::volumes`, in Hex's box spec, its box admitted with their
//!   bounds together as its disk), which Shards makes where it isn't there and holds to its bound (D271, D281); a
//!   session on its machine's host is refused (`harness::lineage`), as it has no store of Shards'.
//! - **Admitted**: what its volumes may still take of its machine's disk is what its placement asks of it
//!   (`Isolation::disk`, `harness::lineage::session_needs`): their bounds together, less what they hold there already
//!   ([`held_on`]), which the disk counts as used. Hex holds each box to their whole bounds, so none writes past them.
//! - **Seen and cleared** by its person alone ([`list`], [`clear`]; `agentdefs.volumes`, `agentdefs.volumes.clear`):
//!   each machine's, asked of its runner (`ServerMsg::Volumes`, [`Ask`]), which reads its microVMs' store
//!   (`Sandbox::volumes`, Hex's `Runtime::volumes`) and clears one there once no session holds it
//!   (`Sandbox::clear_volume`), each refusal typed by its cause ([`Cause`]).

use kiln_seams::sandbox::{Refusal, VolumeCause};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::sessions::Definition;
use super::{Error, R, Who, builtin};
use crate::hub::Hub;

/// A volume an agent keeps on each machine it runs on ([module docs](self)).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Volume {
    /// Its name, once among the agent's: letters, digits, `_`, `.` and `-`, a letter or digit first, as Shards names a
    /// volume. Its harness sees it at `/volumes/<name>`.
    pub name: String,
    /// The most bytes it may hold: what each of its sessions asks of its machine's disk.
    #[schemars(range(min = 1))]
    pub bytes: u64,
}

/// The most bytes a Linux file name takes (`NAME_MAX`, `<linux/limits.h>`): a volume's name is one in its microVM.
const NAME_MAX: usize = 255;

/// Where its harness sees volume `name` in its microVM.
pub fn at(name: &str) -> String {
    format!("{}/{name}", crate::sandbox::isolated::VOLUMES)
}

/// Why `name` isn't a volume's, if it isn't: Shards' grammar for one (`[a-zA-Z0-9][a-zA-Z0-9_.-]*`, its `volumes.rs`
/// RESTRICTED, Docker's own), within one file name of its microVM's Linux.
fn name_problem(name: &str) -> Option<String> {
    let mut b = name.bytes();
    let first = b.next().is_some_and(|c| c.is_ascii_alphanumeric());
    let rest = b.all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-'));
    (!first || !rest || name.len() > NAME_MAX).then(|| format!("{name:?} isn't a volume's name: letters, digits, `_`, `.` and `-`, a letter or digit first, {NAME_MAX} at the most"))
}

/// Why `volumes` aren't an agent's: every reason, none when they are. Each has a name of its own and a bound that lets
/// it hold something, and their bounds together are a number of bytes ([`bytes`]).
pub fn problems(volumes: &[Volume]) -> Vec<String> {
    let mut why = Vec::new();
    let mut names: Vec<&str> = Vec::new();
    for v in volumes {
        if let Some(p) = name_problem(&v.name) {
            why.push(p);
        } else if names.contains(&v.name.as_str()) {
            why.push(format!("two volumes are named {}", v.name));
        } else {
            names.push(&v.name);
        }
        if v.bytes == 0 {
            why.push(format!(
                "volume {} may hold 0 bytes, which holds nothing: give the most it may hold",
                v.name
            ));
        }
    }
    if bytes(volumes).is_none() {
        why.push("the volumes' bounds together pass 2^64 bytes, more than a machine's disk can be asked for".to_owned());
    }
    why
}

/// The bytes `volumes` may hold together, which a session of their agent asks of its machine's disk; none past what a
/// u64 counts.
pub fn bytes(volumes: &[Volume]) -> Option<u64> {
    volumes
        .iter()
        .try_fold(0u64, |sum, v| sum.checked_add(v.bytes))
}

/// The name a machine's store keeps volume `name` of agent definition `definition` by for `person`'s sessions (`owner`,
/// `user:<id>`; empty for a start acting for no one): `kiln-agent-` and the SHA-256, hex, of the three, each ended by a
/// NUL, which none of them holds, so no two of them name one volume. Shards' grammar for a name takes it.
pub fn store_name(person: &str, definition: &str, name: &str) -> String {
    format!(
        "kiln-agent-{}",
        crate::util::sha256_hex(format!("{person}\0{definition}\0{name}\0").as_bytes())
    )
}

/// One of an agent's volumes as a session of it is given it (`Spec::volumes`): its name, its store's name for it on
/// the session's machine ([`store_name`]), where its harness sees it ([`at`]), and the most bytes it may hold, its bound
/// there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Given {
    pub name: String,
    pub store: String,
    pub at: String,
    /// 0 from a server older than bounds, which its launch refuses rather than give it unbounded.
    #[serde(default)]
    pub bytes: u64,
}

/// `volumes` of agent definition `definition` as a session acting for `person` is given them.
pub fn given(person: &str, definition: &str, volumes: &[Volume]) -> Vec<Given> {
    volumes
        .iter()
        .map(|v| Given {
            name: v.name.clone(),
            store: store_name(person, definition, &v.name),
            at: at(&v.name),
            bytes: v.bytes,
        })
        .collect()
}

/// The volumes of `d`, the definition a session starts from (`Ask::definition`), at its version then. kiln says which
/// definition a session starts from, never what was asked, so it's read as kiln reads it, whoever's it is. kiln's own
/// declare none; a version 0 of a person's own (a split's agent whose version it didn't keep) is its newest.
pub async fn of(hub: &Hub, d: &Definition) -> R<Vec<Volume>> {
    if d.id.starts_with(builtin::PREFIX) {
        return Ok(builtin::get(&d.id)
            .map(|b| b.settings.volumes)
            .unwrap_or_default());
    }
    let kiln = Who {
        person: String::new(),
        all: true,
    };
    let mut versions = super::versions(hub, &kiln, &d.id).await?.into_iter();
    let then = if d.version == 0 {
        versions.next()
    } else {
        versions.find(|v| v.version == d.version)
    };
    then.map(|v| v.settings.volumes).ok_or_else(|| {
        Error::Bad(format!(
            "agent definition {} has no version {}",
            d.id, d.version
        ))
    })
}

/// What the server asks a runner of the agent volumes its microVMs' store keeps (`ServerMsg::Volumes`,
/// `proto::feature::VOLUMES`), each by its name in that store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ask {
    /// Those of `stores` it keeps, as it reads them.
    List { stores: Vec<String> },
    /// `store` cleared.
    Clear { store: String },
}

/// A runner's answer to an [`Ask`] (`AgentMsg::Answer`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Answer {
    /// Those of the volumes asked its store keeps; none of any other.
    Listed {
        volumes: Vec<Held>,
    },
    Cleared,
    /// Refused by `cause`, in its store's words.
    Refused {
        cause: Cause,
        why: String,
    },
}

/// A volume a runner's store keeps ([`Answer::Listed`]), as it read it: its name there; the bytes its files take; its
/// bound; its charge, what its files take by its bound's own measure, which its bound holds it to (Shards' D281), each
/// none where unread; and when it was made, in nanoseconds since the Unix epoch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Held {
    pub store: String,
    pub bytes: Option<u64>,
    pub bound: Option<u64>,
    pub charge: Option<u64>,
    pub created_ns: u64,
}

/// Why a runner refused an [`Ask`], each by its own retry: the seam's volume causes, then its machine's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cause {
    /// A session holds it until it ends: its files are kept.
    InUse,
    /// Its store has none of that name: cleared already, or never made there.
    Unknown,
    /// Asked with another bound than the sessions holding it serve it with.
    Bound,
    /// A clearing holds it.
    Clearing,
    /// Its machine keeps no agent's volumes: it holds no microVMs, or none that keep any.
    Unsupported,
    /// Its store couldn't read or clear it.
    Failed,
}

impl Cause {
    /// What it says of a volume a person asked to clear.
    fn words(self) -> &'static str {
        match self {
            Cause::InUse => {
                "is in use by a running session of this agent there, and its files are kept"
            }
            Cause::Unknown => "isn't there: cleared already, or never made there",
            Cause::Bound => "is held by a session with another bound",
            Cause::Clearing => "is being cleared already",
            Cause::Unsupported => "can't be there: that machine keeps no agent's volumes",
            Cause::Failed => "couldn't be cleared there",
        }
    }
}

/// `ask` answered on a runner from its microVMs' store (`crate::sandbox::microvm`): those of the volumes asked that it
/// keeps, or one cleared, each refusal by its cause. Only the names asked are answered, so no other person's or
/// agent's volume leaves its machine.
pub async fn answer(ask: Ask) -> Answer {
    let Some(vms) = crate::sandbox::microvm() else {
        return Answer::Refused {
            cause: Cause::Unsupported,
            why: "this machine holds no microVMs, so it keeps no agent's volumes".into(),
        };
    };
    match ask {
        Ask::List { stores } => match vms.sandbox.volumes().await {
            Ok(kept) => Answer::Listed {
                volumes: kept
                    .into_iter()
                    .filter(|k| stores.contains(&k.name))
                    .map(|k| Held {
                        store: k.name,
                        bytes: k.bytes,
                        bound: k.bound,
                        charge: k.charge,
                        created_ns: k.created_ns,
                    })
                    .collect(),
            },
            Err(r) => refused(r),
        },
        Ask::Clear { store } => match vms.sandbox.clear_volume(store).await {
            Ok(()) => Answer::Cleared,
            Err(r) => refused(r),
        },
    }
}

/// The seam's refusal `r` as an answer's, by its cause. kiln names each volume by its store's grammar itself, so a
/// refusal of anything but the volume is its store's failing.
fn refused(r: Refusal) -> Answer {
    let why = r.to_string();
    let cause = match r {
        Refusal::Volume {
            cause: VolumeCause::InUse,
            ..
        } => Cause::InUse,
        Refusal::Volume {
            cause: VolumeCause::Unknown,
            ..
        } => Cause::Unknown,
        Refusal::Volume {
            cause: VolumeCause::Bound,
            ..
        } => Cause::Bound,
        Refusal::Volume {
            cause: VolumeCause::Clearing,
            ..
        } => Cause::Clearing,
        Refusal::Unsupported(_) => Cause::Unsupported,
        Refusal::Invalid { .. }
        | Refusal::NoSuchProgram(_)
        | Refusal::Full { .. }
        | Refusal::Ended
        | Refusal::Failed(_) => Cause::Failed,
    };
    Answer::Refused { cause, why }
}

/// `ask` of machine `machine`'s runner, waited for as a person's command waits for its runner (`hub::COMMAND_WITHIN`);
/// why not, where its runner names no `proto::feature::VOLUMES`, is away, or didn't answer in time.
async fn ask_machine(hub: &Hub, machine: &str, ask: &Ask) -> Result<Answer, String> {
    let json = serde_json::to_string(ask).map_err(|e| format!("its ask wasn't written: {e}"))?;
    match hub
        .ask_speaking::<Answer>(
            machine,
            crate::proto::feature::VOLUMES,
            crate::hub::COMMAND_WITHIN,
            move |req| crate::proto::ServerMsg::Volumes { req, json },
        )
        .await
    {
        Ok(Some(Some(answer))) => Ok(answer),
        Ok(Some(None)) => Err(format!("{} answered nothing", hub.shown(machine))),
        Ok(None) => Err(format!(
            "{}'s kiln keeps no agent's volumes it can be asked about: upgrade it (System → Updates)",
            hub.shown(machine)
        )),
        Err(e) => Err(format!("{e:#}")),
    }
}

/// The names of agent definition `id`'s volumes whose own the caller may see and clear: one of their own definitions,
/// or one of kiln's (whose volumes are each person's own too); another's is as none whoever asks, an admin too, as a
/// volume is its person's alone. Every name any of its versions declared, so one an older version kept is found too.
async fn names_of(hub: &Hub, who: &Who, id: &str) -> R<Vec<String>> {
    if let Some(b) = builtin::get(id) {
        return Ok(b.settings.volumes.into_iter().map(|v| v.name).collect());
    }
    let own = Who {
        person: who.person.clone(),
        all: false,
    };
    let mut names: Vec<String> = Vec::new();
    for version in super::versions(hub, &own, id).await? {
        for v in version.settings.volumes {
            if !names.contains(&v.name) {
                names.push(v.name);
            }
        }
    }
    Ok(names)
}

/// An agent's volumes as its person sees them ([`list`]): each machine's that keeps any, and the machines that
/// couldn't say.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Listed {
    pub machines: Vec<OnMachine>,
    pub unheard: Vec<Unheard>,
}

/// One machine's volumes of an agent for its person ([`list`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OnMachine {
    pub machine: String,
    pub volumes: Vec<Shown>,
}

/// One of an agent's volumes on a machine as its person sees it: its name in the definition; the bytes its files take;
/// its charge, what they take by its bound's own measure, which its bound holds it to; its bound, each none where its
/// store couldn't read it; and when it was made, in ms since the Unix epoch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Shown {
    pub name: String,
    pub bytes: Option<u64>,
    pub charge: Option<u64>,
    pub bound: Option<u64>,
    pub created_ms: u64,
}

/// A machine that couldn't say ([`list`]), and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Unheard {
    pub machine: String,
    pub why: String,
}

/// The caller's volumes of agent definition `id` ([`names_of`]) on each machine that keeps any (`machine`: that one
/// alone), as each machine's runner reads its store, all asked at once. A machine that holds no microVMs keeps none;
/// one that couldn't say is among `unheard`, saying why.
pub async fn list(hub: &Hub, who: &Who, id: &str, machine: Option<&str>) -> R<Listed> {
    let stores: Vec<(String, String)> = names_of(hub, who, id)
        .await?
        .into_iter()
        .map(|n| (store_name(&who.person, id, &n), n))
        .collect();
    let mut listed = Listed {
        machines: Vec::new(),
        unheard: Vec::new(),
    };
    if stores.is_empty() {
        return Ok(listed);
    }
    let mut machines: Vec<String> = match machine {
        Some(m) => vec![m.to_owned()],
        None => hub
            .agents
            .load()
            .values()
            .filter(|a| a.connected && !a.outpost && !a.ephemeral)
            .map(|a| a.name.clone())
            .collect(),
    };
    machines.sort();
    let ask = Ask::List {
        stores: stores.iter().map(|(store, _)| store.clone()).collect(),
    };
    let answers =
        futures::future::join_all(machines.iter().map(|m| ask_machine(hub, m, &ask))).await;
    for (machine, answer) in machines.into_iter().zip(answers) {
        match answer {
            Ok(Answer::Listed { volumes }) => {
                let shown: Vec<Shown> = volumes
                    .into_iter()
                    .filter_map(|h| {
                        let name = stores
                            .iter()
                            .find(|(store, _)| *store == h.store)
                            .map(|(_, name)| name.clone())?;
                        Some(Shown {
                            name,
                            bytes: h.bytes,
                            charge: h.charge,
                            bound: h.bound,
                            created_ms: h.created_ns / 1_000_000,
                        })
                    })
                    .collect();
                if !shown.is_empty() {
                    listed.machines.push(OnMachine {
                        machine,
                        volumes: shown,
                    });
                }
            }
            Ok(Answer::Refused {
                cause: Cause::Unsupported,
                ..
            }) => {}
            Ok(Answer::Refused { why, .. }) => listed.unheard.push(Unheard { machine, why }),
            Ok(Answer::Cleared) => listed.unheard.push(Unheard {
                machine,
                why: "it answered a listing with a clearing".into(),
            }),
            Err(why) => listed.unheard.push(Unheard { machine, why }),
        }
    }
    Ok(listed)
}

/// The caller's volume `name` of agent definition `id` cleared on machine `machine`: its files removed there, so the
/// agent's next session there finds it empty. Refused, typed by its cause ([`Error::Volume`]): while a session holds
/// it (its files kept), where that machine keeps none of that name, and where its store couldn't; `Unavailable` where
/// that machine can't be asked.
pub async fn clear(hub: &Hub, who: &Who, id: &str, machine: &str, name: &str) -> R<()> {
    if let Some(why) = name_problem(name) {
        return Err(Error::Bad(why));
    }
    names_of(hub, who, id).await?;
    let ask = Ask::Clear {
        store: store_name(&who.person, id, name),
    };
    match ask_machine(hub, machine, &ask)
        .await
        .map_err(Error::Unavailable)?
    {
        Answer::Cleared => Ok(()),
        Answer::Refused { cause, why } => Err(Error::Volume {
            cause,
            why: format!(
                "volume {name} on {} {}: {why}",
                hub.shown(machine),
                cause.words()
            ),
        }),
        Answer::Listed { .. } => Err(Error::Unavailable(format!(
            "{} answered a clearing with a listing",
            hub.shown(machine)
        ))),
    }
}

/// What `given`, a session's volumes, hold already on every one of `machines`, those its placement may choose (ROOT's
/// decision on Hex's P6.2 question, 2026-11-16): on each, each volume's charge there (what its files take by its bound's
/// own measure, which its bound holds it to), at most its bound, together; the least of those over `machines`. A
/// machine's free disk counts what its volumes hold as used already, so its placement asks of it only what they may
/// still grow by, never those bytes twice; the least over every machine it may go to asks none of them less than its
/// volumes may still take there. A machine that can't say (its kiln older, away, its store failing, a charge unread)
/// holds none of them, as far as its placement knows. Each is asked as a person's view asks it, all at once: a listing
/// walks its volumes' files, which a start of an agent with volumes waits on.
pub async fn held_on(hub: &Hub, machines: &[String], given: &[Given]) -> u64 {
    if given.is_empty() || machines.is_empty() {
        return 0;
    }
    let ask = Ask::List {
        stores: given.iter().map(|g| g.store.clone()).collect(),
    };
    let answers =
        futures::future::join_all(machines.iter().map(|m| ask_machine(hub, m, &ask))).await;
    answers
        .into_iter()
        .map(|a| match a {
            Ok(Answer::Listed { volumes }) => held_in(given, &volumes),
            Ok(Answer::Cleared | Answer::Refused { .. }) | Err(_) => 0,
        })
        .min()
        .unwrap_or(0)
}

/// What `given` hold already of their bounds in `listed`, one machine's listing ([`held_on`]): each one's charge there,
/// at most its bound, together. One unlisted, or whose charge was unread, holds none.
fn held_in(given: &[Given], listed: &[Held]) -> u64 {
    given
        .iter()
        .map(|g| {
            listed
                .iter()
                .find(|h| h.store == g.store)
                .and_then(|h| h.charge)
                .map_or(0, |charge| charge.min(g.bytes))
        })
        .fold(0, u64::saturating_add)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn volume(name: &str, bytes: u64) -> Volume {
        Volume {
            name: name.to_owned(),
            bytes,
        }
    }

    #[test]
    fn p6_2_an_agents_volumes_each_have_a_name_of_their_own_and_a_bound() {
        assert!(
            problems(&[
                volume("cache", 1 << 30),
                volume("cargo.registry", 1),
                volume("9-go_mod", 1 << 40)
            ])
            .is_empty()
        );
        for bad in [
            "", ".", "..", "-cache", "_cache", ".cache", "ca/che", "ca che", "cäche", "ca\0che",
        ] {
            assert_eq!(
                problems(&[volume(bad, 1)]).len(),
                1,
                "{bad:?} is refused as a name"
            );
        }
        assert!(
            problems(&[volume(&"a".repeat(NAME_MAX), 1)]).is_empty(),
            "NAME_MAX bytes is one file name"
        );
        assert_eq!(
            problems(&[volume(&"a".repeat(NAME_MAX + 1), 1)]).len(),
            1,
            "past NAME_MAX it is none"
        );
        assert_eq!(
            problems(&[volume("cache", 1), volume("cache", 2)]),
            ["two volumes are named cache"]
        );
        assert_eq!(
            problems(&[volume("cache", 0)]).len(),
            1,
            "a bound of 0 holds nothing"
        );
        assert_eq!(
            problems(&[volume("a", u64::MAX), volume("b", 1)]).len(),
            1,
            "bounds past a u64 are refused, never wrapped"
        );
    }

    #[test]
    fn p6_2_a_sessions_disk_ask_is_its_volumes_bounds_together() {
        assert_eq!(bytes(&[]), Some(0));
        assert_eq!(bytes(&[volume("a", 3), volume("b", 4)]), Some(7));
        assert_eq!(bytes(&[volume("a", u64::MAX), volume("b", 1)]), None);
    }

    #[test]
    fn p6_2_an_agents_volume_is_its_persons_own_and_found_by_the_same_name_each_session() {
        let one = store_name("user:a", "d1", "cache");
        assert_eq!(
            one,
            store_name("user:a", "d1", "cache"),
            "the next session finds it by the same name"
        );
        assert_ne!(
            one,
            store_name("user:b", "d1", "cache"),
            "another person's session has its own"
        );
        assert_ne!(
            one,
            store_name("user:a", "d2", "cache"),
            "another definition has its own"
        );
        assert_ne!(
            one,
            store_name("user:a", "d1", "cargo"),
            "another volume of the definition is another"
        );
        assert_ne!(
            store_name("user:a", "d1x", "y"),
            store_name("user:a", "d1", "xy"),
            "parts never run into each other"
        );
        let hex = one.strip_prefix("kiln-agent-").unwrap_or_default();
        assert!(
            hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()),
            "{one}"
        );
        assert!(name_problem(&one).is_none(), "Shards' grammar takes {one}");
    }

    /// P6.2 (ROOT's decision on Hex's P6.2 question): what a session's volumes hold already on a machine is each one's
    /// charge there, at most its bound, together; one unlisted there, or whose charge its store couldn't read, holds
    /// none, and another volume of the machine's counts for none of them.
    #[test]
    fn p6_2_a_machine_holds_of_a_sessions_volumes_their_charges_at_most_their_bounds() {
        let given = given(
            "user:a",
            "d1",
            &[
                volume("cache", 100),
                volume("tools", 50),
                volume("notes", 10),
            ],
        );
        let row = |store: &str, charge: Option<u64>| Held {
            store: store.to_owned(),
            bytes: charge,
            bound: None,
            charge,
            created_ns: 0,
        };
        let listed = [
            row(&store_name("user:a", "d1", "cache"), Some(40)),
            row(&store_name("user:a", "d1", "tools"), Some(70)),
            row(&store_name("user:a", "d1", "notes"), None),
            row(&store_name("user:b", "d1", "cache"), Some(1000)),
        ];
        assert_eq!(
            held_in(&given, &listed),
            40 + 50,
            "cache's charge, tools' bound (its charge is past it), notes' none (unread), and another person's none"
        );
        assert_eq!(held_in(&given, &[]), 0, "a machine with none of them");
        assert_eq!(held_in(&[], &listed), 0, "a session with no volumes");
    }

    /// P6.2: a runner's refusal of a listing or a clearing keeps its cause, the seam's volume causes by name; a machine
    /// that keeps none says so, and anything else is its store's failing. Its ask and answer read back whole.
    #[test]
    fn p6_2_a_runners_refusal_keeps_its_cause() -> Result<(), serde_json::Error> {
        let held_by = |cause| Refusal::Volume {
            cause,
            why: "kiln-agent-x".into(),
        };
        let cause_of = |a: Answer| match a {
            Answer::Refused { cause, .. } => Some(cause),
            Answer::Listed { .. } | Answer::Cleared => None,
        };
        for (r, cause) in [
            (held_by(VolumeCause::InUse), Cause::InUse),
            (held_by(VolumeCause::Unknown), Cause::Unknown),
            (held_by(VolumeCause::Bound), Cause::Bound),
            (held_by(VolumeCause::Clearing), Cause::Clearing),
            (Refusal::Unsupported("none".into()), Cause::Unsupported),
            (
                Refusal::Invalid {
                    what: "its microVM",
                    why: "shards failed".into(),
                },
                Cause::Failed,
            ),
        ] {
            assert_eq!(cause_of(refused(r)), Some(cause));
        }
        for ask in [
            Ask::List {
                stores: vec![store_name("user:a", "d1", "cache")],
            },
            Ask::Clear {
                store: store_name("user:a", "d1", "cache"),
            },
        ] {
            assert_eq!(
                serde_json::from_str::<Ask>(&serde_json::to_string(&ask)?)?,
                ask
            );
        }
        let answer = Answer::Refused {
            cause: Cause::InUse,
            why: "held".into(),
        };
        assert_eq!(
            serde_json::from_str::<Answer>(&serde_json::to_string(&answer)?)?,
            answer
        );
        Ok(())
    }

    #[test]
    fn p6_2_a_session_is_given_each_volume_by_its_store_name_beneath_the_live_plug_root() {
        let given = given("user:a", "d1", &[volume("cache", 1)]);
        assert_eq!(
            given,
            [Given {
                name: "cache".into(),
                store: store_name("user:a", "d1", "cache"),
                at: "/volumes/cache".into(),
                bytes: 1
            }]
        );
    }
}
