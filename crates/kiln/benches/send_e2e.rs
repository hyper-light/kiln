//! bench send_e2e (CP-T3.2): a send end to end on a machine's host of agent sessions, as a person's
//! message goes: from `Host::command` until the session has handed it to its harness (its user
//! event; the message is a steer, so it goes at once). It uses only what kiln's main had before
//! sessions had inboxes, so the same file measures a branch and main alike: with inboxes, the
//! inbox's synced commit is in the time; on main, nothing is made durable before the harness gets
//! the message. The harness is a program that takes what it's sent and says nothing. Not here: the
//! round trip between the server and the session's runner, which a send over the network adds
//! (and waits on for the inbox's answer) whatever the host does. Recorded with the machine, the
//! drive and the load, refused on a loaded machine (as send_path is).
//!
//! ```text
//! cargo bench -p kiln --bench send_e2e -- --dir <a folder on the disk to measure>
//!     [--runs 3] [--messages 500] [--sessions 1] [--label <what this build is>] [--commit <sha>]
//!     [--out <file.json>] [--wait-quiet <seconds>] [--any-load]
//! ```

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kiln::harness::core::Update;
use kiln::harness::host::{Host, Limits};
use kiln::harness::model::{Body, Command, SendMode};
use kiln::harness::session::Spec;
use serde_json::{Value, json};

#[allow(dead_code)]
mod common;
use common::{QUIET, disk, load, machine, now, summary};

struct Args {
    dir: PathBuf,
    runs: usize,
    messages: usize,
    sessions: usize,
    label: String,
    commit: String,
    out: Option<PathBuf>,
    wait_quiet: u64,
    any_load: bool,
}

fn args() -> anyhow::Result<Args> {
    let mut a = Args { dir: std::env::temp_dir(), runs: 3, messages: 500, sessions: 1, label: String::new(), commit: String::new(), out: None, wait_quiet: 0, any_load: false };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let mut v = || it.next().ok_or_else(|| anyhow::anyhow!("{k} takes a value"));
        match k.as_str() {
            "--dir" => a.dir = PathBuf::from(v()?),
            "--runs" => a.runs = v()?.parse()?,
            "--messages" => a.messages = v()?.parse()?,
            "--sessions" => a.sessions = v()?.parse()?,
            "--label" => a.label = v()?,
            "--commit" => a.commit = v()?,
            "--out" => a.out = Some(PathBuf::from(v()?)),
            "--wait-quiet" => a.wait_quiet = v()?.parse()?,
            "--any-load" => a.any_load = true,
            "--bench" => {}
            other => anyhow::bail!("unknown argument {other}"),
        }
    }
    anyhow::ensure!(a.runs > 0 && a.messages > 0 && a.sessions > 0, "--runs, --messages and --sessions are at least 1");
    Ok(a)
}


/// Whether `u` is session's user event for message `id`.
fn handed(u: &Update, id: &str) -> bool {
    match u {
        Update::Events { events } => events.iter().any(|e| matches!(&e.body, Body::User { client_id: Some(c), .. } if c == id)),
        _ => false,
    }
}

/// One run: a host whose record (and, where it has them, its sessions' inboxes) is in a fresh
/// folder of `dir`, `sessions` sessions running the quiet harness, and `messages` sends, each
/// timed until its session handed it on.
async fn run(dir: &Path, messages: usize, sessions: usize, n: usize) -> anyhow::Result<Value> {
    let scratch = tempfile::Builder::new().prefix("send-e2e-").tempdir_in(dir)?;
    let program = scratch.path().join("quiet-claude");
    std::fs::write(&program, "#!/bin/sh\nexec cat > /dev/null\n")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))?;
    }
    let (tx, rx) = async_channel::unbounded();
    let host = Host::new(tx, Some(scratch.path().join("harness.json")), Limits { per_machine: sessions.saturating_add(1), per_shell: sessions.saturating_add(1) });
    let ids: Vec<String> = (0..sessions).map(|s| format!("e2e-{n}-{s}")).collect();
    for id in &ids {
        let spec = Spec { kind: "claude".into(), cwd: Some(scratch.path().to_string_lossy().into_owned()), program: Some(program.to_string_lossy().into_owned()), ..Spec::default() };
        host.open(Some(id.clone()), spec)?;
    }
    // Each session's harness running and its adapter started (it said what it takes).
    let mut ready = std::collections::HashSet::new();
    let started = Instant::now();
    while ready.len() < sessions {
        anyhow::ensure!(started.elapsed() < Duration::from_secs(60), "the sessions never got going");
        if let Ok(Ok((id, Update::Meta { meta }))) = kiln::rt::timeout(Duration::from_secs(1), rx.recv()).await
            && !meta.features.is_empty()
        {
            ready.insert(id);
        }
    }
    let mut took = Vec::with_capacity(messages);
    for m in 0..messages {
        let session = ids.get(m % sessions).cloned().unwrap_or_default();
        let id = format!("r{n}-m{m}-{}", kiln::util::new_id());
        let t = Instant::now();
        anyhow::ensure!(host.command(&session, Command::Send { id: id.clone(), text: "the next step, please".into(), mode: SendMode::Steer, attachments: Vec::new(), overrides: None, reply_to: None, task: None }), "{session} took nothing");
        loop {
            let (_, u) = kiln::rt::timeout(Duration::from_secs(30), rx.recv()).await.map_err(|_| anyhow::anyhow!("{id} wasn't handed on in 30 s"))??;
            if handed(&u, &id) {
                break;
            }
        }
        took.push(t.elapsed());
    }
    for id in &ids {
        host.close(id);
    }
    Ok(summary(took))
}

fn main() -> anyhow::Result<()> {
    let a = args()?;
    // The load discipline: a loaded machine's numbers aren't recorded.
    let (before, loaded) = common::quiet("send_e2e", a.wait_quiet, a.any_load)?;
    let dir = a.dir.clone();
    let (messages, sessions, runs) = (a.messages, a.sessions, a.runs);
    let results: Vec<Value> = kiln::rt::block_on(async move {
        let mut out = Vec::with_capacity(runs);
        for n in 0..runs {
            out.push(run(&dir, messages, sessions, n).await?);
        }
        anyhow::Ok(out)
    })?;
    let best = results
        .iter()
        .min_by(|x, y| {
            let p = |v: &Value| v.get("p99_ms").and_then(Value::as_f64).unwrap_or(f64::MAX);
            p(x).total_cmp(&p(y))
        })
        .cloned()
        .unwrap_or(Value::Null);
    let record = json!({
        "bench": "send_e2e",
        "label": a.label,
        "commit": a.commit,
        "build": if cfg!(debug_assertions) { "debug" } else { "release" },
        "date": now(),
        "machine": machine(),
        "disk": disk(&a.dir),
        "load": {"before": before, "after": load(), "quiet_below": QUIET, "loaded": loaded},
        "messages": messages,
        "sessions": sessions,
        "runs": runs,
        "best": best,
        "every_run": results,
    });
    let text = serde_json::to_string_pretty(&record)?;
    if let Some(out) = &a.out {
        std::fs::write(out, &text)?;
    }
    println!("{text}");
    Ok(())
}
