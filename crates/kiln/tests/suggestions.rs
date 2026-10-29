//! Suggestions as GitHub-flavoured blocks, applied as a revision of whoever applies them
//! (docs/ci.md W26-R10, AC03), against a server of its own with real `git`: on a diff and on a
//! change request, a one-line and a multi-line block, a comment with two to choose from, one in the
//! old `suggestion` field, a draft's, one applied twice and an outdated one.

#![cfg(unix)]

mod common;

use std::path::Path;
use std::process::Command;

use common::{Options, Server, http, start};
use serde_json::{Value, json};

/// `git` with no configuration but this test's, in `dir`, as `Sam <sam@example.com>`.
fn git(home: &Path, dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let o = Command::new("git")
        .args(["-c", "user.name=Sam", "-c", "user.email=sam@example.com", "-c", "init.defaultBranch=main", "-c", "credential.helper=", "-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_ASKPASS")
        .env_remove("SSH_ASKPASS")
        .output()?;
    let out = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    anyhow::ensure!(o.status.success(), "git {}: {out}", args.join(" "));
    Ok(out)
}

/// A token for `body` (a person's or an agent's).
fn token(s: &Server, body: &Value) -> anyhow::Result<String> {
    let (st, text) = http(s.port, "POST", "/api/tokens", &s.admin, Some(body))?;
    anyhow::ensure!((200..300).contains(&st), "token {body}: {st} {text}");
    serde_json::from_str::<Value>(&text)?.get("token").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("no token in {text}"))
}

/// A developer of acme/app, and a token of theirs.
fn person(s: &Server, name: &str) -> anyhow::Result<String> {
    let (st, text) = http(s.port, "POST", "/api/users", &s.admin, Some(&json!({ "name": name, "grants": ["developer@repo:acme/app"] })))?;
    anyhow::ensure!((200..300).contains(&st), "{name}: {st} {text}");
    token(s, &json!({ "kind": "pat", "user": name, "grants": ["owner"] }))
}

fn call(s: &Server, token: &str, method: &str, path: &str, body: Option<&Value>) -> anyhow::Result<(u16, Value)> {
    let (st, text) = http(s.port, method, path, token, body)?;
    Ok((st, serde_json::from_str(&text).unwrap_or(Value::String(text))))
}

fn get(s: &Server, token: &str, path: &str) -> anyhow::Result<Value> {
    let (st, v) = call(s, token, "GET", path, None)?;
    anyhow::ensure!(st == 200, "GET {path}: {st} {v}");
    Ok(v)
}

fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// matrix.rs, 45 lines (`let r1 = 1;` to `let r45 = 45;`), each line `n` of `changed` replaced
/// by its lines (none: it's gone).
fn matrix(changed: &[(usize, &[&str])]) -> String {
    let lines: Vec<String> = (1..=45)
        .flat_map(|n| match changed.iter().find(|(m, _)| *m == n) {
            Some((_, with)) => with.iter().map(|l| (*l).to_owned()).collect::<Vec<_>>(),
            None => vec![format!("let r{n} = {n};")],
        })
        .collect();
    format!("{}\n", lines.join("\n"))
}

/// A native repository acme/app whose trunk holds a README, cloned as sam's into `work`; sam,
/// ada, kim and the review agent (an agent of kim's) may work on it.
struct Repo {
    s: Server,
    home: std::path::PathBuf,
    work: std::path::PathBuf,
    ada: String,
    kim: String,
    agent: String,
}

fn repo(dir: &Path) -> anyhow::Result<Repo> {
    let s = start(dir, &Options::default())?;
    let home = dir.join("githome");
    std::fs::create_dir_all(&home)?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "native repository: {st} {body}");
    let (sam, ada, kim) = (person(&s, "sam")?, person(&s, "ada")?, person(&s, "kim")?);
    let agent = token(&s, &json!({ "kind": "agent", "agent": "review", "user": "kim", "grants": ["developer@repo:acme/app"] }))?;
    git(&home, dir, &["clone", "-q", &format!("http://x:{sam}@127.0.0.1:{}/git/acme/app.git", s.port), "work"])?;
    let work = dir.join("work");
    std::fs::write(work.join("README.md"), "app\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", "origin", "main"])?;
    Ok(Repo { s, home, work, ada, kim, agent })
}

impl Repo {
    fn g(&self, args: &[&str]) -> anyhow::Result<String> {
        git(&self.home, &self.work, args)
    }

    /// A comment on `change`: its id and its thread's.
    fn comment(&self, token: &str, change: &str, body: Value) -> anyhow::Result<(String, String)> {
        let (st, c) = call(&self.s, token, "POST", &format!("/api/{change}/comments"), Some(&body))?;
        anyhow::ensure!(st == 200, "comment {body}: {st} {c}");
        Ok((text(&c, "id").to_owned(), text(&c, "thread").to_owned()))
    }

    fn apply(&self, token: &str, comment: &str, body: Value) -> anyhow::Result<(u16, Value)> {
        call(&self.s, token, "POST", &format!("/api/comments/{comment}/apply"), Some(&body))
    }

    /// The commit at `rev` of the server's repository, fetched: (author, committer, message).
    fn commit(&self, rev: &str) -> anyhow::Result<(String, String, String)> {
        self.g(&["fetch", "-q", "origin", rev])?;
        let said = self.g(&["log", "-1", "--format=%an <%ae>%x00%cn <%ce>%x00%B", "FETCH_HEAD"])?;
        let mut f = said.splitn(3, '\0');
        Ok((f.next().unwrap_or_default().to_owned(), f.next().unwrap_or_default().to_owned(), f.next().unwrap_or_default().trim_end().to_owned()))
    }

    /// matrix.rs as the server's `rev` has it.
    fn file(&self, rev: &str) -> anyhow::Result<String> {
        self.g(&["fetch", "-q", "origin", rev])?;
        self.g(&["show", "FETCH_HEAD:matrix.rs"])
    }

    /// The thread whose first comment is `comment`, as `change`'s threads show it.
    fn thread(&self, change: &str, comment: &str) -> anyhow::Result<Value> {
        let all = get(&self.s, &self.ada, &format!("/api/{change}/comments"))?;
        Ok(all.get("threads").and_then(Value::as_array).into_iter().flatten().find(|t| t.pointer("/comments/0/id").and_then(Value::as_str) == Some(comment)).cloned().unwrap_or_default())
    }
}

/// D1, a diff of sam's (one commit, amended by each Apply): the review agent's one-line block on
/// matrix.rs:42, kim's comment offering two blocks for lines 10–12 (the second applied: three lines
/// become two), kim's old-field suggestion on line 5, an outdated block on line 20 and a draft's.
/// Each applies as a revision of ada's, who applies it: the commit amended, still sam's, committed by
/// ada, the suggester a co-author, unsigned until W27-R03 signs it; the file changed as the block
/// says; the revision says why, and the block and the thread say which revision applied it.
#[test]
fn w26_ac03_a_diffs_suggestion_is_a_revision_of_whoever_applies_it() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let r = repo(dir.path())?;
    let change_id = format!("I{}", "4".repeat(40));
    std::fs::write(r.work.join("matrix.rs"), matrix(&[]))?;
    r.g(&["add", "matrix.rs"])?;
    r.g(&["commit", "-qm", &format!("pipeline: dynamic matrices\n\nChange-Id: {change_id}")])?;
    r.g(&["push", "-q", "origin", "HEAD:refs/for/main"])?;
    let d1 = "diffs/D1";
    let empty = "if m.is_empty() { return Ok(vec![]) }";

    // The review agent's block, kim's two blocks, kim's old field, a block on line 20, kim's draft.
    let (agents, agents_thread) = r.comment(&r.agent, d1, json!({ "file": "matrix.rs", "start_line": 42, "body": format!("An empty matrix panics here.\n\n```suggestion\n{empty}\n```\n"), "publish": true }))?;
    let two = "Either\n\n```suggestion\nlet r10_12 = 33;\n```\n\nor, keeping r10:\n\n```suggestion\nlet r10 = 10;\nlet r11_12 = 23;\n```\n";
    let (kims, _) = r.comment(&r.kim, d1, json!({ "file": "matrix.rs", "start_line": 10, "end_line": 12, "body": two, "publish": true }))?;
    let (field, _) = r.comment(&r.kim, d1, json!({ "file": "matrix.rs", "start_line": 5, "body": "name it", "suggestion": "let r5 = five();", "publish": true }))?;
    let (late, _) = r.comment(&r.kim, d1, json!({ "file": "matrix.rs", "start_line": 20, "body": "```suggestion\nlet r20 = twenty();\n```", "publish": true }))?;
    let (draft, _) = r.comment(&r.kim, d1, json!({ "file": "matrix.rs", "start_line": 30, "body": "```suggestion\nlet r30 = 0;\n```" }))?;

    // sam changes line 20 himself: D1 v2, and the block on it is outdated.
    std::fs::write(r.work.join("matrix.rs"), matrix(&[(20, &["let r20 = 2 * 10;"])]))?;
    r.g(&["commit", "-q", "--amend", "--no-edit", "-a"])?;
    r.g(&["push", "-q", "origin", "HEAD:refs/for/main"])?;
    let versions = |v: &Value| v.get("versions").and_then(Value::as_array).map_or(0, Vec::len);
    anyhow::ensure!(versions(&get(&r.s, &r.ada, "/api/diffs/D1")?) == 2, "sam's own change is v2");
    let (st, out) = r.apply(&r.ada, &late, json!({}))?;
    assert_eq!((st, text(&out, "error")), (409, "matrix.rs:20: this suggestion is outdated: its lines changed since v1"), "{out}");
    let t = r.thread(d1, &late)?;
    assert_eq!((t.get("outdated"), t.pointer("/comments/0/suggestions/0/outdated")), (Some(&json!(true)), Some(&json!(true))), "an outdated block says so: {t}");
    // A draft's suggestion waits for its review, as on GitHub.
    let (st, out) = r.apply(&r.kim, &draft, json!({}))?;
    assert_eq!((st, text(&out, "error")), (409, "matrix.rs:30: a draft's suggestion applies once its review is published"), "{out}");
    anyhow::ensure!(versions(&get(&r.s, &r.ada, "/api/diffs/D1")?) == 2, "neither applied");

    // The review agent's one-line block, applied by ada: v3, sam's commit amended, committed by ada,
    // the agent a co-author; unsigned until W27-R03 signs it, never signed as anyone else.
    let t = r.thread(d1, &agents)?;
    assert_eq!(t.pointer("/comments/0/suggestions"), Some(&json!([{ "index": 0, "lines": [empty], "field": false, "applied": null, "outdated": false }])), "{t}");
    let (st, out) = r.apply(&r.ada, &agents, json!({}))?;
    anyhow::ensure!(st == 200, "apply: {st} {out}");
    assert_eq!((out.get("version"), out.get("state"), out.get("signer")), (Some(&json!(3)), Some(&json!("resolved")), Some(&Value::Null)), "{out}");
    let (author, committer, message) = r.commit("refs/kiln/diffs/D1/v3")?;
    assert_eq!((author.as_str(), committer.as_str()), ("Sam <sam@example.com>", "ada <ada@kiln>"));
    assert_eq!(message, format!("pipeline: dynamic matrices\n\nChange-Id: {change_id}\nCo-authored-by: agent:review <agent-review@kiln>"));
    assert_eq!(text(&out, "commit"), r.g(&["rev-parse", "FETCH_HEAD"])?.trim());
    assert!(!r.g(&["cat-file", "commit", "FETCH_HEAD"])?.contains("gpgsig"), "unsigned until W27-R03");
    assert_eq!(r.file("refs/kiln/diffs/D1/v3")?, matrix(&[(20, &["let r20 = 2 * 10;"]), (42, &[empty])]));
    // The diff shows it; the revision says why, the block and the thread which revision applied it.
    let files = get(&r.s, &r.ada, "/api/diffs/D1/versions/3/files")?;
    assert!(text(&files, "patch").contains(&format!("+{empty}\n")), "{files}");
    let v3 = get(&r.s, &r.ada, "/api/diffs/D1")?.pointer("/versions/2").cloned().unwrap_or_default();
    assert_eq!((text(&v3, "why"), text(&v3, "thread"), text(&v3, "note")), ("suggestion applied", agents_thread.as_str(), "applied a suggestion from agent:review"), "{v3}");
    let t = r.thread(d1, &agents)?;
    assert_eq!((text(&t, "state"), t.pointer("/comments/0/suggestions/0/applied/version"), t.pointer("/comments/0/suggestions/0/outdated")), ("resolved", Some(&json!(3)), Some(&json!(false))), "{t}");
    assert_eq!((t.pointer("/comments/1/author"), t.pointer("/comments/1/body")), (Some(&json!("user:ada")), Some(&json!("Applied in v3."))), "{t}");
    let (st, out) = r.apply(&r.ada, &agents, json!({}))?;
    assert_eq!((st, text(&out, "error")), (409, "matrix.rs:42: this suggestion was applied in v3"), "once: {out}");

    // kim's comment offers two blocks: Apply asks which; the second turns lines 10-12 into two.
    let (st, out) = r.apply(&r.ada, &kims, json!({}))?;
    assert_eq!((st, text(&out, "error")), (400, "the comment suggests 2 changes: say which (`suggestion`, 0 to 1)"), "{out}");
    let (st, out) = r.apply(&r.ada, &kims, json!({ "suggestion": 1 }))?;
    anyhow::ensure!(st == 200, "apply: {st} {out}");
    assert_eq!((out.get("version"), out.get("suggestion")), (Some(&json!(4)), Some(&json!(1))), "{out}");
    let (author, committer, message) = r.commit("refs/kiln/diffs/D1/v4")?;
    assert_eq!((author.as_str(), committer.as_str()), ("Sam <sam@example.com>", "ada <ada@kiln>"));
    assert!(message.ends_with("Co-authored-by: agent:review <agent-review@kiln>\nCo-authored-by: kim <kim@kiln>"), "each suggester once: {message}");
    let four = matrix(&[(10, &["let r10 = 10;", "let r11_12 = 23;"]), (11, &[]), (12, &[]), (20, &["let r20 = 2 * 10;"]), (42, &[empty])]);
    assert_eq!(r.file("refs/kiln/diffs/D1/v4")?, four);
    let t = r.thread(d1, &kims)?;
    assert_eq!((t.pointer("/comments/0/suggestions/0/applied"), t.pointer("/comments/0/suggestions/1/applied/version")), (Some(&Value::Null), Some(&json!(4))), "{t}");

    // kim's old field applies alike, where line 5 still is.
    let (st, out) = r.apply(&r.ada, &field, json!({}))?;
    anyhow::ensure!(st == 200, "apply: {st} {out}");
    assert_eq!(out.get("version"), Some(&json!(5)), "{out}");
    let five = matrix(&[(5, &["let r5 = five();"]), (10, &["let r10 = 10;", "let r11_12 = 23;"]), (11, &[]), (12, &[]), (20, &["let r20 = 2 * 10;"]), (42, &[empty])]);
    assert_eq!(r.file("refs/kiln/diffs/D1/v5")?, five);
    let v5 = get(&r.s, &r.ada, "/api/diffs/D1")?.pointer("/versions/4").cloned().unwrap_or_default();
    assert_eq!((text(&v5, "why"), text(&v5, "pushed_by").is_empty()), ("suggestion applied", false), "{v5}");
    let t = r.thread(d1, &field)?;
    let s0 = t.pointer("/comments/0/suggestions/0").cloned().unwrap_or_default();
    assert_eq!((s0.get("lines"), s0.get("field"), s0.pointer("/applied/version"), s0.pointer("/applied/by")), (Some(&json!(["let r5 = five();"])), Some(&json!(true)), Some(&json!(5)), Some(&json!(text(&v5, "pushed_by")))), "{t}");
    // The outdated block still says so, and changed nothing.
    assert_eq!(r.thread(d1, &late)?.pointer("/comments/0/suggestions/0/outdated"), Some(&json!(true)));
    Ok(())
}

/// CR1, a change request of sam's branch: each Apply is a commit of ada's on the branch (its author
/// and committer, as on GitHub), the suggester a co-author, made a version; a one-line block of the
/// review agent's and a multi-line old-field suggestion of kim's; and a block whose lines sam changed
/// since, which says it's outdated and doesn't apply.
#[test]
fn w26_ac03_a_change_requests_suggestion_is_a_commit_of_whoever_applies_it() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let r = repo(dir.path())?;
    r.g(&["checkout", "-qb", "topic"])?;
    std::fs::write(r.work.join("matrix.rs"), matrix(&[]))?;
    r.g(&["add", "matrix.rs"])?;
    r.g(&["commit", "-qm", "matrix: rows"])?;
    let out = r.g(&["push", "-o", "title=Matrix rows", "origin", "topic:refs/for/main/branch/topic"])?;
    anyhow::ensure!(out.contains("CR1 v1 (new)"), "{out}");
    let cr1 = "change-requests/CR1";
    let v1 = r.g(&["rev-parse", "HEAD"])?.trim().to_owned();
    let empty = "if m.is_empty() { return Ok(vec![]) }";

    let (agents, agents_thread) = r.comment(&r.agent, cr1, json!({ "file": "matrix.rs", "start_line": 42, "body": format!("```suggestion\n{empty}\n```"), "publish": true }))?;
    let (kims, _) = r.comment(&r.kim, cr1, json!({ "file": "matrix.rs", "start_line": 7, "end_line": 8, "body": "one statement each", "suggestion": "let r7 = 7;\nlet r8 = r7 + 1;\nlet r8b = r8;", "publish": true }))?;
    let (late, _) = r.comment(&r.kim, cr1, json!({ "file": "matrix.rs", "start_line": 30, "body": "```suggestion\nlet r30 = thirty();\n```", "publish": true }))?;

    // The agent's block, applied by ada: a commit of hers on topic, the agent a co-author: CR1 v2.
    let (st, out) = r.apply(&r.ada, &agents, json!({}))?;
    anyhow::ensure!(st == 200, "apply: {st} {out}");
    assert_eq!((out.get("version"), out.get("state"), out.get("signer")), (Some(&json!(2)), Some(&json!("resolved")), Some(&Value::Null)), "{out}");
    let (author, committer, message) = r.commit("topic")?;
    assert_eq!((author.as_str(), committer.as_str()), ("ada <ada@kiln>", "ada <ada@kiln>"));
    assert_eq!(message, "Apply a suggestion from agent:review\n\nOn matrix.rs, line 42.\n\nCo-authored-by: agent:review <agent-review@kiln>");
    assert_eq!(r.g(&["rev-parse", "FETCH_HEAD^"])?.trim(), v1, "on the branch, after sam's commit");
    assert_eq!(text(&out, "commit"), r.g(&["rev-parse", "FETCH_HEAD"])?.trim());
    assert_eq!(r.file("refs/kiln/diffs/CR1/v2")?, matrix(&[(42, &[empty])]));
    let cr = get(&r.s, &r.ada, "/api/change-requests/CR1")?;
    let v2 = cr.pointer("/versions/1").cloned().unwrap_or_default();
    assert_eq!((text(&v2, "why"), text(&v2, "thread"), text(&v2, "note")), ("suggestion applied", agents_thread.as_str(), "applied a suggestion from agent:review"), "{v2}");
    assert_eq!(r.thread(cr1, &agents)?.pointer("/comments/0/suggestions/0/applied/version"), Some(&json!(2)));

    // kim's three lines for two, in the old field: CR1 v3, kim a co-author.
    let (st, out) = r.apply(&r.ada, &kims, json!({}))?;
    anyhow::ensure!(st == 200, "apply: {st} {out}");
    assert_eq!(out.get("version"), Some(&json!(3)), "{out}");
    let (author, _, message) = r.commit("topic")?;
    assert_eq!((author.as_str(), message.as_str()), ("ada <ada@kiln>", "Apply a suggestion from user:kim\n\nOn matrix.rs, lines 7-8.\n\nCo-authored-by: kim <kim@kiln>"));
    let three = matrix(&[(7, &["let r7 = 7;", "let r8 = r7 + 1;", "let r8b = r8;"]), (8, &[]), (42, &[empty])]);
    assert_eq!(r.file("refs/kiln/diffs/CR1/v3")?, three);

    // sam pulls, changes line 30 (31 now) himself and pushes: CR1 v4, and kim's block on it is outdated.
    r.g(&["fetch", "-q", "origin", "topic"])?;
    r.g(&["reset", "-q", "--hard", "FETCH_HEAD"])?;
    let mine = three.replace("let r30 = 30;", "let r30 = 3 * 10;");
    std::fs::write(r.work.join("matrix.rs"), &mine)?;
    r.g(&["commit", "-qam", "matrix: r30"])?;
    let out = r.g(&["push", "origin", "topic"])?;
    anyhow::ensure!(out.contains("CR1 v4"), "{out}");
    let (st, out) = r.apply(&r.ada, &late, json!({}))?;
    assert_eq!((st, text(&out, "error")), (409, "matrix.rs:30: this suggestion is outdated: its lines changed since v1"), "{out}");
    assert_eq!(r.thread(cr1, &late)?.pointer("/comments/0/suggestions/0/outdated"), Some(&json!(true)));
    assert_eq!(r.file("topic")?, mine, "nothing applied");
    Ok(())
}
