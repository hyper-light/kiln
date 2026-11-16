//! A harness swapped in on its machine's host (kiln plan 01a1dd2e…, P6c.3, acceptance 3; `super::overrides`): installed
//! into the session's own folder from its pinned http(s) source, held to its checksum as it streams, and run from there
//! in place of its machine's, its conversation going on through its own resume. Its folder is named by its checksum, so
//! a swap back to one installed before fetches nothing, and a start that finds it gone (its session moved to another
//! machine) installs it again.
//!
//! A source that serves its harness's executable is installed as that program. One that serves a gzip tar (its URL's
//! path ending `.tar.gz` or `.tgz`) is unpacked whole by kiln's one unpacker (`crate::agentfiles::unpack`), and its
//! harness's program is found at its root or in its `bin/`, or the same beneath its one top directory.

use std::path::{Path, PathBuf};

use super::overrides::Component;

/// Where a session's folder keeps its components.
const COMPONENTS: &str = "components";

/// The checksum's hex digits `c` is installed by.
fn hex_of(c: &Component) -> Result<&str, String> {
    c.checksum.as_deref().and_then(|s| s.strip_prefix("sha256:")).ok_or_else(|| format!("{} carries no checksum to install it by", c.source))
}

/// Where `c` is installed in session folder `dir`: `<dir>/components/<its checksum's hex>`.
pub fn dir_of(dir: &Path, c: &Component) -> Result<PathBuf, String> {
    Ok(dir.join(COMPONENTS).join(hex_of(c)?))
}

/// Whether `source`'s URL path names a gzip tar.
fn archive(source: &str) -> bool {
    let path = source.split(['?', '#']).next().unwrap_or(source);
    path.ends_with(".tar.gz") || path.ends_with(".tgz")
}

/// Its executable's file name for harness program `program` on this machine.
fn file_of(program: &str) -> String {
    if cfg!(windows) { format!("{program}.exe") } else { program.to_owned() }
}

/// `program`'s executable in installed tree `at`: at its root or in its `bin/`, or the same beneath its one top
/// directory.
fn found(at: &Path, program: &str) -> Option<PathBuf> {
    let file = file_of(program);
    let within = |root: &Path| [root.join(&file), root.join("bin").join(&file)].into_iter().find(|p| p.is_file());
    within(at).or_else(|| {
        let mut dirs = std::fs::read_dir(at).ok()?.filter_map(Result::ok).filter(|e| e.path().is_dir());
        match (dirs.next(), dirs.next()) {
            (Some(one), None) => within(&one.path()),
            _ => None,
        }
    })
}

/// Harness component `c` installed in session folder `dir`, made where it isn't: the executable of `program` (its
/// harness's own name for it) that its harness runs.
pub async fn installed(dir: &Path, c: &Component, program: &str) -> Result<PathBuf, String> {
    let at = dir_of(dir, c)?;
    if let Some(exe) = found(&at, program) {
        return Ok(exe);
    }
    let hex = hex_of(c)?.to_owned();
    let parent = dir.join(COMPONENTS);
    std::fs::create_dir_all(&parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    let client = crate::http::client::Client::public().map_err(|e| format!("kiln's HTTP client: {e:#}"))?;
    let arrived = crate::agentfiles::context::fetch_held(&client, &c.source, &hex, None, &parent).await.map_err(|e| e.to_string())?;
    let (source, file, archived) = (c.source.clone(), file_of(program), archive(&c.source));
    let into = parent.clone();
    // A tree of a harness's unpacks off the runtime's threads.
    let made = crate::rt::blocking(move || -> Result<(), String> {
        if archived {
            use crate::agentfiles::unpack::{Spec, Tree, tar_gz, totals};
            let (unpacked_bytes, entries) = totals(&arrived.path, &source).map_err(|e| e.to_string())?;
            return tar_gz(&arrived.path, &source, &Spec::Tree(Tree { strip: 0, unpacked_bytes, entries, skip: Vec::new() }), &into, Path::new(&hex), None).map_err(|e| e.to_string());
        }
        // One file: its harness's executable, renamed into place in a folder made whole first.
        let staging = into.join(format!(".{hex}.{}", crate::util::new_id()));
        let placed = std::fs::create_dir(&staging)
            .and_then(|()| std::fs::rename(&arrived.path, staging.join(&file)))
            .and_then(|()| crate::os::set_mode(&staging.join(&file), 0o755))
            .and_then(|()| std::fs::rename(&staging, into.join(&hex)));
        if placed.is_err() {
            let _ = std::fs::remove_file(&arrived.path);
            let _ = std::fs::remove_dir_all(&staging);
        }
        placed.map_err(|e| format!("{}: {e}", into.join(&hex).display()))
    })
    .await;
    match made {
        Ok(Ok(())) => {}
        Ok(Err(why)) => return Err(why),
        Err(e) => return Err(format!("{e:#}")),
    }
    found(&at, program).ok_or_else(|| format!("{} holds no {} at its root or in its bin/", c.source, file_of(program)))
}

/// The components installed in session folder `dir` but `keep` (the one it holds, if any) let go of: a swap's old
/// version, once its harness runs another.
pub fn prune(dir: &Path, keep: Option<&Component>) {
    let keep = keep.and_then(|c| dir_of(dir, c).ok());
    let Ok(entries) = std::fs::read_dir(dir.join(COMPONENTS)) else { return };
    for e in entries.filter_map(Result::ok) {
        let path = e.path();
        if Some(&path) != keep.as_ref() && path.is_dir() {
            crate::agentfiles::unpack::discard(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn harness(source: &str) -> Component {
        Component { kind: super::super::overrides::ComponentKind::Harness, path: "/harness/claude".into(), source: source.into(), checksum: Some(format!("sha256:{}", "ab".repeat(32))), version_id: None }
    }

    /// P6c.3: an archive is told by its URL's path, its query and fragment aside.
    #[test]
    fn p6c_3_an_archive_is_told_by_its_urls_path() {
        assert!(archive("https://example.com/claude-2.1.tar.gz"));
        assert!(archive("https://example.com/claude.tgz?sig=1"));
        assert!(!archive("https://example.com/claude"));
        assert!(!archive("https://example.com/claude?f=x.tar.gz"));
    }

    /// P6c.3: its harness's program is found at an installed tree's root, in its bin/, or beneath its one top
    /// directory; not where two top directories leave it unclear.
    #[test]
    fn p6c_3_its_harness_is_found_where_a_tree_keeps_it() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let exe = file_of("claude");
        let root = dir.path().join("root");
        std::fs::create_dir_all(root.join("bin"))?;
        std::fs::write(root.join("bin").join(&exe), b"#!/bin/sh\n")?;
        assert_eq!(found(&root, "claude"), Some(root.join("bin").join(&exe)));
        let nested = dir.path().join("nested");
        std::fs::create_dir_all(nested.join("claude-2.1"))?;
        std::fs::write(nested.join("claude-2.1").join(&exe), b"#!/bin/sh\n")?;
        assert_eq!(found(&nested, "claude"), Some(nested.join("claude-2.1").join(&exe)));
        std::fs::create_dir_all(nested.join("other"))?;
        assert_eq!(found(&nested, "claude"), None, "two top directories: which one is unclear");
        Ok(())
    }

    /// P6c.3: an installed component is kept by its checksum, and the ones a session no longer holds are let go of.
    #[test]
    fn p6c_3_components_are_kept_by_checksum_and_the_unheld_let_go_of() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let held = harness("https://example.com/claude");
        let at = dir_of(dir.path(), &held).map_err(anyhow::Error::msg)?;
        assert_eq!(at, dir.path().join("components").join("ab".repeat(32)));
        let old = dir.path().join("components").join("cd".repeat(32));
        std::fs::create_dir_all(&at)?;
        std::fs::create_dir_all(&old)?;
        prune(dir.path(), Some(&held));
        assert!(at.is_dir() && !old.exists(), "the one it holds stays, the other goes");
        prune(dir.path(), None);
        assert!(!at.exists(), "restored: none stays");
        Ok(())
    }
}
