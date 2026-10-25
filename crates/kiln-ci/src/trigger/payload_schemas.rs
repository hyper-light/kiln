//! W03-AC04: the payloads kiln makes, checked against GitHub's webhook schemas as
//! `@octokit/webhooks-schemas` 7.6.1 writes them (`schemas/`: the part these events need). The
//! checker knows the keywords those schemas use: `$ref`, `type`, `enum`, `required`,
//! `properties`, `additionalProperties`, `items`, `allOf`, `oneOf` and the common `format`s.

use serde_json::{Value, json};

use super::context::{self, Commit, Counts, Person, PullRequest, Repo, RunSummary, Side};
use super::event::Event;

const SCHEMA: &str = include_str!("schemas/octokit-webhooks-7.6.1.json");

fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

fn type_ok(t: &str, v: &Value) -> bool {
    match t {
        "object" => v.is_object(),
        "array" => v.is_array(),
        "string" => v.is_string(),
        "integer" => v.is_i64() || v.is_u64(),
        "number" => v.is_number(),
        "boolean" => v.is_boolean(),
        "null" => v.is_null(),
        _ => true,
    }
}

/// What's wrong with `v` against schema `s` (in `root`), each with where.
fn validate(root: &Value, s: &Value, v: &Value, path: &str, errors: &mut Vec<String>) {
    let Some(o) = s.as_object() else { return };
    // Draft 7: a `$ref` is the whole schema.
    if let Some(r) = o.get("$ref").and_then(Value::as_str) {
        match r.strip_prefix('#').and_then(|p| root.pointer(p)) {
            Some(t) => validate(root, t, v, path, errors),
            None => errors.push(format!("{path}: unknown $ref {r}")),
        }
        return;
    }
    if let Some(t) = o.get("type") {
        let ok = match t {
            Value::String(t) => type_ok(t, v),
            Value::Array(ts) => ts.iter().filter_map(Value::as_str).any(|t| type_ok(t, v)),
            _ => true,
        };
        if !ok {
            errors.push(format!("{path}: is {}, not {t}", kind(v)));
            return;
        }
    }
    if let Some(e) = o.get("enum").and_then(Value::as_array)
        && !e.contains(v)
    {
        errors.push(format!("{path}: {v} isn't one of {}", Value::Array(e.clone())));
    }
    for s in o.get("allOf").and_then(Value::as_array).into_iter().flatten() {
        validate(root, s, v, path, errors);
    }
    if let Some(one) = o.get("oneOf").and_then(Value::as_array) {
        let mut why = Vec::new();
        let passing = one
            .iter()
            .filter(|s| {
                let mut e = Vec::new();
                validate(root, s, v, path, &mut e);
                why.extend(e.iter().take(3).cloned());
                e.is_empty()
            })
            .count();
        if passing != 1 {
            errors.push(format!("{path}: matches {passing} of oneOf's {} ({})", one.len(), why.join("; ")));
        }
    }
    if let Some(obj) = v.as_object() {
        let props = o.get("properties").and_then(Value::as_object);
        for k in o.get("required").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
            if !obj.contains_key(k) {
                errors.push(format!("{path}/{k}: required"));
            }
        }
        for (k, x) in obj {
            let at = format!("{path}/{k}");
            match props.and_then(|p| p.get(k)) {
                Some(ps) => validate(root, ps, x, &at, errors),
                None => match o.get("additionalProperties") {
                    Some(Value::Bool(false)) => errors.push(format!("{at}: not a property it has")),
                    Some(ap @ Value::Object(_)) => validate(root, ap, x, &at, errors),
                    _ => {}
                },
            }
        }
    }
    if let (Some(items), Some(arr)) = (o.get("items"), v.as_array()) {
        for (i, x) in arr.iter().enumerate() {
            validate(root, items, x, &format!("{path}/{i}"), errors);
        }
    }
    if let (Some(f), Some(text)) = (o.get("format").and_then(Value::as_str), v.as_str()) {
        let ok = match f {
            "date-time" => text.len() >= 20 && text.as_bytes().get(10) == Some(&b'T') && (text.ends_with('Z') || text.get(text.len() - 6..).is_some_and(|z| z.starts_with(['+', '-']))),
            "uri" => text.contains("://"),
            "email" => text.contains('@'),
            _ => true,
        };
        if !ok {
            errors.push(format!("{path}: `{text}` isn't a {f}"));
        }
    }
}

/// What's wrong with `v` as GitHub's `def` (`push$event`, `pull_request$opened`, …). kiln's own
/// namespace (`kiln`, which GitHub's workflows never read) is set aside first.
fn check(def: &str, v: &Value) -> Vec<String> {
    let root: Value = serde_json::from_str(SCHEMA).unwrap_or_default();
    let mut v = v.clone();
    if let Some(o) = v.as_object_mut() {
        o.remove("kiln");
    }
    let mut errors = Vec::new();
    match root.pointer(&format!("/definitions/{def}")) {
        Some(s) => validate(&root, s, &v, "", &mut errors),
        None => errors.push(format!("no definition `{def}`")),
    }
    errors
}

fn repo() -> Repo {
    Repo { full_name: "acme/app".into(), id: 7, owner_id: 3, default_branch: "main".into(), private: false, server_url: "https://kiln.example".into(), created_at: 1_792_000_000 }
}

fn person(name: &str) -> Person {
    Person { name: name.into(), email: format!("{}@example.com", name.to_ascii_lowercase()), username: Some(name.to_ascii_lowercase()) }
}

fn commit(n: u32) -> Commit {
    Commit { id: format!("{n:040x}"), tree_id: format!("{:040x}", n + 1000), message: format!("change {n}\n\nwhy"), timestamp: "2026-10-19T12:00:00Z".into(), author: person("Ada"), committer: person("Ada") }
}

/// The checker catches what it should.
#[test]
fn w03_ac04_the_checker() {
    assert!(check("push$event", &json!({})).iter().any(|e| e.contains("/ref: required")));
    assert!(check("push$event", &json!({ "ref": 3 })).iter().any(|e| e.contains("/ref: is a number")));
    assert!(check("pull_request$opened", &json!({ "action": "closed" })).iter().any(|e| e.contains("/action")));
    assert!(!check("nope", &json!({})).is_empty());
}

/// Synthesized `push`, `pull_request`, `workflow_dispatch`, `merge_group` and `workflow_run`
/// payloads are GitHub's (`schedule` has no webhook, so no schema; `release` is the facade's).
#[test]
fn w03_ac04_payloads_are_githubs() {
    let mut failures: Vec<String> = Vec::new();
    let mut expect = |what: &str, def: &str, v: &Value| {
        for e in check(def, v) {
            failures.push(format!("{what} ({def}){e}"));
        }
    };
    // A push of two commits to main, and a branch's first push. GitHub's payloads for Actions
    // leave out the commits' `added`, `removed` and `modified` (its docs say so), as kiln's do; the
    // schema is the webhooks', which have them.
    let with_files = |mut v: Value| {
        let add = |c: &mut Value| {
            if let Some(o) = c.as_object_mut() {
                for k in ["added", "removed", "modified"] {
                    o.insert(k.into(), json!([]));
                }
            }
        };
        for c in v.get_mut("commits").and_then(Value::as_array_mut).into_iter().flatten() {
            add(c);
        }
        if let Some(h) = v.get_mut("head_commit") {
            add(h);
        }
        v
    };
    let e = Event { name: "push".into(), git_ref: "refs/heads/main".into(), sha: format!("{:040x}", 2), before: Some(format!("{:040x}", 1)), actor: "ada".into(), ..Event::default() };
    expect("a push", "push$event", &with_files(context::push(&e, &repo(), &[commit(1), commit(2)], &person("Ada"), false)));
    let first = Event { before: None, git_ref: "refs/heads/feature".into(), ..e.clone() };
    expect("a first push", "push$event", &with_files(context::push(&first, &repo(), &[commit(2)], &person("Ada"), false)));
    // A pull request, opened, synchronized and closed.
    let pr = PullRequest {
        number: 12,
        title: Some("A change".into()),
        body: Some("Why it changes".into()),
        draft: Some(false),
        state: "open".into(),
        author: Some("ada".into()),
        labels: Some(vec!["bug".into()]),
        head: Side { branch: "feature".into(), sha: format!("{:040x}", 2), repo: repo() },
        base: Side { branch: "main".into(), sha: format!("{:040x}", 1), repo: repo() },
        created_at: Some("2026-10-19T11:00:00Z".into()),
        updated_at: Some("2026-10-19T12:00:00Z".into()),
        counts: Counts { commits: 2, additions: 12, deletions: 3, changed_files: 2 },
    };
    for (action, def) in [("opened", "pull_request$opened"), ("synchronize", "pull_request$synchronize"), ("reopened", "pull_request$reopened")] {
        let e = Event { name: "pull_request".into(), action: action.into(), git_ref: "refs/pull/12/merge".into(), sha: format!("{:040x}", 3), before: Some(format!("{:040x}", 1)), actor: "ada".into(), ..Event::default() };
        expect(&format!("a pull request {action}"), def, &context::pull_request(&e, &repo(), &pr));
    }
    let closed = PullRequest { state: "closed".into(), ..pr.clone() };
    let e = Event { name: "pull_request".into(), action: "closed".into(), git_ref: "refs/pull/12/merge".into(), sha: format!("{:040x}", 3), actor: "ada".into(), ..Event::default() };
    expect("a pull request closed", "pull_request$closed", &context::pull_request(&e, &repo(), &closed));
    // A dispatch with inputs.
    let e = Event { name: "workflow_dispatch".into(), git_ref: "refs/heads/main".into(), sha: format!("{:040x}", 2), actor: "ada".into(), ..Event::default() };
    expect("a dispatch", "workflow_dispatch$event", &context::workflow_dispatch(&e, &repo(), ".github/workflows/ci.yml", &json!({ "level": "debug" })));
    // A merge group's candidate.
    expect("a merge group", "merge_group$checks_requested", &context::merge_group(&repo(), "main", &format!("{:040x}", 1), &commit(3), "refs/kiln/land/main/b1", &person("Ada")));
    // A run requested and completed.
    let run = RunSummary {
        id: 99,
        name: "CI".into(),
        path: ".github/workflows/ci.yml".into(),
        display_title: "change 2".into(),
        run_number: 4,
        run_attempt: 1,
        event: "push".into(),
        status: "completed".into(),
        conclusion: Some("success".into()),
        head_branch: "main".into(),
        head_sha: format!("{:040x}", 2),
        head_message: "change 2".into(),
        actor: "ada".into(),
        created_at: "2026-10-19T12:00:00Z".into(),
        updated_at: "2026-10-19T12:05:00Z".into(),
    };
    expect("a run completed", "workflow_run$completed", &context::workflow_run("completed", &run, &repo()));
    let requested = RunSummary { status: "requested".into(), conclusion: None, ..run };
    expect("a run requested", "workflow_run$requested", &context::workflow_run("requested", &requested, &repo()));
    assert!(failures.is_empty(), "{} differences from GitHub's schemas:\n{}", failures.len(), failures.join("\n"));
}
