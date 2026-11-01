//! Update drafts for consumers (docs/ci.md W29-R22): a consumer's setting, which draft an update
//! goes in, and its `uses:` moved to a new release. The server says what's released, what each
//! consumer's files say, and the time.

use crate::pipelines::{full_version, newest_release};

/// `updates.group`: a draft per update (`none`), or the consumer's updates of a day or a week in
/// one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Group {
    #[default]
    None,
    Daily,
    Weekly,
}

/// A consumer's `updates:` (W29-R22).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Updates {
    /// `actions: on`: off unless set.
    pub on: bool,
    pub cooldown_s: i64,
    pub group: Group,
}

/// Dependabot's cooldown when none is set: three days.
pub const COOLDOWN_S: i64 = 3 * 86_400;

/// Who kiln's update drafts are by, before their bucket ([`bucket`]): `kiln-updates:acme/kit`.
pub const AUTHOR: &str = "kiln-updates:";

/// A duration as `cooldown` writes it: `3d`, `12h`, `30m`, `45s`, or seconds alone (`0`).
pub fn duration_s(s: &str) -> Option<i64> {
    let s = s.trim();
    let (n, unit) = match s.find(|c: char| !c.is_ascii_digit()) {
        Some(i) => s.split_at(i),
        None => (s, "s"),
    };
    let n: i64 = n.parse().ok()?;
    let per = match unit.trim() {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        _ => return None,
    };
    n.checked_mul(per)
}

/// `updates:` as `.kiln/config.yaml` writes it: `actions` (`on` or `off`, or true or false),
/// `cooldown` (Dependabot's 3 days unless set), `group` (`none`, `daily`, `weekly`). What it can't
/// read is as unset.
pub fn of_config(actions: Option<&str>, cooldown: Option<&str>, group: Option<&str>) -> Updates {
    let on = actions.is_some_and(|a| matches!(a.trim().to_ascii_lowercase().as_str(), "on" | "true" | "yes"));
    let cooldown_s = cooldown.and_then(duration_s).unwrap_or(COOLDOWN_S);
    let group = match group.map(|g| g.trim().to_ascii_lowercase()).as_deref() {
        Some("daily") => Group::Daily,
        Some("weekly") => Group::Weekly,
        _ => Group::None,
    };
    Updates { on, cooldown_s, group }
}

/// Which of a consumer's drafts an update goes in: its publisher's own (`group: none`, where a
/// newer release supersedes the one before), or the day's or the week's (from Monday, UTC), with
/// every update due in it.
pub fn bucket(group: Group, publisher: &str, due_at: i64) -> String {
    match group {
        Group::None => publisher.to_ascii_lowercase(),
        Group::Daily => format!("day-{}", due_at.div_euclid(86_400)),
        Group::Weekly => format!("week-{}", (due_at + 3 * 86_400).div_euclid(7 * 86_400)),
    }
}

/// Whether a `.github/dependabot.yml` updates GitHub Actions: then kiln updates only what it
/// hosts, so no update comes twice.
pub fn dependabot_covers_actions(text: &str) -> bool {
    let Ok(doc) = serde_yaml::from_str::<serde_yaml::Value>(text) else { return false };
    doc.get("updates").and_then(serde_yaml::Value::as_sequence).is_some_and(|u| u.iter().any(|e| e.get("package-ecosystem").and_then(serde_yaml::Value::as_str) == Some("github-actions")))
}

/// `text` with each `uses:` of `publisher` (`owner/repo[/path]@ref`, or `kiln://` before it)
/// pinned to a full version older than `to` moved to `to`, and the refs it moved. A floating
/// ref, a branch, a commit, a newer release and other repositories stay as they are.
pub fn moved_uses(text: &str, publisher: &str, to: &str) -> (String, Vec<String>) {
    let mut moved = Vec::new();
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        out.push_str(&moved_line(line, publisher, to, &mut moved));
    }
    (out, moved)
}

fn moved_line(line: &str, publisher: &str, to: &str, moved: &mut Vec<String>) -> String {
    let Some(at) = line.find("uses:") else { return line.to_owned() };
    let value = line.get(at + 5..).unwrap_or_default().trim().trim_matches(['"', '\'']);
    let name = value.strip_prefix("kiln://").unwrap_or(value);
    let p = publisher.to_ascii_lowercase();
    let lower = name.to_ascii_lowercase();
    if !(lower.starts_with(&format!("{p}/")) || lower.starts_with(&format!("{p}@"))) {
        return line.to_owned();
    }
    let Some((_, old)) = name.rsplit_once('@') else { return line.to_owned() };
    let old = old.split_whitespace().next().unwrap_or_default();
    if old == to || full_version(old).is_none() || newest_release([old, to]) != Some(to) {
        return line.to_owned();
    }
    moved.push(old.to_owned());
    line.replacen(&format!("@{old}"), &format!("@{to}"), 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w29_r22_update_settings_and_buckets() {
        assert_eq!(of_config(Some("on"), Some("0"), None), Updates { on: true, cooldown_s: 0, group: Group::None });
        assert_eq!(of_config(None, None, None), Updates { on: false, cooldown_s: COOLDOWN_S, group: Group::None }, "off, and Dependabot's cooldown, unless set");
        assert_eq!(of_config(Some("true"), Some("12h"), Some("Weekly")).group, Group::Weekly);
        for (s, n) in [("3d", Some(259_200)), ("12h", Some(43_200)), ("30m", Some(1_800)), ("45s", Some(45)), ("90", Some(90)), ("3w", None), ("", None)] {
            assert_eq!(duration_s(s), n, "{s:?}");
        }
        assert_eq!(bucket(Group::None, "Acme/Kit", 0), "acme/kit");
        // 1970-01-05 was a Monday: a week from it is one bucket.
        let monday = 4 * 86_400;
        assert_eq!(bucket(Group::Weekly, "acme/kit", monday), bucket(Group::Weekly, "acme/tools", monday + 6 * 86_400 + 86_399));
        assert_ne!(bucket(Group::Weekly, "acme/kit", monday), bucket(Group::Weekly, "acme/kit", monday - 1));
        assert_ne!(bucket(Group::Daily, "acme/kit", 0), bucket(Group::Daily, "acme/kit", 86_400));
        assert!(dependabot_covers_actions("version: 2\nupdates:\n  - package-ecosystem: github-actions\n    directory: /\n"));
        assert!(!dependabot_covers_actions("version: 2\nupdates:\n  - package-ecosystem: cargo\n    directory: /\n"));
    }

    #[test]
    fn w29_r22_uses_moved() {
        let text = "jobs:\n  a:\n    steps:\n      - uses: kiln://acme/kit/.github/actions/x@v1.0.0\n      - uses: \"acme/kit/.github/actions/y@v1.0.0\"\n      - uses: acme/kit/.github/actions/x@v1\n      - uses: acme/kit/.github/actions/x@main\n      - uses: acme/kitchen/x@v1.0.0\n      - uses: acme/kit/x@v2.0.0\n  b:\n    uses: acme/kit/.github/workflows/build.yml@v1.0.0\n";
        let (out, moved) = moved_uses(text, "acme/kit", "v1.1.0");
        assert_eq!(moved, ["v1.0.0", "v1.0.0", "v1.0.0"]);
        assert!(out.contains("kiln://acme/kit/.github/actions/x@v1.1.0\n") && out.contains("\"acme/kit/.github/actions/y@v1.1.0\"") && out.contains("build.yml@v1.1.0\n"), "{out}");
        for kept in ["actions/x@v1\n", "actions/x@main\n", "acme/kitchen/x@v1.0.0\n", "acme/kit/x@v2.0.0\n"] {
            assert!(out.contains(kept), "{kept}: {out}");
        }
        assert_eq!(moved_uses(text, "acme/tools", "v9.0.0").1, Vec::<String>::new());
    }
}
