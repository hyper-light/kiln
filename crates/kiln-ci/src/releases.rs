//! Releases by tag (docs/ci.md W29-R21). A release is a full version, `vMAJOR.MINOR.PATCH` (a
//! pre-release after `-`), and its floating tags `vMAJOR` and `vMAJOR.MINOR` move to the newest
//! stable release, as GitHub's guidance for actions has it. In a repository kiln hosts, a full
//! version is never moved or deleted and a released name is never used again, as GitHub's
//! immutable releases; a floating version moves only by Release. The server says which names were
//! released and who's pushing.

use crate::pipelines::full_version;

/// A tag's kind, for what a push to it may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagKind {
    /// `v2.4.1`, `v2.4.1-rc.1`.
    Full,
    /// `v2`, `v2.4`.
    Floating,
    /// Anything else (`nightly`, `release-1`).
    Other,
}

/// `v2` or `v2.4`, with its `v` or without.
pub fn floating(tag: &str) -> bool {
    let t = tag.strip_prefix('v').unwrap_or(tag);
    let parts: Vec<&str> = t.split('.').collect();
    (1..=2).contains(&parts.len()) && parts.iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

pub fn kind_of(tag: &str) -> TagKind {
    if full_version(tag).is_some() {
        TagKind::Full
    } else if floating(tag) {
        TagKind::Floating
    } else {
        TagKind::Other
    }
}

/// The floating tags a release moves to its commit (`v2` and `v2.4` for `v2.4.1`), with its `v`
/// or without as it has it; none for a pre-release, or for what isn't a full version.
pub fn floating_of(tag: &str) -> Vec<String> {
    let Some((major, minor, _, None)) = full_version(tag) else { return Vec::new() };
    let v = if tag.starts_with('v') { "v" } else { "" };
    vec![format!("{v}{major}"), format!("{v}{major}.{minor}")]
}

/// The floating tags a release moves to its commit (W29-R21): each of [`floating_of`] that it's
/// the newest stable release under, among `others` (the repository's other tags). An older
/// patch released after a newer minor moves its `vMAJOR.MINOR`, not its `vMAJOR`.
pub fn floating_moved(tag: &str, others: &[&str]) -> Vec<String> {
    let Some((major, minor, _, None)) = full_version(tag) else { return Vec::new() };
    let newest = |under_minor: bool| {
        let mut all: Vec<&str> = others.iter().copied().filter(|t| matches!(full_version(t), Some((a, b, _, None)) if a == major && (!under_minor || b == minor))).collect();
        all.push(tag);
        crate::pipelines::newest_release(all) == Some(tag)
    };
    let v = if tag.starts_with('v') { "v" } else { "" };
    let mut out = Vec::new();
    if newest(false) {
        out.push(format!("{v}{major}"));
    }
    if newest(true) {
        out.push(format!("{v}{major}.{minor}"));
    }
    out
}

/// Whether a push may do this to `tag` (without `refs/tags/`) in a repository kiln hosts: create
/// it, delete it, or else move it. `released` says the repository has had a release of that name;
/// `by_release`, that kiln's Release is pushing. The refusal says why, in W29-R21's words.
pub fn tag_push(tag: &str, creates: bool, deletes: bool, released: bool, by_release: bool) -> Result<(), String> {
    match kind_of(tag) {
        TagKind::Full if creates && released => Err(format!("`{tag}` was released before: a release's name isn't used again")),
        TagKind::Full if creates => Ok(()),
        TagKind::Full => Err(format!("`{tag}` is a release: it isn't {}", if deletes { "deleted" } else { "moved" })),
        TagKind::Floating if by_release && !deletes => Ok(()),
        TagKind::Floating => Err(format!("`{tag}` is a floating version: only Release moves it, to the newest release (ci.releases.tag)")),
        TagKind::Other => Ok(()),
    }
}

/// The versions Release offers after `newest` (W29-R21), the one what it changes calls for first:
/// a major when it breaks what a consumer passes (an input removed, now required or new and
/// required; a new secret; more permissions or egress), a minor when it adds (an input, an
/// output), else a patch; then the other two, lowest first. With no release yet, `v0.1.0` then
/// `v1.0.0`.
pub fn next_versions(newest: Option<&str>, changes: &[crate::wf::diff::Change]) -> Vec<String> {
    let Some((major, minor, patch, _)) = newest.and_then(full_version) else { return vec!["v0.1.0".to_owned(), "v1.0.0".to_owned()] };
    let v = if newest.is_some_and(|t| t.starts_with('v')) { "v" } else { "" };
    let breaks = changes.iter().any(|c| match (c.kind, c.what) {
        ("input", "removed" | "now required") => true,
        ("input", "added") => c.detail.as_deref() == Some("required"),
        ("secret", "added") | ("permission", "added" | "changed") | ("egress", "added") => true,
        _ => false,
    });
    let adds = changes.iter().any(|c| c.what == "added");
    let (pat, min, maj) = (format!("{v}{major}.{minor}.{}", patch + 1), format!("{v}{major}.{}.0", minor + 1), format!("{v}{}.0.0", major + 1));
    match (breaks, adds) {
        (true, _) => vec![maj, pat, min],
        (false, true) => vec![min, pat, maj],
        (false, false) => vec![pat, min, maj],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w29_r21_versions_and_floating_tags() {
        for (tag, kind) in [("v2.4.1", TagKind::Full), ("2.4.1", TagKind::Full), ("v2.4.1-rc.1", TagKind::Full), ("v2", TagKind::Floating), ("v2.4", TagKind::Floating), ("2", TagKind::Floating)] {
            assert_eq!(kind_of(tag), kind, "{tag}");
        }
        for other in ["nightly", "v2.x", "release-1", "v", "", "v2.4.1.0", "latest"] {
            assert_eq!(kind_of(other), TagKind::Other, "{other:?}");
        }
        assert_eq!(floating_of("v2.4.1"), ["v2", "v2.4"]);
        assert_eq!(floating_of("2.4.1"), ["2", "2.4"]);
        assert!(floating_of("v2.0.0-rc.1").is_empty(), "a pre-release moves nothing");
        assert!(floating_of("v2").is_empty() && floating_of("nightly").is_empty());
        // Only where it's the newest stable release.
        assert_eq!(floating_moved("v1.2.0", &["v1.1.0", "v1", "v1.1"]), ["v1", "v1.2"]);
        assert_eq!(floating_moved("v1.1.1", &["v1.1.0", "v1.2.0", "v1", "v1.1", "v1.2"]), ["v1.1"], "an older patch moves its minor only");
        assert_eq!(floating_moved("v1.0.1", &["v1.1.0", "v1.0.0-rc.1"]), ["v1.0"]);
        assert_eq!(floating_moved("v2.0.0", &["v1.9.0", "v3.0.0-rc.1"]), ["v2", "v2.0"], "a pre-release doesn't count");
        assert!(floating_moved("v2.0.0-rc.1", &[]).is_empty());
    }

    #[test]
    fn w29_r21_tag_pushes() {
        // A full version: created once, then never moved or deleted, and its name never again.
        assert_eq!(tag_push("v1.0.0", true, false, false, false), Ok(()));
        assert_eq!(tag_push("v1.0.0", true, false, false, true), Ok(()), "Release makes one too");
        let moved = tag_push("v1.0.0", false, false, true, false).err().unwrap_or_default();
        assert!(moved.contains("`v1.0.0` is a release: it isn't moved"), "{moved}");
        assert!(tag_push("v1.0.0", false, false, true, true).is_err(), "not even by Release");
        let deleted = tag_push("v1.0.0", false, true, true, false).err().unwrap_or_default();
        assert!(deleted.contains("isn't deleted"), "{deleted}");
        assert!(tag_push("v1.0.0", true, false, true, false).is_err_and(|e| e.contains("isn't used again")));
        // A floating version: only Release moves it, and nothing deletes it.
        assert!(tag_push("v1", true, false, false, false).is_err_and(|e| e.contains("only Release moves it")));
        assert!(tag_push("v1.2", false, false, false, false).is_err());
        assert_eq!(tag_push("v1", false, false, false, true), Ok(()));
        assert_eq!(tag_push("v1", true, false, false, true), Ok(()));
        assert!(tag_push("v1", false, true, false, true).is_err());
        // Other tags, as before.
        for (creates, deletes) in [(true, false), (false, false), (false, true)] {
            assert_eq!(tag_push("nightly", creates, deletes, false, false), Ok(()));
        }
    }

    #[test]
    fn w29_r21_the_next_version_is_what_it_changes_calls_for() {
        use crate::wf::diff::Change;
        let c = |kind: &'static str, what: &'static str, detail: Option<&str>| Change { kind, name: "x".into(), what, detail: detail.map(str::to_owned) };
        assert_eq!(next_versions(Some("v2.4.0"), &[]), ["v2.4.1", "v2.5.0", "v3.0.0"]);
        assert_eq!(next_versions(Some("v2.4.0"), &[c("input", "added", None)]), ["v2.5.0", "v2.4.1", "v3.0.0"]);
        assert_eq!(next_versions(Some("v2.4.0"), &[c("input", "added", Some("required"))]), ["v3.0.0", "v2.4.1", "v2.5.0"]);
        assert_eq!(next_versions(Some("2.4.0"), &[c("input", "removed", None)]), ["3.0.0", "2.4.1", "2.5.0"]);
        assert_eq!(next_versions(Some("v1.0.0"), &[c("permission", "changed", Some("read → write"))]), ["v2.0.0", "v1.0.1", "v1.1.0"]);
        assert_eq!(next_versions(None, &[]), ["v0.1.0", "v1.0.0"]);
    }
}
