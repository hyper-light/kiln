//! Whether a workflow runs for an event (docs/ci.md W03-R03, W03-R06, W03-R09), GitHub-exact for
//! GitHub's events: activity types and their defaults, branch and tag filters (a tag push doesn't
//! run a workflow that filters only branches, and the other way round), path filters (not for
//! tags; both kinds of filter must pass), `workflow_run`'s workflows, skip markers. Every "no"
//! says why, for the run list's "why didn't this run?".

use crate::expr::{self, Budget, Condition, Dialect, MapContexts, ParseOptions, Value};
use crate::wf::ir::{Trigger, Workflow, YVal};

use super::event::Event;
use super::pattern::Patterns;
use super::skip;

/// The activity types an event runs for when `types` is not given: GitHub runs `pull_request` and
/// `pull_request_target` only for these, and every other event for all of its types.
pub fn default_types(event: &str) -> Option<&'static [&'static str]> {
    match event {
        "pull_request" | "pull_request_target" => Some(&["opened", "synchronize", "reopened"]),
        _ => None,
    }
}

/// Whether `wf` runs for `e`; `Err` says why not.
pub fn matches(wf: &Workflow, e: &Event) -> Result<(), String> {
    let triggers: Vec<&Trigger> = wf.on.iter().filter(|t| t.event == e.name).collect();
    if triggers.is_empty() {
        return Err(format!("it doesn't run on `{}`", e.name));
    }
    if skip::applies(&e.name, wf.honour_skip_ci) && e.message.as_deref().is_some_and(skip::asks_to_skip) {
        return Err("the commit message asks to skip CI".into());
    }
    let mut why = Vec::new();
    for t in triggers {
        match trigger(t, e) {
            Ok(()) => return Ok(()),
            Err(w) => why.push(w),
        }
    }
    Err(why.join("; "))
}

fn patterns(list: &[String]) -> Result<Patterns, String> {
    Patterns::new(list).map_err(|e| e.to_string())
}

/// `include`/`ignore` filters over one name (`branches`/`branches-ignore`, `tags`/`tags-ignore`).
fn names(include: &Option<Vec<String>>, ignore: &Option<Vec<String>>, name: &str, kind: &str) -> Result<(), String> {
    let plural = if kind == "branch" { "branches" } else { "tags" };
    if let Some(p) = include
        && !patterns(p)?.matches(name)
    {
        return Err(format!("{kind} `{name}` doesn't match `{plural}`"));
    }
    if let Some(p) = ignore
        && patterns(p)?.matches(name)
    {
        return Err(format!("{kind} `{name}` matches `{plural}-ignore`"));
    }
    Ok(())
}

fn branch(t: &Trigger, name: &str) -> Result<(), String> {
    names(&t.branches, &t.branches_ignore, name, "branch")
}

fn short(r: &str) -> &str {
    r.strip_prefix("refs/heads/").or_else(|| r.strip_prefix("refs/tags/")).unwrap_or(r)
}

/// `push`'s refs: branch filters for branches, tag filters for tags; only one kind given means the
/// other kind of ref doesn't run it.
fn push_ref(t: &Trigger, e: &Event) -> Result<(), String> {
    let has_branch = t.branches.is_some() || t.branches_ignore.is_some();
    let has_tag = t.tags.is_some() || t.tags_ignore.is_some();
    let name = e.ref_name();
    if e.is_tag() {
        match (has_tag, has_branch) {
            (true, _) => names(&t.tags, &t.tags_ignore, name, "tag"),
            (false, true) => Err(format!("`{name}` is a tag and it filters only branches")),
            (false, false) => Ok(()),
        }
    } else {
        match (has_branch, has_tag) {
            (true, _) => branch(t, name),
            (false, true) => Err(format!("`{name}` is a branch and it filters only tags")),
            (false, false) => Ok(()),
        }
    }
}

/// `paths`/`paths-ignore` over the changed files: not for tags; an unknown set runs (as GitHub
/// runs when the diff can't be computed); an empty one doesn't.
fn paths(t: &Trigger, e: &Event) -> Result<(), String> {
    if t.paths.is_none() && t.paths_ignore.is_none() {
        return Ok(());
    }
    if e.is_tag() {
        return Ok(());
    }
    let Some(files) = &e.changed else { return Ok(()) };
    if files.is_empty() {
        return Err("no files changed".into());
    }
    if let Some(p) = &t.paths
        && !patterns(p)?.any(files.iter().map(String::as_str))
    {
        return Err("no changed file matches `paths`".into());
    }
    if let Some(p) = &t.paths_ignore {
        let ig = patterns(p)?;
        if files.iter().all(|f| ig.matches(f)) {
            return Err("every changed file matches `paths-ignore`".into());
        }
    }
    Ok(())
}

fn types(t: &Trigger, e: &Event) -> Result<(), String> {
    let ok = if t.types.is_empty() { default_types(&e.name).is_none_or(|d| d.contains(&e.action.as_str())) } else { t.types.contains(&e.action) };
    if ok { Ok(()) } else { Err(format!("`{}` is not one of the activity types it runs for", e.action)) }
}

fn extra<'a>(t: &'a Trigger, k: &str) -> Option<&'a YVal> {
    t.extra.iter().find(|(x, _)| x == k).map(|(_, v)| v)
}

/// A kiln filter given as a string or a list: whether `value` is among them (`*` for any).
fn listed(t: &Trigger, k: &str, value: &str) -> Option<bool> {
    let v = extra(t, k)?;
    let list: Vec<&str> = match v {
        YVal::Seq(s) => s.iter().filter_map(YVal::as_str).collect(),
        YVal::Str(s) => vec![s.src.as_str()],
        YVal::Bool(b) => return Some((*b && value == "true") || (!*b && value == "false")),
        _ => return Some(false),
    };
    Some(list.iter().any(|x| *x == "*" || x.eq_ignore_ascii_case(value)))
}

fn pointer<'a>(e: &'a Event, p: &str) -> Option<&'a serde_json::Value> {
    e.payload.pointer(p)
}

fn text(e: &Event, p: &str) -> String {
    match pointer(e, p) {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Bool(b)) => b.to_string(),
        Some(v) if !v.is_null() => v.to_string(),
        _ => String::new(),
    }
}

fn trigger(t: &Trigger, e: &Event) -> Result<(), String> {
    match e.name.as_str() {
        "push" => {
            push_ref(t, e)?;
            paths(t, e)
        }
        "pull_request" | "pull_request_target" => {
            types(t, e)?;
            branch(t, e.base_ref.as_deref().unwrap_or_default())?;
            paths(t, e)
        }
        "merge_group" => {
            types(t, e)?;
            branch(t, short(e.base_ref.as_deref().unwrap_or_default()))
        }
        "workflow_run" => {
            types(t, e)?;
            let name = e.workflow.as_deref().unwrap_or_default();
            if !t.workflows.is_empty() && !patterns(&t.workflows)?.matches(name) {
                return Err(format!("the finished workflow `{name}` is not one of its `workflows`"));
            }
            branch(t, e.workflow_branch.as_deref().unwrap_or_default())
        }
        "schedule" => {
            if t.schedules.iter().any(|s| s.cron == e.action) {
                Ok(())
            } else {
                Err(format!("no schedule of it is `{}`", e.action))
            }
        }
        "repository_dispatch" => types(t, e),
        // kiln events (W03-R09). Their payloads carry `kiln.*`.
        "diff" => {
            branch(t, e.base_ref.as_deref().unwrap_or_default())?;
            paths(t, e)?;
            let draft = text(e, "/kiln/diff/draft") == "true";
            if draft && listed(t, "drafts", "true") != Some(true) {
                return Err("the diff is a draft and `drafts` isn't true".into());
            }
            match listed(t, "authors", &text(e, "/kiln/diff/author_kind")) {
                Some(false) => Err("the diff's author is not one of `authors`".into()),
                _ => Ok(()),
            }
        }
        "land" => branch(t, e.base_ref.as_deref().unwrap_or_default()),
        "review" => match listed(t, "states", &e.action) {
            Some(false) => Err(format!("the review is `{}`, not one of `states`", e.action)),
            _ => Ok(()),
        },
        "comment" => {
            let body = text(e, "/comment/body");
            if let Some(YVal::Seq(cmds)) = extra(t, "commands")
                && !cmds.iter().filter_map(YVal::as_str).any(|c| body.trim_start().starts_with(c))
            {
                return Err("the comment starts with none of `commands`".into());
            }
            if let Some(YVal::Seq(names)) = extra(t, "mentions") {
                let mentions: Vec<String> = pointer(e, "/kiln/mentions").and_then(serde_json::Value::as_array).map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()).unwrap_or_default();
                if !names.iter().filter_map(YVal::as_str).any(|n| mentions.iter().any(|m| m.eq_ignore_ascii_case(n))) {
                    return Err("the comment mentions none of `mentions`".into());
                }
            }
            Ok(())
        }
        "pipeline" => {
            let name = e.workflow.as_deref().unwrap_or_default();
            let path = e.workflow_path.as_deref().unwrap_or_default();
            if let Some(v) = extra(t, "name").and_then(YVal::as_str)
                && v != name
                && v != path
            {
                return Err(format!("the finished workflow is `{name}`, not `{v}`"));
            }
            if listed(t, "conclusion", e.conclusion.as_deref().unwrap_or_default()) == Some(false) {
                return Err("the finished run's conclusion is not one of `conclusion`".into());
            }
            branch(t, e.workflow_branch.as_deref().unwrap_or_default())
        }
        "agent" => match listed(t, "agents", &e.actor) {
            Some(false) => Err(format!("`{}` is not one of its `agents`", e.actor)),
            _ => Ok(()),
        },
        "deploy" => {
            for (k, p) in [("app", "/kiln/deploy/app"), ("environment", "/kiln/deploy/environment"), ("states", "/kiln/deploy/state")] {
                if listed(t, k, &text(e, p)) == Some(false) {
                    return Err(format!("the deployment's {k} is not one of `{k}`"));
                }
            }
            Ok(())
        }
        "webhook" => {
            if let Some(h) = extra(t, "hook").and_then(YVal::as_str)
                && h != text(e, "/kiln/hook/id")
            {
                return Err(format!("the call is to another hook, not `{h}`"));
            }
            if let Some(f) = extra(t, "filter").and_then(YVal::as_str) {
                let c = Condition::parse(f, &ParseOptions::permissive(Dialect::Kiln)).map_err(|e| format!("`filter`: {}", e.message))?;
                let cx = MapContexts::default().with("event", Value::from_json(&e.payload));
                if !expr::eval_condition(&c, &cx, &mut Budget::default()).map_err(|e| format!("`filter`: {}", e.message))? {
                    return Err("the call doesn't pass `filter`".into());
                }
            }
            Ok(())
        }
        // The rest run for every activity type unless `types` says otherwise.
        _ => types(t, e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wf::parse_workflow;

    fn wf(on: &str, dialect: Dialect) -> Workflow {
        let src = format!("name: t\non:\n{on}\njobs:\n  a:\n    runs-on: x\n    steps: [{{run: a}}]\n");
        let (w, d) = parse_workflow(if dialect == Dialect::Gha { ".github/workflows/t.yml" } else { ".kiln/t.yaml" }, &src, dialect);
        assert!(d.iter().all(|x| !x.is_error()), "{d:?}");
        w.unwrap_or_default()
    }

    fn push(r: &str, changed: Option<&[&str]>) -> Event {
        Event { name: "push".into(), git_ref: r.into(), changed: changed.map(|c| c.iter().map(|s| (*s).to_owned()).collect()), ..Event::default() }
    }

    fn pr(action: &str, base: &str, changed: &[&str]) -> Event {
        Event { name: "pull_request".into(), action: action.into(), base_ref: Some(base.into()), git_ref: "refs/pull/1/merge".into(), changed: Some(changed.iter().map(|s| (*s).to_owned()).collect()), ..Event::default() }
    }

    /// W03-AC01: GitHub's documented matching, case by case.
    #[test]
    fn w03_ac01_push_branches_tags_and_paths() {
        let g = Dialect::Gha;
        // No filters: every branch and tag.
        let w = wf("  push:", g);
        assert!(matches(&w, &push("refs/heads/x", None)).is_ok() && matches(&w, &push("refs/tags/v1", None)).is_ok());
        // Only branches: tags don't run it, and the other way round.
        let w = wf("  push:\n    branches: [main, 'releases/**']", g);
        assert!(matches(&w, &push("refs/heads/main", None)).is_ok());
        assert!(matches(&w, &push("refs/heads/releases/1/2", None)).is_ok());
        assert!(matches(&w, &push("refs/heads/feature", None)).is_err());
        assert!(matches(&w, &push("refs/tags/v1", None)).is_err_and(|e| e.contains("filters only branches")));
        let w = wf("  push:\n    tags: ['v[0-9]*']", g);
        assert!(matches(&w, &push("refs/tags/v1.2", None)).is_ok() && matches(&w, &push("refs/heads/main", None)).is_err());
        // Both kinds: each ref by its own.
        let w = wf("  push:\n    branches: [main]\n    tags: [v1]", g);
        assert!(matches(&w, &push("refs/heads/main", None)).is_ok() && matches(&w, &push("refs/tags/v1", None)).is_ok() && matches(&w, &push("refs/tags/v2", None)).is_err());
        // Negation, and re-inclusion.
        let w = wf("  push:\n    branches: ['releases/**', '!releases/**-alpha']", g);
        assert!(matches(&w, &push("refs/heads/releases/10", None)).is_ok() && matches(&w, &push("refs/heads/releases/10-alpha", None)).is_err());
        let w = wf("  push:\n    branches-ignore: ['mona/octocat', 'releases/**-alpha']", g);
        assert!(matches(&w, &push("refs/heads/main", None)).is_ok() && matches(&w, &push("refs/heads/mona/octocat", None)).is_err());
        // Paths: one matching file is enough; ignore needs every file ignored; tags skip paths.
        let w = wf("  push:\n    paths: ['**.js']", g);
        assert!(matches(&w, &push("refs/heads/m", Some(&["a.md", "src/app.js"]))).is_ok());
        assert!(matches(&w, &push("refs/heads/m", Some(&["a.md"]))).is_err_and(|e| e.contains("paths")));
        assert!(matches(&w, &push("refs/heads/m", Some(&[]))).is_err_and(|e| e.contains("no files changed")));
        assert!(matches(&w, &push("refs/heads/m", None)).is_ok(), "an unknown diff runs");
        assert!(matches(&w, &push("refs/tags/v1", Some(&["a.md"]))).is_ok(), "path filters are not evaluated for tags");
        let w = wf("  push:\n    paths-ignore: ['docs/**']", g);
        assert!(matches(&w, &push("refs/heads/m", Some(&["docs/a.md", "src/x.rs"]))).is_ok());
        assert!(matches(&w, &push("refs/heads/m", Some(&["docs/a.md", "docs/b/c.md"]))).is_err());
        // Branches and paths: both must pass.
        let w = wf("  push:\n    branches: [main]\n    paths: ['src/**']", g);
        assert!(matches(&w, &push("refs/heads/main", Some(&["src/a"]))).is_ok());
        assert!(matches(&w, &push("refs/heads/dev", Some(&["src/a"]))).is_err() && matches(&w, &push("refs/heads/main", Some(&["README"]))).is_err());
    }

    #[test]
    fn w03_ac01_pull_requests_and_types() {
        let g = Dialect::Gha;
        let w = wf("  pull_request:\n    branches: [main]", g);
        // The default types: opened, synchronize, reopened.
        for a in ["opened", "synchronize", "reopened"] {
            assert!(matches(&w, &pr(a, "main", &["x"])).is_ok(), "{a}");
        }
        assert!(matches(&w, &pr("closed", "main", &["x"])).is_err() && matches(&w, &pr("labeled", "main", &["x"])).is_err());
        // Branch filters are the base branch's.
        assert!(matches(&w, &pr("opened", "dev", &["x"])).is_err());
        let w = wf("  pull_request:\n    types: [closed, labeled]", g);
        assert!(matches(&w, &pr("closed", "x", &[])).is_ok() && matches(&w, &pr("opened", "x", &[])).is_err());
        // Other events run for all their types by default.
        let w = wf("  issues:", g);
        assert!(matches(&w, &Event { name: "issues".into(), action: "transferred".into(), ..Event::default() }).is_ok());
        // repository_dispatch's types are its event_type.
        let w = wf("  repository_dispatch:\n    types: [deploy]", g);
        assert!(matches(&w, &Event { name: "repository_dispatch".into(), action: "deploy".into(), ..Event::default() }).is_ok());
        assert!(matches(&w, &Event { name: "repository_dispatch".into(), action: "test".into(), ..Event::default() }).is_err());
        // An event it doesn't list.
        assert!(matches(&w, &push("refs/heads/main", None)).is_err_and(|e| e.contains("doesn't run on `push`")));
    }

    #[test]
    fn w03_ac01_workflow_run_merge_group_and_schedule() {
        let g = Dialect::Gha;
        let w = wf("  workflow_run:\n    workflows: [Build]\n    types: [completed]\n    branches: [main]", g);
        let done = |name: &str, branch: &str, action: &str| Event { name: "workflow_run".into(), action: action.into(), workflow: Some(name.into()), workflow_branch: Some(branch.into()), ..Event::default() };
        assert!(matches(&w, &done("Build", "main", "completed")).is_ok());
        assert!(matches(&w, &done("Test", "main", "completed")).is_err() && matches(&w, &done("Build", "dev", "completed")).is_err() && matches(&w, &done("Build", "main", "requested")).is_err());
        let w = wf("  merge_group:\n    branches: [main]", g);
        let mg = |b: &str| Event { name: "merge_group".into(), action: "checks_requested".into(), base_ref: Some(b.into()), ..Event::default() };
        assert!(matches(&w, &mg("refs/heads/main")).is_ok() && matches(&w, &mg("refs/heads/dev")).is_err());
        let w = wf("  schedule:\n    - cron: '0 3 * * *'\n    - cron: '30 5 * * 1'", g);
        assert!(matches(&w, &Event { name: "schedule".into(), action: "30 5 * * 1".into(), ..Event::default() }).is_ok());
        assert!(matches(&w, &Event { name: "schedule".into(), action: "0 4 * * *".into(), ..Event::default() }).is_err());
    }

    #[test]
    fn w03_ac03_skip_markers_in_matching() {
        let w = wf("  push:\n  workflow_dispatch:", Dialect::Gha);
        let skipped = Event { message: Some("docs [skip ci]".into()), ..push("refs/heads/main", None) };
        assert!(matches(&w, &skipped).is_err_and(|e| e.contains("skip CI")));
        assert!(matches(&w, &Event { name: "workflow_dispatch".into(), message: Some("[skip ci]".into()), ..Event::default() }).is_ok());
    }

    #[test]
    fn kiln_events() {
        let k = Dialect::Kiln;
        let w = wf("  diff:\n    branches: [main]\n    authors: [human]\n  review:\n    states: [accepted]\n  agent:\n    agents: [triage]\n  pipeline:\n    name: build\n    conclusion: [success]\n  webhook:\n    hook: h1\n    filter: event.action == 'opened'", k);
        let diff = |kind: &str, draft: bool| Event { name: "diff".into(), base_ref: Some("main".into()), payload: serde_json::json!({"kiln": {"diff": {"author_kind": kind, "draft": draft}}}), ..Event::default() };
        assert!(matches(&w, &diff("human", false)).is_ok());
        assert!(matches(&w, &diff("agent", false)).is_err() && matches(&w, &diff("human", true)).is_err_and(|e| e.contains("draft")));
        assert!(matches(&w, &Event { name: "review".into(), action: "accepted".into(), ..Event::default() }).is_ok());
        assert!(matches(&w, &Event { name: "review".into(), action: "commented".into(), ..Event::default() }).is_err());
        assert!(matches(&w, &Event { name: "agent".into(), actor: "triage".into(), ..Event::default() }).is_ok());
        assert!(matches(&w, &Event { name: "agent".into(), actor: "other".into(), ..Event::default() }).is_err());
        let pipe = |c: &str| Event { name: "pipeline".into(), workflow: Some("build".into()), conclusion: Some(c.into()), ..Event::default() };
        assert!(matches(&w, &pipe("success")).is_ok() && matches(&w, &pipe("failure")).is_err());
        let hook = |id: &str, action: &str| Event { name: "webhook".into(), payload: serde_json::json!({"action": action, "kiln": {"hook": {"id": id}}}), ..Event::default() };
        assert!(matches(&w, &hook("h1", "opened")).is_ok());
        assert!(matches(&w, &hook("h2", "opened")).is_err() && matches(&w, &hook("h1", "closed")).is_err());
    }
}
