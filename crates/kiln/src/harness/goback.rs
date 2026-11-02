//! The go-back commands — rewind a prompt and all after it, restore the workspace to a prompt's
//! point, branch the conversation from a prompt, fork it whole — done by kiln where a harness
//! has none of its own. The conversation store stands in for the harness's history: the
//! timeline truncates (`Core::truncate_from`), its transcript seeds a branch's first message,
//! and the workspace goes back to the checkpoint (`refs/kiln/checkpoints/<session>/<at>`,
//! `pause::snapshot`) the conversation's start and each prompt made.

use serde_json::Value;

use super::core::{Core, Effect, GoBackOp};
use super::model::{Body, Control, ControlKind, ControlScope, Level};

/// The go-back actions kiln stands in for: (id, label, hint, dangerous, turn scope).
const OPS: &[(&str, &str, &str, bool, bool)] = &[
    ("rewind_files", "Restore files to here", "Undo the file changes made since this prompt; keep the conversation", true, true),
    ("rewind", "Rewind conversation to here", "Undo this prompt and everything after it: the conversation and the file changes", true, true),
    ("branch", "Branch from here", "A new session with the conversation up to the end of this turn", false, true),
    ("fork", "Fork the conversation", "A new session with the whole conversation", false, false),
];

/// The go-back controls appended to a harness's own where it doesn't declare them (a prompt's
/// ⋯ menu offers what `Meta::controls` lists).
pub fn declare(core: &mut Core) {
    for (id, label, hint, danger, turn) in OPS {
        if core.meta.controls.iter().any(|c| c.id == *id) {
            continue;
        }
        core.meta.controls.push(Control {
            id: (*id).to_owned(),
            group: "Session".into(),
            label: (*label).to_owned(),
            description: (*hint).to_owned(),
            kind: ControlKind::Action,
            options: Vec::new(),
            value: None,
            dangerous: *danger,
            per_turn: false,
            requires: None,
            scope: if *turn { ControlScope::Turn } else { ControlScope::Session },
            input: None,
        });
    }
}

/// Run a go-back control where the harness has none of it (asks the session for the effect).
/// `false` when `id` isn't one of them: the caller's own handling then applies.
pub fn control(core: &mut Core, id: &str, value: Option<&Value>) -> bool {
    let op = match id {
        "rewind" => GoBackOp::Rewind,
        "rewind_files" => GoBackOp::RewindFiles,
        "branch" => GoBackOp::Branch,
        "fork" => GoBackOp::Fork,
        _ => return false,
    };
    let event = value.and_then(|v| v.get("event")).and_then(Value::as_str).unwrap_or_default().to_owned();
    if op != GoBackOp::Fork {
        match core.get(&event) {
            Some(e) if matches!(e.body, Body::User { .. }) => {}
            _ => {
                notice(core, Level::Warn, "Only one of your own messages can be gone back to.");
                return true;
            }
        }
    }
    core.effects.push(Effect::GoBack { op, event });
    true
}

/// A worded notice on the timeline.
pub fn notice(core: &mut Core, level: Level, text: impl Into<String>) {
    core.upsert(&format!("goback-{}", Core::now()), None, Body::Notice { level, text: text.into() });
}

/// The conversation in words, to seed a branch or a restarted session with its history: each
/// prompt and answer in order. `upto` (a prompt's event) bounds it after that prompt's whole
/// turn — everything up to the next prompt goes in.
pub fn seed_prompt(core: &Core, upto: Option<&str>) -> String {
    let boundary = upto.and_then(|id| core.get(id)).map_or(u64::MAX, |e| {
        core.events()
            .filter(|o| o.seq > e.seq)
            .find(|o| matches!(o.body, Body::User { .. }))
            .map_or(u64::MAX, |o| o.seq)
    });
    let mut out = String::new();
    for e in core.events().filter(|e| e.seq < boundary && e.parent.is_none()) {
        match &e.body {
            Body::User { text, origin, .. } if origin.as_deref() != Some("system") => out.push_str(&format!("You: {}\n\n", text.trim())),
            Body::Assistant { text, .. } if !text.trim().is_empty() => out.push_str(&format!("Agent: {}\n\n", text.trim())),
            _ => {}
        }
    }
    let out = out.trim().to_owned();
    if out.is_empty() {
        "The conversation it came from is empty.".to_owned()
    } else {
        super::model::clip_middle(&out, 32 * 1024).0
    }
}

/// What a branch (or a fork) is told first so it owns the conversation it came from.
pub fn branch_prompt(core: &Core, upto: Option<&str>) -> String {
    format!(
        "You are continuing a conversation that branched off another session of yours. What it was before the branch:\n\n{}\n\nPick up where it left off.",
        seed_prompt(core, upto)
    )
}

/// What one of its harness's own subagents (its thread under its spawning call `spawn`) was asked
/// (`asked`, its brief when its thread lacks one) and did, in words, for a variant of it
/// ([`branch_spec`]).
pub fn thread_words(core: &Core, spawn: &str, asked: Option<&str>) -> String {
    let mut out = String::new();
    let thread: Vec<&super::model::Event> = core.events().filter(|e| e.parent.as_deref() == Some(spawn)).collect();
    if let Some(a) = asked.map(str::trim).filter(|a| !a.is_empty())
        && !thread.iter().any(|e| matches!(&e.body, Body::User { .. }))
    {
        out.push_str(&format!("Its agent: {a}\n\n"));
    }
    for e in thread {
        match &e.body {
            Body::User { text, origin, .. } if origin.as_deref() == Some("human") => out.push_str(&format!("You: {}\n\n", text.trim())),
            Body::User { text, .. } => out.push_str(&format!("Its agent: {}\n\n", text.trim())),
            Body::Assistant { text, .. } if !text.trim().is_empty() => out.push_str(&format!("It: {}\n\n", text.trim())),
            _ => {}
        }
    }
    let out = out.trim();
    if out.is_empty() {
        "It had nothing to show yet.".to_owned()
    } else {
        super::model::clip_middle(out, 32 * 1024).0
    }
}

/// What a branch a person asks for (`Command::Branch`) is made from: session `id`, its own spec
/// as it resumes (`own`), whether its harness forks a resumed conversation (`can_fork`), its
/// conversation in words for when it can't ([`branch_prompt`]), and, for one of its harness's own
/// subagents, that one's task and its thread in words ([`thread_words`]).
pub struct BranchFrom<'a> {
    pub id: &'a str,
    pub own: super::session::Spec,
    pub can_fork: bool,
    pub conversation: String,
    pub task: Option<(&'a str, String)>,
}

/// The new session a person's branch starts (`Command::Branch`), named by their `words`. Of the
/// session itself: from its own spec, so beside it under the same agent, its conversation forked
/// where its harness can (their words its first message) and said in words where it can't. Of one
/// of its harness's own subagents: that one's thread in words, then theirs, a subagent of the
/// session. `top`: an agent of its own (no parent, not a subagent).
pub fn branch_spec(from: BranchFrom<'_>, words: &str, top: bool) -> super::session::Spec {
    let words = words.trim();
    let mut b = from.own;
    match from.task {
        None => {
            if from.can_fork && b.resume.is_some() {
                b.fork = true;
                b.prompt = Some(words.to_owned());
            } else {
                b.resume = None;
                b.fork = false;
                b.prompt = Some(format!("{}\n\n{words}", from.conversation));
            }
            b.variant_of = Some(from.id.to_owned());
        }
        Some((task, thread)) => {
            b.resume = None;
            b.fork = false;
            b.prompt = Some(format!("You are a variant of another agent's subagent. What it was asked, and did, so far:\n\n{thread}\n\nGo on from there, with this change: {words}"));
            b.parent = Some(from.id.to_owned());
            b.subagent = true;
            b.variant_of = Some(format!("{}/{task}", from.id));
        }
    }
    if top {
        b.parent = None;
        b.subagent = false;
    }
    b.resume_at = None;
    b.name = Some(super::model::clip_middle(words, 80).0);
    // Its own, not what it was branched from's: no plan step, card, or process to take over.
    b.plan_run = None;
    b.helper = None;
    b.takeover_pid = None;
    b.takeover_stop = false;
    b
}

/// Put `cwd`'s workspace back to its state at `at`: the newest checkpoint at or before it (each
/// prompt and pause made one — `pause::snapshot`), taken into the folder's index and worktree;
/// what the tree doesn't know is cleaned away. Human words on what happened.
pub fn restore_files(cwd: &str, session: &str, at: i64) -> String {
    match restore_snapshot(cwd, session, at) {
        Some(snap_at) => format!("the workspace was restored to the checkpoint from {}s ago", (Core::now() - snap_at).max(0)),
        None => "no checkpoint reaches back that far — the files stay as they are".to_owned(),
    }
}

/// The workspace as the checkpoint (a hidden-ref commit, `pause::snapshot`) remembered it:
/// index and worktree take its tree, and what the tree doesn't know is cleaned away.
fn restore_snapshot(cwd: &str, session: &str, at: i64) -> Option<i64> {
    use std::process::{Command as Proc, Stdio};
    let git = |args: &[&str]| -> Option<String> {
        let o = Proc::new("git").arg("-C").arg(cwd).args(args).stdin(Stdio::null()).output().ok()?;
        o.status.success().then(|| String::from_utf8_lossy(&o.stdout).trim().to_owned())
    };
    let prefix = format!("refs/kiln/checkpoints/{session}/");
    let (snap_at, git_ref) = refs_of(git, &prefix)
        .lines()
        .filter_map(|r| r.strip_prefix(&prefix).and_then(|a| a.parse::<i64>().ok()).map(|a| (a, r.to_owned())))
        .filter(|(a, _)| *a <= at)
        .max_by_key(|(a, _)| *a)?;
    git(&["read-tree", "-m", "-u", &git_ref])?;
    git(&["clean", "-fd"])?;
    Some(snap_at)
}

fn refs_of(git: impl Fn(&[&str]) -> Option<String>, prefix: &str) -> String {
    git(&["for-each-ref", "--format=%(refname)", prefix]).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt(core: &mut Core, id: &str, text: &str) {
        core.upsert(id, None, Body::User { text: text.into(), images: 0, client_id: Some(id.into()), origin: Some("human".into()), attachments: Vec::new(), reply_to: None });
    }

    fn said(core: &mut Core, id: &str, text: &str) {
        core.upsert(id, None, Body::Assistant { text: text.into(), streaming: false });
    }

    /// A person's branch: of a subagent kiln started, beside it with its conversation forked where
    /// its harness can and said where it can't; of one of its harness's own, its thread said, a
    /// subagent of the session; New agent, of its own. Each says what it was branched from.
    #[test]
    fn a_branch_is_a_variant_beside_it_or_an_agent_of_its_own() {
        use super::super::session::Spec;
        let own = Spec { kind: "claude".into(), resume: Some("cs-1".into()), parent: Some("lead".into()), subagent: true, plan_run: Some("run".into()), name: Some("Leader election".into()), ..Spec::default() };
        let from = |can_fork: bool, task: Option<(&'static str, String)>| BranchFrom { id: "s1", own: own.clone(), can_fork, conversation: "What it was: elections.".into(), task };

        let v = branch_spec(from(true, None), " Batch entries 64 at a time ", false);
        assert!(v.fork && v.resume.as_deref() == Some("cs-1"), "forked from its own conversation");
        assert_eq!(v.prompt.as_deref(), Some("Batch entries 64 at a time"));
        assert_eq!((v.parent.as_deref(), v.subagent, v.variant_of.as_deref()), (Some("lead"), true, Some("s1")), "beside it, under the same agent");
        assert_eq!((v.name.as_deref(), v.plan_run.as_deref()), (Some("Batch entries 64 at a time"), None), "named by the words; no step of a plan of its own");

        let said = branch_spec(from(false, None), "Batch entries", false);
        assert!(!said.fork && said.resume.is_none());
        assert_eq!(said.prompt.as_deref(), Some("What it was: elections.\n\nBatch entries"));

        let sub = branch_spec(from(true, Some(("t9", "It: measured two terms.".into()))), "Try five nodes", false);
        assert!(!sub.fork && sub.resume.is_none());
        assert!(sub.prompt.as_deref().is_some_and(|p| p.contains("It: measured two terms.") && p.ends_with("Go on from there, with this change: Try five nodes")));
        assert_eq!((sub.parent.as_deref(), sub.subagent, sub.variant_of.as_deref()), (Some("s1"), true, Some("s1/t9")), "a subagent of the session it's in");

        let top = branch_spec(from(true, None), "Its own way", true);
        assert_eq!((top.parent, top.subagent, top.variant_of.as_deref()), (None, false, Some("s1")), "an agent of its own");
    }

    /// A harness's own subagent's thread, in words: what its agent wrote it, what the person did, and what it said.
    #[test]
    fn a_subagents_thread_in_words() {
        let mut core = Core::new();
        core.upsert("brief", Some("spawn".into()), Body::User { text: "Time ten elections.".into(), images: 0, client_id: None, origin: Some("system".into()), attachments: Vec::new(), reply_to: None });
        core.upsert("m1", Some("spawn".into()), Body::User { text: "Use five nodes".into(), images: 0, client_id: Some("m1".into()), origin: Some("human".into()), attachments: Vec::new(), reply_to: None });
        core.upsert("a1", Some("spawn".into()), Body::Assistant { text: "Two terms each.".into(), streaming: false });
        prompt(&mut core, "main", "not in the thread");
        let w = thread_words(&core, "spawn", Some("Time ten elections."));
        assert_eq!(w, "Its agent: Time ten elections.\n\nYou: Use five nodes\n\nIt: Two terms each.");
        // With nothing in its thread, its brief.
        assert_eq!(thread_words(&core, "other", Some("Audit")), "Its agent: Audit");
        assert_eq!(thread_words(&core, "other", None), "It had nothing to show yet.");
    }

    /// The seeding transcript takes a prompt's whole turn when bounded at it.
    #[test]
    fn a_branch_seeds_the_conversation_up_to_the_end_of_the_turn() {
        let mut core = Core::new();
        prompt(&mut core, "u1", "first thing");
        said(&mut core, "a1", "first answer");
        prompt(&mut core, "u2", "second thing");
        said(&mut core, "a2", "second answer");
        let whole = seed_prompt(&core, None);
        assert!(whole.contains("first thing") && whole.contains("second answer"));
        let at_u1 = seed_prompt(&core, Some("u1"));
        assert!(at_u1.contains("first thing") && at_u1.contains("first answer"));
        assert!(!at_u1.contains("second thing"), "{at_u1}");
    }

    /// A go-back control on a prompt queues the session's effect; on anything else it warns.
    #[test]
    fn a_go_back_control_queues_the_effect_or_warns() {
        let mut core = Core::new();
        prompt(&mut core, "u1", "do it");
        assert!(control(&mut core, "rewind", Some(&serde_json::json!({"event": "u1"}))));
        assert!(core.effects.iter().any(|e| matches!(e, Effect::GoBack { op: GoBackOp::Rewind, event } if event == "u1")));
        assert!(control(&mut core, "branch", Some(&serde_json::json!({"event": "u1"}))));
        assert!(control(&mut core, "fork", None));
        assert!(!control(&mut core, "export", None));
        let n = core.events().filter(|e| matches!(e.body, Body::Notice { level: Level::Warn, .. })).count();
        assert_eq!(n, 0);
        assert!(control(&mut core, "rewind", Some(&serde_json::json!({"event": "nope"}))));
        let n = core.events().filter(|e| matches!(e.body, Body::Notice { level: Level::Warn, .. })).count();
        assert_eq!(n, 1);
    }
}
