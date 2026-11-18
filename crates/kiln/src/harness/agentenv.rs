//! A session's environment (docs/agents.md, "Environments"): a person's named set of variables,
//! as the server resolves it for a session's runner ([`Resolved`]), and what a machine's user has,
//! by name alone ([`MachineVar`]). Decided by the user (2026-11-18).
//!
//! The server keeps environments and opens their secrets; the runner reads `machine` variables from
//! its user's environment as the harness starts and layers the whole over the base
//! (`crate::harness::kinds::set_env`). A secret's value is in [`Resolved`] alone, which its
//! `Debug` never shows, and which the runner keeps in memory only (never in its record of sessions,
//! a move or the session's state).

use serde::{Deserialize, Serialize};

/// What a session's environment starts from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum Base {
    /// The machine's user's environment (docs/agents.md, "Where a harness is found").
    #[default]
    User,
    /// Only what a harness needs to run (docs/agents.md, "What one is").
    Clean,
}

/// Where a variable's value comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum From {
    /// The machine's user's variable `of` (its own name when none), read on the machine.
    Machine,
    /// A value kept as it is, shown to its owner.
    Value,
    /// The owner's own secret (`secrets.mine`), by name.
    Secret,
    /// Taken away from what the base gives.
    Unset,
}

/// One variable of an environment as its owner wrote it: what the server keeps and the UI shows
/// (a secret by its name, never its value).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Var {
    pub name: String,
    pub from: From,
    /// `machine`: the machine's variable it takes, when not its own name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub of: Option<String>,
    /// `value`: its value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub value: Option<String>,
    /// `secret`: the owner's secret's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub secret: Option<String>,
}

/// One variable as a session's runner gets it: a secret's value opened.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Set {
    /// The machine's user's variable of this name.
    Machine(String),
    /// This value (a `value` variable's, or a secret's).
    Value(String),
    Unset,
}

impl std::fmt::Debug for Set {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Machine(of) => f.debug_tuple("Machine").field(of).finish(),
            // A secret's value may be one: never written out.
            Self::Value(_) => f.write_str("Value(..)"),
            Self::Unset => f.write_str("Unset"),
        }
    }
}

/// A session's environment as its runner gets it (`Spec::agentenv`, `Command::Agentenv`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resolved {
    pub id: String,
    pub name: String,
    pub version: u64,
    pub base: Base,
    /// In order: a later one of a name wins.
    pub vars: Vec<(String, Set)>,
}

/// Where a machine's user's variable is set (docs/agents.md, "What a machine's user has").
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum Source {
    LoginShell,
    Launchd,
    SystemdUser,
    Pam,
    UserRegistry,
    SystemRegistry,
    Runner,
}

/// A variable a machine's user has, by name alone, and where it's set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct MachineVar {
    pub name: String,
    pub sources: Vec<Source>,
}

/// What the server asks a runner (`ServerMsg::UserEnv`): its user's variables.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ask {}

/// Its answer (`AgentMsg::Answer`): its user's variables, by name.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Answer {
    pub vars: Vec<MachineVar>,
}

/// This machine's user's variables, by name, and where each is set: the runner's own process's and
/// its user's login shell's (`crate::reach::serve::login_env`), kiln's own (`KILN_*`) left out.
pub async fn answer(_: Ask) -> Answer {
    let login = crate::reach::serve::login_env().await;
    let mut by: std::collections::BTreeMap<String, Vec<Source>> = std::collections::BTreeMap::new();
    for (name, source) in std::env::vars_os().filter_map(|(k, _)| k.into_string().ok()).map(|k| (k, Source::Runner)).chain(login.into_iter().map(|(k, _)| (k, Source::LoginShell))) {
        if !name.starts_with("KILN_") {
            let sources = by.entry(name).or_default();
            if !sources.contains(&source) {
                sources.push(source);
            }
        }
    }
    Answer { vars: by.into_iter().map(|(name, mut sources)| (name, { sources.sort(); sources })).map(|(name, sources)| MachineVar { name, sources }).collect() }
}

/// Whether `name` is a variable's name an environment may hold: a portable one
/// (`[A-Za-z_][A-Za-z0-9_]*`, POSIX.1-2017 XBD 8.1's portable set, which Windows takes too).
pub fn valid_name(name: &str) -> bool {
    let mut cs = name.chars();
    cs.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && cs.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_variables_name_is_a_portable_one() {
        for ok in ["ANTHROPIC_API_KEY", "_x", "a1"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in ["", "1A", "A-B", "A B", "A=B", "É"] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn a_resolved_values_debug_never_shows_it() {
        let r = Resolved { id: "e".into(), name: "n".into(), version: 1, base: Base::User, vars: vec![("K".into(), Set::Value("sk-ant-secret".into())), ("M".into(), Set::Machine("HOME".into()))] };
        let shown = format!("{r:?}");
        assert!(!shown.contains("sk-ant-secret"), "{shown}");
        assert!(shown.contains("HOME"), "{shown}");
    }
}
