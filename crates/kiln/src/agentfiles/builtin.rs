//! The Agentfiles kiln ships, one per harness it runs in a Shards microVM (kiln plan
//! 01a1cd8e862e7169a074db3d5fa33f3d, "Every kiln harness in Shards microVMs"): each embedded in kiln's
//! one binary, listed read-only beside a person's own, and copied to make one theirs. Each harness's
//! own step adds its Agentfile here (`include_str!` of `.kiln/images/harness-<id>/`, where kiln keeps
//! image definitions, W13-R11).

use serde::Serialize;

/// An Agentfile kiln ships.
pub struct Builtin {
    /// Its name, an image's (as a person's Agentfile's is: [`super::name_problem`]).
    pub name: &'static str,
    /// The harness it runs (`crate::harness::kinds::Kind`'s id: `claude`, `codex` ...).
    pub harness: &'static str,
    pub text: &'static str,
    /// Its pin.json: each platform's files of the harness directory, by URL, SHA-256 and size, and
    /// how its pin.sh checked them.
    pub pin: &'static str,
    /// The small files of its build context beside the Agentfile (its harness.json), by name.
    pub files: &'static [(&'static str, &'static str)],
}

/// Every one, in the order they're listed.
pub const ALL: &[Builtin] = &[
    // Goose, its own static musl build, as kiln's Goose kind starts it (kiln plan
    // 01a1cd8e862e7169a074db3d5fa33f3d, step 23).
    Builtin {
        name: "harness-goose",
        harness: "goose",
        text: include_str!("../../../../.kiln/images/harness-goose/Agentfile"),
        pin: include_str!("../../../../.kiln/images/harness-goose/pin.json"),
        files: &[("harness.json", include_str!("../../../../.kiln/images/harness-goose/harness.json"))],
    },
    // Claude Code, Anthropic's own Linux build for each platform and libc, over stream-json as kiln's
    // Claude kind drives it (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 19). Its musl harness
    // file (CLAUDE_LIBC=musl) also takes the system's ripgrep.
    Builtin {
        name: "harness-claude",
        harness: "claude",
        text: include_str!("../../../../.kiln/images/harness-claude/Agentfile"),
        pin: include_str!("../../../../.kiln/images/harness-claude/pin.json"),
        files: &[
            ("harness.json", include_str!("../../../../.kiln/images/harness-claude/harness.json")),
            ("harness.musl.json", include_str!("../../../../.kiln/images/harness-claude/harness.musl.json")),
        ],
    },
    // Amp, through its ACP adapter amp-acp (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 24).
    Builtin {
        name: "harness-amp",
        harness: "amp",
        text: include_str!("../../../../.kiln/images/harness-amp/Agentfile"),
        pin: include_str!("../../../../.kiln/images/harness-amp/pin.json"),
        files: &[("harness.json", include_str!("../../../../.kiln/images/harness-amp/harness.json"))],
    },
    // Antigravity's CLI, agy, Google's static musl build, in its headless stream-json mode as kiln's
    // Antigravity kind starts it (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 26).
    Builtin {
        name: "harness-antigravity",
        harness: "antigravity",
        text: include_str!("../../../../.kiln/images/harness-antigravity/Agentfile"),
        pin: include_str!("../../../../.kiln/images/harness-antigravity/pin.json"),
        files: &[("harness.json", include_str!("../../../../.kiln/images/harness-antigravity/harness.json"))],
    },
    // Devin for Terminal, Cognition's own static builds, as kiln's Devin kind starts it (`devin acp`;
    // kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 25).
    Builtin {
        name: "harness-devin",
        harness: "devin",
        text: include_str!("../../../../.kiln/images/harness-devin/Agentfile"),
        pin: include_str!("../../../../.kiln/images/harness-devin/pin.json"),
        files: &[("harness.json", include_str!("../../../../.kiln/images/harness-devin/harness.json"))],
    },
    // Cursor's CLI agent, its own Linux package unpacked as its installer does, run as kiln's Cursor
    // kind starts it, `acp` (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 27).
    Builtin {
        name: "harness-cursor",
        harness: "cursor",
        text: include_str!("../../../../.kiln/images/harness-cursor/Agentfile"),
        pin: include_str!("../../../../.kiln/images/harness-cursor/pin.json"),
        files: &[("harness.json", include_str!("../../../../.kiln/images/harness-cursor/harness.json"))],
    },
];

/// One as the list shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AgentfileBuiltin"))]
pub struct View {
    pub name: String,
    pub harness: String,
    pub text: String,
}

/// Every one, as the list shows them.
pub fn views() -> Vec<View> {
    ALL.iter().map(|b| View { name: b.name.to_owned(), harness: b.harness.to_owned(), text: b.text.to_owned() }).collect()
}

#[cfg(test)]
mod goose_tests;

#[cfg(test)]
mod claude_tests;

#[cfg(test)]
mod amp_tests;

#[cfg(test)]
mod antigravity_tests;

#[cfg(test)]
mod devin_tests;

#[cfg(test)]
mod cursor_tests;

#[cfg(test)]
mod tests {
    use std::path::Path;

    /// Each is what a person's Agentfile must be (a name an image may have, a text Shards may read), and
    /// no two share a name.
    #[test]
    fn every_builtin_is_a_valid_agentfile_once() {
        let mut names = std::collections::BTreeSet::new();
        for b in super::ALL {
            assert_eq!(super::super::name_problem(b.name), None, "{}", b.name);
            assert_eq!(super::super::text_problem(b.text), None, "{}", b.name);
            assert!(crate::harness::kinds::Kind::get(b.harness).is_some(), "{}: no harness {}", b.name, b.harness);
            assert!(names.insert(b.name), "{} twice", b.name);
        }
    }

    /// Of `paths` (from the repository's root), those no COPY of the Dockerfile's `build` stage brings
    /// in from the build context.
    fn uncopied<'a>(dockerfile: &str, paths: &[&'a str]) -> Vec<&'a str> {
        let mut sources = Vec::new();
        let stage = dockerfile.lines().map(str::trim).skip_while(|l| !(l.starts_with("FROM ") && l.ends_with(" AS build"))).skip(1).take_while(|l| !l.starts_with("FROM "));
        for rest in stage.filter_map(|l| l.strip_prefix("COPY ")) {
            if rest.contains("--from=") {
                continue;
            }
            let words: Vec<&str> = rest.split_whitespace().filter(|w| !w.starts_with("--")).collect();
            if let Some((_, from)) = words.split_last() {
                sources.extend(from.iter().map(|s| s.to_string()));
            }
        }
        paths.iter().copied().filter(|p| !sources.iter().any(|s| if s.ends_with('/') { p.starts_with(s.as_str()) } else { p == s })).collect()
    }

    /// kiln's own image builds with every file a built-in embeds: the Dockerfile's `build` stage copies
    /// each one, as `.kiln/images/` sits outside `crates/` (step 25's finding: without it kiln's docker
    /// build fails). The Dockerfile without that COPY is caught.
    #[test]
    fn step23_kilns_dockerfile_copies_what_builtins_embed() -> anyhow::Result<()> {
        let dockerfile = include_str!("../../../../Dockerfile");
        let table = include_str!("builtin.rs").split("#[cfg(test)]").next().unwrap_or_default();
        let embedded: Vec<&str> = table.split("include_str!(\"../../../../").skip(1).filter_map(|s| s.split('"').next()).collect();
        anyhow::ensure!(!embedded.is_empty(), "no built-in embeds a file from outside the crate");
        let missing = uncopied(dockerfile, &embedded);
        anyhow::ensure!(missing.is_empty(), "the Dockerfile's build stage copies none of {missing:?}");
        let without: String = dockerfile.lines().filter(|l| l.trim() != "COPY .kiln/images/ .kiln/images/").map(|l| format!("{l}\n")).collect();
        anyhow::ensure!(uncopied(&without, &embedded) == embedded);
        Ok(())
    }

    /// The real Shards, where a test names it (`KILN_TEST_SHARDS`, a `shards` binary, as
    /// `check`'s tests take it): each built-in passes its static check with no network asked for.
    #[test]
    fn step23_a_real_shards_checks_every_builtin() -> anyhow::Result<()> {
        let Some(bin) = std::env::var_os("KILN_TEST_SHARDS") else {
            eprintln!("skipped: KILN_TEST_SHARDS names no shards binary");
            return Ok(());
        };
        crate::rt::block_on(async move {
            let root = tempfile::tempdir()?;
            for b in super::ALL {
                let c = super::super::check::run(Path::new(&bin), root.path(), b.text).await;
                anyhow::ensure!(c.checked && c.diagnostics.is_empty(), "{}: {c:?}", b.name);
            }
            Ok(())
        })
    }

    /// What breaks a harness's HOME in `text` (an Agentfile) and `harness_json`. A harness sees its image
    /// read-only, so a HOME its harness file sets must be one of its volumes; and no volume may be at or
    /// under /tmp, each harness's own scratch (step 19's research 01a1cf48e8157494b048e1fb2c437f08, f22
    /// and f23).
    fn home_problems(text: &str, harness_json: &str, harness: &str) -> anyhow::Result<Vec<String>> {
        let tail = format!(" FOR {harness}");
        let volumes: Vec<&str> = text.lines().map(str::trim).filter_map(|l| l.strip_prefix("VOLUME --target-kind=harness ")?.strip_suffix(tail.as_str())).collect();
        let mut found: Vec<String> = volumes.iter().filter(|v| **v == "/tmp" || v.starts_with("/tmp/")).map(|v| format!("a volume at {v}")).collect();
        let h: serde_json::Value = serde_json::from_str(harness_json)?;
        let env = h.pointer("/run/env").and_then(serde_json::Value::as_array).cloned().unwrap_or_default();
        if let Some(home) = env.iter().filter_map(serde_json::Value::as_str).find_map(|e| e.strip_prefix("HOME="))
            && !volumes.contains(&home)
        {
            found.push(format!("HOME={home} is none of its volumes {volumes:?}"));
        }
        Ok(found)
    }

    /// Every built-in keeps its harness's HOME on one of its volumes, if it sets one, and no volume under
    /// /tmp; a HOME off its volumes and a volume under /tmp are each caught.
    #[test]
    fn step19_a_harness_home_is_its_volume_and_none_is_under_tmp() -> anyhow::Result<()> {
        for b in super::ALL {
            for (name, text) in b.files.iter().filter(|(n, _)| n.starts_with("harness.") && n.ends_with(".json")) {
                let found = home_problems(b.text, text, b.harness)?;
                anyhow::ensure!(found.is_empty(), "{} {name}: {found:?}", b.name);
            }
        }
        let text = "VOLUME --target-kind=harness /workspace FOR h\nVOLUME --target-kind=harness /home/h FOR h\n";
        let json = r#"{"run":{"command":["h"],"env":["HOME=/home/h"]}}"#;
        anyhow::ensure!(home_problems(text, json, "h")?.is_empty());
        anyhow::ensure!(!home_problems(&text.replace("/home/h FOR", "/tmp/h FOR"), json, "h")?.is_empty(), "a volume under /tmp passed");
        anyhow::ensure!(!home_problems(text, &json.replace("/home/h", "/root"), "h")?.is_empty(), "a HOME off its volumes passed");
        Ok(())
    }
}
