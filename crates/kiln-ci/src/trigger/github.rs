//! GitHub's webhook deliveries as events (docs/ci.md W03-R02, W03-R07, W03-R08). The payload
//! passes through as `github.event`. `GITHUB_REF` and `GITHUB_SHA` are what GitHub's "events
//! that trigger workflows" table gives for each event:
//! - most events run on the default branch's latest commit;
//! - `push` and `create` on the pushed or created ref;
//! - `pull_request` and its reviews on `refs/pull/<n>/merge`;
//! - `pull_request_target` on the base branch;
//! - `release` on its tag.
//!
//! A pull request whose head is another repository (a fork) is untrusted.

use serde_json::Value;

use super::event::{Event, Source, ZERO_SHA};

fn text<'a>(v: &'a Value, pointer: &str) -> &'a str {
    v.pointer(pointer).and_then(Value::as_str).unwrap_or_default()
}

/// A ref a delivery moved: `(full ref, new commit)`. Polling sees the same moves; whichever
/// source records a move first triggers it (W03-R14).
pub fn transition(name: &str, payload: &Value) -> Option<(String, String)> {
    match name {
        "push" => {
            let after = text(payload, "/after");
            (!after.is_empty() && after != ZERO_SHA).then(|| (text(payload, "/ref").to_owned(), after.to_owned()))
        }
        "pull_request" if matches!(text(payload, "/action"), "opened" | "synchronize" | "reopened") => {
            let n = payload.pointer("/number").and_then(Value::as_u64)?;
            Some((format!("refs/pull/{n}/head"), text(payload, "/pull_request/head/sha").to_owned()))
        }
        _ => None,
    }
}

/// The event a delivery stands for; `None` for deliveries no workflow runs on (`ping`,
/// installation events). `default_branch` and `default_head` are the repository's default
/// branch and its latest commit.
pub fn event(name: &str, payload: &Value, default_branch: &str, default_head: &str) -> Option<Event> {
    if matches!(name, "ping" | "installation" | "installation_repositories" | "github_app_authorization" | "meta" | "security_advisory" | "marketplace_purchase" | "sponsorship") {
        return None;
    }
    let mut e = Event {
        name: name.to_owned(),
        action: text(payload, "/action").to_owned(),
        repo: text(payload, "/repository/full_name").to_owned(),
        git_ref: format!("refs/heads/{default_branch}"),
        sha: default_head.to_owned(),
        actor: text(payload, "/sender/login").to_owned(),
        trusted: true,
        source: Source::Webhook,
        payload: payload.clone(),
        ..Event::default()
    };
    match name {
        "push" => {
            e.action = String::new();
            e.git_ref = text(payload, "/ref").to_owned();
            e.sha = text(payload, "/after").to_owned();
            let before = text(payload, "/before");
            e.before = (!before.is_empty()).then(|| before.to_owned());
            e.created = payload.pointer("/created").and_then(Value::as_bool).unwrap_or(false);
            e.deleted = payload.pointer("/deleted").and_then(Value::as_bool).unwrap_or(false);
            e.message = payload.pointer("/head_commit/message").and_then(Value::as_str).map(str::to_owned);
            if e.deleted {
                // A deleted ref runs nothing on push; GitHub sends `delete` for it.
                return None;
            }
        }
        "create" => {
            let r = text(payload, "/ref");
            e.git_ref = if text(payload, "/ref_type") == "tag" { format!("refs/tags/{r}") } else { format!("refs/heads/{r}") };
            // The created ref's commit; the caller looks it up when the payload doesn't say.
            e.sha = String::new();
        }
        "pull_request" | "pull_request_review" | "pull_request_review_comment" => {
            let n = payload.pointer("/pull_request/number").or_else(|| payload.pointer("/number")).and_then(Value::as_u64).unwrap_or(0);
            e.git_ref = format!("refs/pull/{n}/merge");
            e.sha = text(payload, "/pull_request/head/sha").to_owned();
            e.base_ref = Some(text(payload, "/pull_request/base/ref").to_owned());
            e.head_ref = Some(text(payload, "/pull_request/head/ref").to_owned());
            e.trusted = !from_fork(payload);
            e.message = None;
        }
        "pull_request_target" => {
            let base = text(payload, "/pull_request/base/ref");
            e.git_ref = format!("refs/heads/{base}");
            e.sha = String::new();
            e.base_ref = Some(base.to_owned());
            e.head_ref = Some(text(payload, "/pull_request/head/ref").to_owned());
        }
        "release" => {
            let tag = text(payload, "/release/tag_name");
            e.git_ref = format!("refs/tags/{tag}");
            e.sha = String::new();
        }
        "merge_group" => {
            e.git_ref = text(payload, "/merge_group/head_ref").to_owned();
            e.sha = text(payload, "/merge_group/head_sha").to_owned();
            e.base_ref = Some(text(payload, "/merge_group/base_ref").to_owned());
        }
        "deployment" | "deployment_status" => {
            e.sha = text(payload, "/deployment/sha").to_owned();
            let r = text(payload, "/deployment/ref");
            if !r.is_empty() {
                e.git_ref = if r.starts_with("refs/") { r.to_owned() } else { format!("refs/heads/{r}") };
            }
        }
        _ => {}
    }
    Some(e)
}

/// Whether a pull request's head lives in another repository.
pub fn from_fork(payload: &Value) -> bool {
    let head = text(payload, "/pull_request/head/repo/full_name");
    let base = text(payload, "/pull_request/base/repo/full_name");
    !head.is_empty() && !base.is_empty() && !head.eq_ignore_ascii_case(base)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn w03_r07_deliveries_get_githubs_ref_and_sha() -> Result<(), String> {
        let push = json!({ "ref": "refs/heads/dev", "before": "a".repeat(40), "after": "b".repeat(40), "repository": { "full_name": "acme/app" }, "sender": { "login": "ada" }, "head_commit": { "message": "fix [skip ci]" } });
        let e = event("push", &push, "main", "c").ok_or("push")?;
        assert_eq!((e.git_ref.as_str(), e.sha.len(), e.actor.as_str()), ("refs/heads/dev", 40, "ada"));
        assert_eq!(e.message.as_deref(), Some("fix [skip ci]"));
        assert_eq!(transition("push", &push), Some(("refs/heads/dev".into(), "b".repeat(40))));
        // A deleted branch runs nothing on push.
        let gone = json!({ "ref": "refs/heads/dev", "after": ZERO_SHA, "deleted": true });
        assert!(event("push", &gone, "main", "c").is_none());
        assert!(transition("push", &gone).is_none());
        // Comments, issues, labels and the like run on the default branch's head.
        let comment = json!({ "action": "created", "issue": { "number": 3 }, "repository": { "full_name": "acme/app" }, "sender": { "login": "bo" } });
        let e = event("issue_comment", &comment, "main", "f00").ok_or("comment")?;
        assert_eq!((e.git_ref.as_str(), e.sha.as_str(), e.action.as_str()), ("refs/heads/main", "f00", "created"));
        // Pull requests run on the merge ref; a fork's are untrusted.
        let pr = |head: &str| json!({ "action": "opened", "number": 9, "pull_request": { "number": 9, "head": { "ref": "feat", "sha": "d".repeat(40), "repo": { "full_name": head } }, "base": { "ref": "main", "repo": { "full_name": "acme/app" } } }, "repository": { "full_name": "acme/app" } });
        let e = event("pull_request", &pr("acme/app"), "main", "f00").ok_or("pr")?;
        assert_eq!((e.git_ref.as_str(), e.base_ref.as_deref(), e.trusted), ("refs/pull/9/merge", Some("main"), true));
        assert!(!event("pull_request", &pr("someone/app"), "main", "f00").ok_or("fork")?.trusted);
        assert_eq!(transition("pull_request", &pr("acme/app")), Some(("refs/pull/9/head".into(), "d".repeat(40))));
        // pull_request_target: the base branch, trusted (it runs the base's workflow).
        let e = event("pull_request_target", &pr("someone/app"), "main", "f00").ok_or("target")?;
        assert_eq!((e.git_ref.as_str(), e.trusted), ("refs/heads/main", true));
        // Releases run on their tag; pings on nothing.
        let rel = json!({ "action": "published", "release": { "tag_name": "v1.2.0" } });
        assert_eq!(event("release", &rel, "main", "f00").ok_or("release")?.git_ref, "refs/tags/v1.2.0");
        assert!(event("ping", &json!({}), "main", "f00").is_none());
        Ok(())
    }
}
