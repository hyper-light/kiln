//! Skipping by commit message (docs/ci.md W03-R06), GitHub-exact: `[skip ci]`, `[ci skip]`,
//! `[no ci]`, `[skip actions]` or `[actions skip]` anywhere in the head commit's message, or a
//! `skip-checks: true` trailer, skips `push` and `pull_request` runs.

const MARKERS: &[&str] = &["[skip ci]", "[ci skip]", "[no ci]", "[skip actions]", "[actions skip]"];

/// Whether `message` asks to skip the run.
pub fn asks_to_skip(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    if MARKERS.iter().any(|m| lower.contains(m)) {
        return true;
    }
    // The trailer: the message's last paragraph holds `skip-checks:true` or `skip-checks: true`.
    let last = lower.trim_end().rsplit("\n\n").next().unwrap_or_default();
    last.lines().any(|l| {
        let l = l.trim();
        l.strip_prefix("skip-checks:").is_some_and(|v| v.trim() == "true")
    })
}

/// Whether the skip markers apply to `event` (they don't to `workflow_dispatch`, schedules, …).
/// kiln's events honour them only when the workflow says `kiln: { honour-skip-ci: true }`.
pub fn applies(event: &str, honour_for_kiln_events: bool) -> bool {
    match event {
        "push" | "pull_request" => true,
        "diff" | "land" | "review" | "comment" | "pipeline" | "agent" | "deploy" | "webhook" => honour_for_kiln_events,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W03-AC03: all six forms, case-insensitive; not for `workflow_dispatch`.
    #[test]
    fn w03_ac03_markers() {
        for m in ["fix things [skip ci]", "[CI SKIP] fix", "x [No Ci]", "[skip actions] y", "z [actions skip]", "Fix\n\nMore words.\n\nskip-checks: true", "Fix\n\nskip-checks:true\n"] {
            assert!(asks_to_skip(m), "{m:?}");
        }
        for m in ["fix ci skip", "[skip-ci]", "skip-checks: true is mentioned mid-message\n\nand then more", "Fix\n\nskip-checks: false"] {
            assert!(!asks_to_skip(m), "{m:?}");
        }
        assert!(applies("push", false) && applies("pull_request", false));
        assert!(!applies("workflow_dispatch", true) && !applies("schedule", true));
        assert!(!applies("diff", false) && applies("diff", true));
    }
}
