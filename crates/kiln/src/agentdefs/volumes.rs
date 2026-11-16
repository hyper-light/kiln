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
//! - **Given** to its session's harness by that name as it starts in its microVM (`sandbox::isolated::launch`: a
//!   `Change::Plug` of `PlugSource::Volume`, which Shards makes where it isn't there, D271); a session on its machine's
//!   host is refused (`harness::lineage`), as it has no store of Shards'.
//! - **Admitted**: the bytes its volumes may hold ([`bytes`]) are what its placement asks of its machine's disk
//!   (`Isolation::disk`, `harness::lineage::session_needs`), so it starts only where the disk has room for them.

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
/// the session's machine ([`store_name`]), and where its harness sees it ([`at`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Given {
    pub name: String,
    pub store: String,
    pub at: String,
}

/// `volumes` of agent definition `definition` as a session acting for `person` is given them.
pub fn given(person: &str, definition: &str, volumes: &[Volume]) -> Vec<Given> {
    volumes
        .iter()
        .map(|v| Given {
            name: v.name.clone(),
            store: store_name(person, definition, &v.name),
            at: at(&v.name),
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

    #[test]
    fn p6_2_a_session_is_given_each_volume_by_its_store_name_beneath_the_live_plug_root() {
        let given = given("user:a", "d1", &[volume("cache", 1)]);
        assert_eq!(
            given,
            [Given {
                name: "cache".into(),
                store: store_name("user:a", "d1", "cache"),
                at: "/volumes/cache".into()
            }]
        );
    }
}
