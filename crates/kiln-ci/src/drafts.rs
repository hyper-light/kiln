//! Drafts of workflows and actions (docs/ci.md W29-R10 to R13): where a host keeps one, what its
//! commits say, and what no trigger and no other repository may do with it. The server passes in
//! what it read.

use crate::trigger::skip;

/// Where a host keeps its drafts: branch `kiln/draft/<stem>-<id8>` (W29-R10).
pub const DRAFT_PREFIX: &str = "kiln/draft/";

/// Whether `git_ref` (`refs/heads/kiln/draft/…`, or the branch's own name) is a draft's: kiln's
/// engine starts no run for an event on one (W29-R10).
pub fn is_draft_ref(git_ref: &str) -> bool {
    let branch = git_ref.strip_prefix("refs/heads/").unwrap_or(git_ref);
    branch.strip_prefix(DRAFT_PREFIX).is_some_and(|rest| !rest.is_empty())
}

/// A draft's stem, from the file it drafts: an action's folder (`setup` for
/// `.github/actions/setup/action.yml`), else the file's name to its first dot (`ci` for
/// `.kiln/ci.yaml`), in what a branch name takes.
pub fn draft_stem(path: &str) -> String {
    let mut parts = path.trim_end_matches('/').rsplit('/');
    let file = parts.next().unwrap_or_default();
    let stem = if matches!(file, "action.yml" | "action.yaml") { parts.next().unwrap_or_default() } else { file.split('.').next().unwrap_or_default() };
    let clean: String = stem.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c.to_ascii_lowercase() } else { '-' }).collect();
    let clean = clean.trim_matches('-');
    if clean.is_empty() { "pipeline".to_owned() } else { clean.to_owned() }
}

/// The branch a host's draft of `path` lives on, `id8` its own: `kiln/draft/<stem>-<id8>`.
pub fn draft_branch(path: &str, id8: &str) -> String {
    format!("{DRAFT_PREFIX}{}-{id8}", draft_stem(path))
}

/// A draft's head commit message (W29-R11): its title with `[skip ci]`, so GitHub skips its push
/// and pull-request workflows for it, and kiln, GitHub-exact, does too (W03-R06).
pub fn draft_message(title: &str) -> String {
    let t = plain(title);
    format!("{} [skip ci]\n", if t.is_empty() { "Draft" } else { &t })
}

/// What Ready commits the draft's tree again with (W29-R12): the same title, asking to skip
/// nothing, so the host's checks and kiln's run on it.
pub fn ready_message(title: &str) -> String {
    let t = plain(title);
    // A title that is itself a `skip-checks: true` trailer would still skip.
    if t.is_empty() || skip::asks_to_skip(&t) { format!("Ready: {t}\n").replace(": \n", "\n") } else { format!("{t}\n") }
}

/// A title's first line, without the markers that skip CI.
fn plain(title: &str) -> String {
    skip::without_markers(title.lines().next().unwrap_or_default())
}

/// A draft's title as GitLab takes it (W29-R11): its API has no draft flag, so the title starts
/// "Draft: "; one that says so already stays as it is. Bitbucket's, when it refuses its flag.
pub fn drafted_title(title: &str) -> String {
    if undrafted_title(title).is_some() { title.to_owned() } else { format!("Draft: {title}") }
}

/// A title without the prefix GitLab reads as a draft (`[Draft]`, `Draft:` or `(Draft)`, any
/// case): what Ready leaves of it (W29-R12). None when it has none.
pub fn undrafted_title(title: &str) -> Option<String> {
    let t = title.trim_start();
    ["[draft]", "draft:", "(draft)"].iter().find(|p| t.get(..p.len()).is_some_and(|head| head.eq_ignore_ascii_case(p))).map(|p| t.get(p.len()..).unwrap_or_default().trim_start().to_owned())
}

/// GitHub's mutation that marks a draft pull request ready for review, by its node id (W29-R12):
/// GitHub's REST API can't.
pub const GITHUB_READY: &str = "mutation($id: ID!) { markPullRequestReadyForReview(input: { pullRequestId: $id }) { pullRequest { isDraft } } }";

/// Why another repository may not use `uses` (W29-R10): it names a ref under `kiln/draft/`, and a
/// draft is its author's until it's published. A local action (`./…`) and an image aren't
/// another repository's.
pub fn uses_draft(uses: &str) -> Option<String> {
    let u = uses.trim();
    if u.starts_with("./") || u.starts_with("$/") || u.starts_with("docker://") {
        return None;
    }
    let (_, r) = u.rsplit_once('@')?;
    is_draft_ref(r).then(|| format!("`{u}` names a draft (a ref under {DRAFT_PREFIX}), which is its author's until it's published"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w29_r10_draft_refs_start_nothing() {
        for r in ["refs/heads/kiln/draft/ci-01a19f2c", "kiln/draft/setup-1", "refs/heads/kiln/draft/a/b"] {
            assert!(is_draft_ref(r), "{r}");
        }
        for r in ["refs/heads/main", "refs/heads/kiln/drafts", "refs/heads/kiln/draft/", "kiln/edit-ci-01a19f2c", "refs/tags/kiln/draft/x", "refs/heads/feature/kiln/draft/x"] {
            assert!(!is_draft_ref(r), "{r}");
        }
        assert_eq!(draft_branch(".kiln/ci.yaml", "01a19f2c"), "kiln/draft/ci-01a19f2c");
        assert_eq!(draft_branch(".github/actions/setup/action.yml", "01a19f2c"), "kiln/draft/setup-01a19f2c");
        assert_eq!(draft_branch(".github/workflows/Release Notes.yml", "1"), "kiln/draft/release-notes-1");
        assert_eq!(draft_stem(".github/workflows/.yml"), "pipeline");
        assert!(is_draft_ref(&draft_branch(".kiln/actions/fuzz/action.yaml", "x")));
    }

    #[test]
    fn w29_r10_a_draft_names_no_use() {
        assert!(uses_draft("acme/kit/.github/actions/x@kiln/draft/x-01a19f2c").is_some_and(|w| w.contains("draft")));
        assert!(uses_draft("acme/kit/.github/workflows/build.yml@refs/heads/kiln/draft/build-1").is_some());
        assert!(uses_draft("kiln://acme/kit/lint@kiln/draft/lint-1").is_some());
        for u in ["acme/kit@v1", "acme/kit/.github/actions/x@main", "./.github/actions/x", "docker://alpine:3.20", "acme/kit@kiln/drafts", "acme/kit"] {
            assert_eq!(uses_draft(u), None, "{u}");
        }
    }

    #[test]
    fn w29_r11_draft_heads_skip_ci() {
        for t in ["Edit .kiln/ci.yaml", "", "already [skip ci]", "multi\nline"] {
            let d = draft_message(t);
            assert!(skip::asks_to_skip(&d), "{d:?}");
            assert!(d.ends_with("[skip ci]\n"), "{d:?}");
            let r = ready_message(t);
            assert!(!skip::asks_to_skip(&r), "{r:?}");
            assert!(!r.is_empty() && r.ends_with('\n'), "{r:?}");
        }
        assert_eq!(draft_message("Edit ci"), "Edit ci [skip ci]\n");
        assert_eq!(ready_message("Edit ci [CI SKIP]"), "Edit ci\n");
        assert_eq!(ready_message(""), "Ready\n");
        assert!(!skip::asks_to_skip(&ready_message("skip-checks: true")));
    }

    #[test]
    fn w29_r11_gitlab_draft_titles() {
        assert_eq!(drafted_title("Edit ci"), "Draft: Edit ci");
        for t in ["Draft: Edit ci", "draft:Edit ci", "[Draft] Edit ci", "(DRAFT) Edit ci", "  Draft: Edit ci"] {
            assert_eq!(drafted_title(t), t, "already a draft's");
            assert_eq!(undrafted_title(t).as_deref(), Some("Edit ci"), "{t}");
        }
        assert_eq!(undrafted_title("Edit ci"), None);
        assert_eq!(undrafted_title("Drafting the ci"), None, "a word, not the prefix");
        assert_eq!(undrafted_title(&drafted_title("Édit ci")).as_deref(), Some("Édit ci"));
        assert!(GITHUB_READY.contains("markPullRequestReadyForReview") && GITHUB_READY.contains("$id: ID!"));
    }
}
