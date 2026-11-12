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
    // Codex, through its app-server, from OpenAI's whole package (kiln plan
    // 01a1cd8e862e7169a074db3d5fa33f3d, step 20).
    Builtin {
        name: "harness-codex",
        harness: "codex",
        text: include_str!("../../../../.kiln/images/harness-codex/Agentfile"),
        pin: include_str!("../../../../.kiln/images/harness-codex/pin.json"),
        files: &[("harness.json", include_str!("../../../../.kiln/images/harness-codex/harness.json"))],
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
mod codex_tests;

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
}
