//! Where an environment applies (docs/agents.md, "Scopes, stacks, and applied on its own"; decided by the user,
//! 2026-11-19): its scope, chips each `{key, value}` ([`Chip`]), matched against what a session is ([`Facts`]). Chips of
//! one key are any of them; chips of different keys are all of them; no chips, every session. A key whose fact a session
//! hasn't (an `agent` chip for a session from no definition, an `effort` chip for one whose harness has none) fails that
//! key. The UI matches by the same rule, held to the same cases (`testdata/scope-cases.json`).

use serde::{Deserialize, Serialize};

use crate::harness::session::Info;
use crate::hub::Hub;

/// What a chip matches a session by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AgentenvScopeKey"))]
#[serde(rename_all = "snake_case")]
pub enum ScopeKey {
    /// Its runner's machine, by id (`local`: the server).
    Machine,
    /// Its machine's system: `linux`, `macos`, `windows`.
    Os,
    /// Its machine's architecture, as OCI names it: `amd64`, `arm64`.
    Arch,
    /// Its harness, by id (`claude`, `codex`, …).
    Harness,
    /// The agent definition it started from, by id (`kiln:<name>` for kiln's own).
    Agent,
    /// Its model, as the session names it.
    Model,
    /// Its harness's permission mode, `<harness>:<mode>`.
    Permission,
    /// Its harness's effort, `<harness>:<effort>`.
    Effort,
}

impl ScopeKey {
    pub const ALL: [ScopeKey; 8] = [Self::Machine, Self::Os, Self::Arch, Self::Harness, Self::Agent, Self::Model, Self::Permission, Self::Effort];
}

/// One chip of a scope.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AgentenvChip"))]
pub struct Chip {
    pub key: ScopeKey,
    pub value: String,
}

/// What a session is, as scopes see it: what it was as it started, or is now (`agentenvs.session`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AgentenvFacts"))]
pub struct Facts {
    /// Its runner's machine, by id (`local`: the server).
    pub machine: String,
    /// `linux`, `macos` or `windows`.
    pub os: String,
    /// `amd64` or `arm64`.
    pub arch: String,
    /// Its harness's id.
    pub harness: String,
    /// The agent definition it started from; none, it started from none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub agent: Option<String>,
    /// Its model ("" while it names none: its harness's own).
    pub model: String,
    /// `<harness>:<mode>`.
    pub permission: String,
    /// `<harness>:<effort>`; none where its harness says none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub effort: Option<String>,
}

impl Facts {
    /// Its fact for `key`; none where it has none.
    pub fn get(&self, key: ScopeKey) -> Option<&str> {
        match key {
            ScopeKey::Machine => Some(&self.machine),
            ScopeKey::Os => Some(&self.os),
            ScopeKey::Arch => Some(&self.arch),
            ScopeKey::Harness => Some(&self.harness),
            ScopeKey::Agent => self.agent.as_deref(),
            ScopeKey::Model => Some(&self.model),
            ScopeKey::Permission => Some(&self.permission),
            ScopeKey::Effort => self.effort.as_deref(),
        }
    }
}

/// Whether `scope` matches a session that is `facts`: for each key it has chips of, one of them is that fact.
pub fn matches(scope: &[Chip], facts: &Facts) -> bool {
    ScopeKey::ALL.iter().all(|&key| {
        let mut of_key = scope.iter().filter(|c| c.key == key).peekable();
        of_key.peek().is_none() || facts.get(key).is_some_and(|f| of_key.any(|c| c.value == f))
    })
}

/// What's wrong with `scope`, if anything ([`super::Refusal::BadVar`]): each value non-empty, on one line, without
/// control characters; a `permission` or `effort` value names its harness (`<harness>:<value>`).
pub fn problem(scope: &[Chip]) -> Option<String> {
    for c in scope {
        let key = serde_json::to_value(c.key).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default();
        if c.value.trim().is_empty() {
            return Some(format!("a `{key}` chip needs a value"));
        }
        if c.value.chars().any(char::is_control) {
            return Some(format!("a `{key}` chip's value is words on one line, without control characters"));
        }
        if matches!(c.key, ScopeKey::Permission | ScopeKey::Effort) && !c.value.split_once(':').is_some_and(|(h, v)| !h.is_empty() && !v.is_empty()) {
            return Some(format!("a `{key}` chip names its harness: `<harness>:<{key}>` (`claude:plan`), not {:?}", c.value));
        }
    }
    None
}

/// `scope` as kept: each value trimmed, each chip once, in the order given.
pub fn kept(scope: Vec<Chip>) -> Vec<Chip> {
    let mut out: Vec<Chip> = Vec::with_capacity(scope.len());
    for c in scope {
        let c = Chip { key: c.key, value: c.value.trim().to_owned() };
        if !out.contains(&c) {
            out.push(c);
        }
    }
    out
}

/// Docker's and OCI's word for a CPU Rust names (`x86_64` is `amd64`, `aarch64` `arm64`).
fn oci_arch(cpu: &str) -> &str {
    match cpu {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    }
}

/// The machine of `target` (`local`, `agent:<machine>`, or none: the server's), with its system and architecture as its
/// runner said them (the server's own for `local`); "" where kiln doesn't know them.
pub fn machine_of(hub: &Hub, target: &str) -> (String, String, String) {
    match target.strip_prefix("agent:") {
        Some(m) => {
            let live = hub.agents.load().get(m).map(|a| (a.os.clone(), oci_arch(&a.arch).to_owned()));
            let (os, arch) = live.unwrap_or_default();
            (m.to_owned(), os, arch)
        }
        None => ("local".to_owned(), std::env::consts::OS.to_owned(), oci_arch(std::env::consts::ARCH).to_owned()),
    }
}

/// What a session of harness `kind` on `target` is as it starts: from the definition `agent` (by id), with `model`,
/// `mode` (none or "", its harness's own) and `effort` as it starts with them.
pub fn at_start(hub: &Hub, target: &str, kind: &str, agent: Option<&str>, model: Option<&str>, mode: Option<&str>, effort: Option<&str>) -> Facts {
    let (machine, os, arch) = machine_of(hub, target);
    let mode = match mode.map(str::trim).filter(|m| !m.is_empty()) {
        Some(m) => m.to_owned(),
        None => crate::harness::kinds::Kind::get(kind).map(|k| crate::harness::lineage::default_mode(k).to_owned()).unwrap_or_default(),
    };
    Facts {
        machine,
        os,
        arch,
        harness: kind.to_owned(),
        agent: agent.map(str::to_owned).filter(|a| !a.is_empty()),
        model: model.map(str::trim).unwrap_or_default().to_owned(),
        permission: format!("{kind}:{mode}"),
        effort: effort.map(str::trim).filter(|e| !e.is_empty()).map(|e| format!("{kind}:{e}")),
    }
}

/// The agent definition session `session` started from, by id (`agentdefs::sessions`): none, it started from none.
pub async fn definition_of(hub: &Hub, session: &str) -> anyhow::Result<Option<String>> {
    use rusqlite::OptionalExtension as _;
    let s = session.to_owned();
    hub.store.read(move |c| Ok(c.query_row("SELECT agent_def_id FROM agent_def_sessions WHERE session_id = ?1", [s], |r| r.get(0)).optional()?)).await
}

/// What running session `info` is now: its machine, harness and definition, and its model, mode and effort as its
/// harness last said them.
pub async fn of_session(hub: &Hub, info: &Info) -> anyhow::Result<Facts> {
    let agent = definition_of(hub, &info.id).await?;
    let effort = hub.harness.state(&info.id).map(|s| s.effort).filter(|e| !e.is_empty());
    let model = Some(info.model.as_str()).filter(|m| !m.is_empty());
    Ok(at_start(hub, &info.target, &info.kind, agent.as_deref(), model, Some(&info.mode), effort.as_deref()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct Case {
        name: String,
        scope: Vec<Chip>,
        facts: Facts,
        matches: bool,
    }

    /// The cases the server and the UI both match by (`testdata/scope-cases.json`): each as it says.
    #[test]
    fn scopes_match_the_shared_cases() -> anyhow::Result<()> {
        let cases: Vec<Case> = serde_json::from_str(include_str!("testdata/scope-cases.json"))?;
        anyhow::ensure!(cases.len() >= 17, "{} cases", cases.len());
        for c in &cases {
            anyhow::ensure!(matches(&c.scope, &c.facts) == c.matches, "{}: matched {}", c.name, !c.matches);
        }
        Ok(())
    }

    /// A chip's value is a word on one line; `permission` and `effort` name their harness; kept trimmed, each once.
    #[test]
    fn a_scopes_chips_are_checked_and_kept_once() -> anyhow::Result<()> {
        let chip = |key, value: &str| Chip { key, value: value.into() };
        assert_eq!(problem(&[chip(ScopeKey::Os, "linux"), chip(ScopeKey::Permission, "claude:plan"), chip(ScopeKey::Effort, "codex:high")]), None);
        for bad in [chip(ScopeKey::Os, " "), chip(ScopeKey::Model, "a\nb"), chip(ScopeKey::Permission, "plan"), chip(ScopeKey::Effort, "codex:"), chip(ScopeKey::Permission, ":plan")] {
            assert!(problem(std::slice::from_ref(&bad)).is_some(), "{bad:?}");
        }
        assert_eq!(kept(vec![chip(ScopeKey::Os, " linux "), chip(ScopeKey::Os, "linux"), chip(ScopeKey::Arch, "arm64")]), [chip(ScopeKey::Os, "linux"), chip(ScopeKey::Arch, "arm64")]);
        let unknown: Result<Chip, _> = serde_json::from_value(serde_json::json!({"key": "colour", "value": "red"}));
        anyhow::ensure!(unknown.is_err(), "a key kiln doesn't know is refused");
        Ok(())
    }

    /// As a session starts: its mode, where it names none, its harness's own; its effort and permission name their
    /// harness; the server's machine is `local`, its architecture OCI's word.
    #[test]
    fn a_starts_facts_name_their_harness() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let f = at_start(&hub, "local", "claude", Some("kiln:reviewer"), Some("claude-opus-5-5"), None, Some("high"));
            anyhow::ensure!(f.machine == "local" && f.os == std::env::consts::OS && matches!(f.arch.as_str(), "amd64" | "arm64"), "{f:?}");
            anyhow::ensure!(f.permission == "claude:default" && f.effort.as_deref() == Some("claude:high") && f.agent.as_deref() == Some("kiln:reviewer"), "{f:?}");
            hub.put_agent(crate::hub::tests::live_runner("box-1", "linux", "x86_64"));
            let f = at_start(&hub, "agent:box-1", "codex", None, None, Some("read-only"), Some(""));
            anyhow::ensure!(f.machine == "box-1" && f.os == "linux" && f.arch == "amd64" && f.permission == "codex:read-only" && f.effort.is_none() && f.agent.is_none(), "{f:?}");
            Ok(())
        })
    }
}
