//! The environment kiln gives an isolated session's harness in its microVM, over its image's own
//! (`harness.json`'s `run.env`, which Shards applies; D231's `member-env`). On its machine a
//! harness gets kiln's own variables and nothing else of kiln's (`harness::kinds::set_env`): its
//! mark, `KILN_AGENT`, and the sockets of its machine's runner. In a microVM it gets the same
//! variables where they mean the same thing there, and none that names its machine: each of
//! [`HOST`]'s variables is decided, a test holding every one `set_env` gives a harness to it.
//!
//! Nothing of the machine's own environment goes, nor of the shell `kiln spawn` took the
//! environment of: its paths are the machine's (`HOME=/Users/…`, its `PATH`), which name nothing in
//! the microVM, and its `KILN_*` are the server's or runner's (a data folder, an admin token).

/// What becomes of one of kiln's variables a harness on its machine gets, in a microVM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fate {
    /// Given as on its machine.
    Given,
    /// Left out: it names what isn't in the microVM, and this is what does its work there.
    LeftOut(&'static str),
}

/// Each of kiln's own variables a harness gets on its machine, and its fate in a microVM.
pub const HOST: &[(&str, Fate)] = &[
    // The session's id, which everything it starts carries: the mark that says whose a process is.
    ("KILN_AGENT", Fate::Given),
    ("KILN_REACH_SOCK", Fate::LeftOut("a socket of its machine's runner, which its microVM can't reach: its `kiln run` is kiln's MCP `run` there")),
    ("KILN_HARNESS_SOCK", Fate::LeftOut("a socket of its machine, which its microVM can't reach: its `kiln spawn` and `kiln agents` are kiln's MCP `agents_*` there")),
    ("KILN_RUNNER_SOCK", Fate::LeftOut("a socket of its runner, which its microVM can't reach: what its hooks ask comes over its microVM's control lane (kiln plan steps 10 and 11)")),
    ("KILN_REACH_ON", Fate::LeftOut("the runner `kiln reach mcp` prefers, which kiln's MCP keeps for the session itself")),
    ("KILN_SESSION", Fate::LeftOut("whom `kiln reach mcp` asks for; kiln's MCP in a microVM knows the session by the link it rides, the session's own (its `mcp` lane, Shards D254)")),
];

/// The environment for session `session`'s harness of `kind` in its microVM: kiln's variables
/// that go ([`HOST`]), and what kiln's adapter has its harness run with (Claude Code's file
/// checkpoints).
pub fn member_env(session: &str, kind: &crate::harness::kinds::Kind) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = HOST.iter().filter(|(_, f)| *f == Fate::Given).map(|(k, _)| ((*k).to_owned(), session.to_owned())).collect();
    if kind.protocol == crate::harness::kinds::Protocol::ClaudeStreamJson {
        out.extend(crate::harness::claude::FIXED_ENV.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{Fate, HOST, member_env};
    use crate::harness::kinds::Kind;

    /// Every `KILN_*` variable kiln gives a harness on its machine (`kinds::set_env`, the session's
    /// mark, `kiln reach mcp`'s and agy's) has its fate in a microVM decided: one added there
    /// without one fails here.
    #[test]
    fn step18_every_kiln_variable_a_harness_gets_is_decided_for_a_microvm() {
        let kinds = include_str!("../kinds.rs");
        let reach = include_str!("../../reach/mcp.rs");
        // Each file's own code, its tests' module left out (their variables are a test's).
        let live = |src: &'static str| src.split("#[cfg(test)]\nmod tests {").next().unwrap_or_default();
        let mut given: Vec<String> = Vec::new();
        for src in [live(kinds), live(reach)] {
            for chunk in src.split("\"KILN_").skip(1) {
                let name: String = chunk.chars().take_while(|c| c.is_ascii_uppercase() || *c == '_').collect();
                // `"KILN_"` alone is the prefix `kinds::kilns` strips, no variable.
                if !name.is_empty() && chunk.get(name.len()..).is_some_and(|rest| rest.starts_with('"')) {
                    given.push(format!("KILN_{name}"));
                }
            }
        }
        // Both sources were read: one variable each is known to set.
        assert!(given.iter().any(|g| g == "KILN_HARNESS_SOCK") && given.iter().any(|g| g == "KILN_SESSION"), "{given:?}");
        given.push(crate::harness::survivors::MARK.to_owned());
        given.sort();
        given.dedup();
        let undecided: Vec<&String> = given.iter().filter(|g| !HOST.iter().any(|(k, _)| k == g)).collect();
        assert!(undecided.is_empty(), "no fate in a microVM for {undecided:?}");
        // The prefix `kinds::kilns` strips is no variable of its own.
        assert!(!HOST.iter().any(|(k, _)| *k == "KILN_"));
    }

    /// A Claude Code session's harness gets its mark and kiln's fixed variables; none of its
    /// machine's sockets, and nothing of this process's environment.
    #[test]
    fn step18_a_harness_in_a_microvm_gets_its_mark_and_no_machine_path() -> anyhow::Result<()> {
        let claude = Kind::get("claude").ok_or_else(|| anyhow::anyhow!("no claude kind"))?;
        let env = member_env("s1", claude);
        anyhow::ensure!(env.contains(&("KILN_AGENT".into(), "s1".into())), "{env:?}");
        anyhow::ensure!(env.iter().any(|(k, _)| k == "CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING"), "{env:?}");
        for (k, f) in HOST {
            if let Fate::LeftOut(_) = f {
                anyhow::ensure!(!env.iter().any(|(n, _)| n == k), "{k}");
            }
        }
        let machine: Vec<String> = std::env::vars().map(|(k, _)| k).filter(|k| k != "KILN_AGENT" && !k.starts_with("CLAUDE_CODE_")).collect();
        anyhow::ensure!(!env.iter().any(|(k, _)| machine.contains(k)), "{env:?}");
        let codex = Kind::get("codex").ok_or_else(|| anyhow::anyhow!("no codex kind"))?;
        anyhow::ensure!(member_env("s2", codex) == vec![("KILN_AGENT".to_owned(), "s2".to_owned())]);
        Ok(())
    }
}
