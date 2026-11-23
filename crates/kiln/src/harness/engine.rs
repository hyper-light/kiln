//! How agent sessions run, and whether a runner can run them in microVMs (the person's decision of 2026-11-23: the
//! engine is each session's choice, never its runner's; docs/isolation.md, "Each session's engine"). A session that
//! asks for a microVM (its spec's `isolation`, `{kind: "microvm", image}`, from an Agentfile) runs in a Shards microVM
//! of its own, through Hex (`crate::sandbox::hexvm`). One that asks for none runs as a process of its machine's host:
//! the default. An agent definition says its sessions' engine ([`Engine`], `crate::agentdefs`).
//!
//! A session that asks for a microVM is placed on, and admitted by, a runner whose facts say it can run microVMs now
//! ([`capable`]: `isolation.available`, which `virt.microvm` says too), and whose Hex node lists the class image VMs run
//! as, or doesn't say ([`node_images`]). Elsewhere it's refused, saying why (`crate::harness::lineage::NoMicrovm`). A
//! session on its host runs on any runner.

use kiln_ci::facts::RunnerFacts;

pub use kiln_ci::facts::Engine;

/// Whether a runner can run microVMs now, and why not: the one place kiln reads that from.
///
/// From its facts as its runner says them: its kiln carries Shards, and Shards' own check of its host says it
/// runs microVMs now (`shards_api::host_check`'s `{backend, usable, why}` as `isolation`'s `hypervisor`,
/// `available` and `why_not`: `crate::sandbox::facts`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capable {
    pub usable: bool,
    pub why: Option<String>,
}

/// Why a runner whose facts say nothing of microVMs (a runner from before them) runs none.
pub const NOT_SAID: &str = "its runner doesn't say whether it starts microVMs: its kiln is from before them";

/// What `facts` say the runner can do now ([`Capable`]).
pub fn capable(facts: &RunnerFacts) -> Capable {
    match &facts.isolation {
        Some(i) if i.available => Capable { usable: true, why: None },
        Some(i) => Capable { usable: false, why: Some(i.why_not.clone().unwrap_or_else(|| crate::sandbox::NO_MICROVM.to_owned())) },
        None => Capable { usable: false, why: Some(NOT_SAID.to_owned()) },
    }
}

/// `facts` as the server keeps them, whatever kiln their runner runs: `virt.microvm` what it can do now
/// ([`capable`]), and no engine of its own (`isolation.engine`, retired). A runner from before sessions chose their
/// engine writes the engine its owner set into both: `virt.microvm` false where that was `host`, though it can run
/// microVMs. Placement, admission's word on the server and the runners API read these.
pub fn as_capable(facts: &mut RunnerFacts) {
    facts.virt.microvm = capable(facts).usable;
    if let Some(i) = facts.isolation.as_mut() {
        i.engine = None;
    }
}

/// Whether a runner's Hex node lists the class image VMs run as (Hex's `shards-image`, P2.2), as its
/// facts say (`isolation.image_class`, which its runner reads from its node: `crate::sandbox::facts_of`).
/// A runner that can run microVMs but whose node lists none is refused a session that asks for one, in Hex's words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeImages {
    /// Its node lists the class.
    Listed,
    /// Its node lists no such class, `why` in Hex's words.
    NotListed { why: String },
    /// Its facts don't say: a runner holding no microVMs through Hex, or one from before the class,
    /// placed by what it can do alone.
    Unknown,
}

/// What `facts` say of their node's image class ([`NodeImages`]).
pub fn node_images(facts: &RunnerFacts) -> NodeImages {
    match facts.isolation.as_ref().and_then(|i| i.image_class.as_ref()) {
        Some(kiln_ci::facts::NodeImageClass::Listed) => NodeImages::Listed,
        Some(kiln_ci::facts::NodeImageClass::Unlisted { why }) => NodeImages::NotListed { why: why.clone() },
        None => NodeImages::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_ci::facts::Isolation;

    fn facts(isolation: Option<Isolation>) -> RunnerFacts {
        RunnerFacts { isolation, ..RunnerFacts::default() }
    }

    fn able() -> Isolation {
        Isolation { available: true, hypervisor: Some("Hypervisor.framework".into()), why_not: None, engine: None, image_class: None }
    }

    fn unable(why: &str) -> Isolation {
        Isolation { available: false, hypervisor: None, why_not: Some(why.into()), engine: None, image_class: None }
    }

    /// The person's decision of 2026-11-23: what a runner can do is read from its isolation facts alone. Do read the
    /// facts of a runner that can run microVMs, one that can't and says why, one that can't and says nothing of why, and
    /// one from before isolation facts. Expect the first usable; the second not, in its own words; the third not, in
    /// kiln's; the last not, saying its kiln is from before them.
    #[test]
    fn what_a_runner_can_do_is_read_from_its_isolation_facts() {
        assert_eq!(capable(&facts(Some(able()))), Capable { usable: true, why: None });
        assert_eq!(capable(&facts(Some(unable("no hypervisor")))), Capable { usable: false, why: Some("no hypervisor".into()) });
        let unsaid = Isolation { why_not: None, ..unable("") };
        assert_eq!(capable(&facts(Some(unsaid))), Capable { usable: false, why: Some(crate::sandbox::NO_MICROVM.into()) });
        assert_eq!(capable(&facts(None)), Capable { usable: false, why: Some(NOT_SAID.into()) }, "a runner from before microVMs");
    }

    /// The person's decision of 2026-11-23: a runner has no engine, so the facts the server keeps say what it can do,
    /// whatever kiln it runs. Do read in the facts a runner from before writes (its owner's `host` over a machine that
    /// can run microVMs: `virt.microvm` false and its engine named; its owner's `microvm` over one that can't any more:
    /// `virt.microvm` true), and this kiln's own. Expect `virt.microvm` what each can do, no engine named, and the rest
    /// of their isolation facts as they said them. Taking the engine's overlay out of placement is what made this hold:
    /// written as before, the first stays false and the second true.
    #[test]
    fn the_facts_kept_say_what_a_runner_can_do_and_no_engine_of_its_own() {
        let mut set_to_host = facts(Some(Isolation { engine: Some("host".into()), ..able() }));
        set_to_host.virt.microvm = false;
        as_capable(&mut set_to_host);
        assert_eq!((set_to_host.virt.microvm, set_to_host.isolation.as_ref().and_then(|i| i.engine.clone())), (true, None));
        assert_eq!(set_to_host.isolation.as_ref().map(|i| (i.available, i.hypervisor.as_deref())), Some((true, Some("Hypervisor.framework"))));
        let mut lost = facts(Some(Isolation { engine: Some("microvm".into()), ..unable("no hypervisor") }));
        lost.virt.microvm = true;
        as_capable(&mut lost);
        assert_eq!((lost.virt.microvm, lost.isolation.as_ref().and_then(|i| i.why_not.clone())), (false, Some("no hypervisor".into())));
        let mut ours = facts(Some(able()));
        ours.virt.microvm = true;
        as_capable(&mut ours);
        assert!(ours.virt.microvm, "this kiln's own say it already");
        let mut older = facts(None);
        older.virt.microvm = true;
        as_capable(&mut older);
        assert!(!older.virt.microvm, "a runner from before isolation facts runs none");
    }

    /// A runner from before sessions chose their engine says the engine its owner set, `container` among them: its
    /// facts are read all the same, that word taken and left unread.
    #[test]
    fn a_runner_from_before_is_read_whatever_engine_it_names() -> anyhow::Result<()> {
        for engine in ["host", "microvm", "container"] {
            let said = serde_json::json!({"os": "linux", "arch": "x86_64", "virt": {"microvm": false}, "isolation": {"available": true, "engine": engine}});
            let mut f: RunnerFacts = serde_json::from_value(said)?;
            as_capable(&mut f);
            anyhow::ensure!(f.virt.microvm && f.isolation.as_ref().is_some_and(|i| i.available && i.engine.is_none()), "{engine}: {f:?}");
        }
        Ok(())
    }

    /// P2.4 (on Hex's P2.2): placement reads whether a runner's Hex node lists the class image VMs run as from what its
    /// runner said of its node (`isolation.image_class`). Do read facts saying it's listed, that it isn't and why, that
    /// say nothing of it, and none at all. Expect it listed; not listed, in Hex's words; and unknown for the last two,
    /// whose runners are placed by what they can do alone.
    #[test]
    fn p2_4_node_images_is_what_the_runner_said_of_its_hex_node() {
        use kiln_ci::facts::NodeImageClass;
        let said = |class: Option<NodeImageClass>| facts(Some(Isolation { image_class: class, ..able() }));
        assert_eq!(node_images(&said(Some(NodeImageClass::Listed))), NodeImages::Listed);
        let why = "Shards image VMs on linux: cannot open /dev/kvm: permission denied";
        assert_eq!(node_images(&said(Some(NodeImageClass::Unlisted { why: why.into() }))), NodeImages::NotListed { why: why.into() });
        assert_eq!(node_images(&said(None)), NodeImages::Unknown);
        assert_eq!(node_images(&facts(None)), NodeImages::Unknown, "a runner from before microVMs");
    }
}
