//! Drafts on a host (docs/ci.md W29-R11, R12): the branch a host keeps a draft on, the title a
//! draft has where a host has no draft flag, and GitHub's mutation that marks one ready. The server
//! passes in what it read.

/// Where a host keeps its drafts: branch `kiln/draft/<stem>-<id8>` (W29-R10).
pub const DRAFT_PREFIX: &str = "kiln/draft/";

/// Whether `git_ref` (`refs/heads/kiln/draft/…`, or the branch's own name) is a draft's: kiln's
/// engine starts no run for an event on one (W29-R10).
pub fn is_draft_ref(git_ref: &str) -> bool {
    let branch = git_ref.strip_prefix("refs/heads/").unwrap_or(git_ref);
    branch.strip_prefix(DRAFT_PREFIX).is_some_and(|rest| !rest.is_empty())
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
