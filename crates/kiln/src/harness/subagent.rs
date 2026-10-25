//! Messages a person writes to one of an agent's own subagents (a task of class "agent"): queued
//! for when it finishes, steered into its turn, or sent now, as its `Task::takes` allows. Each
//! adapter delivers them its harness's way: straight in where the harness takes them (Codex's
//! subagents, each a thread of its own), else by way of the agent that started it, which passes
//! them on with its own tool (Claude Code's SendMessage). What's the same for every harness is
//! here: its queue, the message in its thread, and the refusal of one it can't take.

use super::core::Core;
use super::model::{Body, Command, Delivery, Level, QueueItem, SendMode, Task};

/// `Task::via` of a subagent whose messages go by way of the agent that started it.
pub const PARENT: &str = "parent";

/// The ids of the messages kiln writes an agent so it passes one on to its subagent (kiln's own,
/// `pause::KILN_ID`), each followed by the person's message's id.
pub const RELAY_ID: &str = "kiln-relay-";

/// What became of a command about a message waiting in a subagent's queue.
pub enum Queued {
    /// Edited, moved or taken out, there.
    Done,
    /// Out of the queue, to go to subagent `0` now this way.
    Send(String, QueueItem, SendMode),
    /// Not about one: the adapter has it as it is.
    Not(Command),
}

/// A queue command (`queue_edit`, `queue_remove`, `queue_move`, `queue_steer`,
/// `queue_send_now`) for a message one of its subagents' queues holds is handled here: the
/// agent's own queue is its adapter's.
pub fn queue_command(core: &mut Core, c: Command) -> Queued {
    let id = match &c {
        Command::QueueEdit { id, .. } | Command::QueueRemove { id } | Command::QueueMove { id, .. } | Command::QueueSteer { id } | Command::QueueSendNow { id } => id.clone(),
        _ => return Queued::Not(c),
    };
    let Some((task, at)) = core.state.tasks.iter().enumerate().find_map(|(i, t)| t.queue.iter().position(|q| q.id == id).map(|at| (i, at))) else { return Queued::Not(c) };
    let Some(t) = core.state.tasks.get_mut(task) else { return Queued::Not(c) };
    let sent = match c {
        Command::QueueEdit { text, attachments, .. } => {
            if let Some(q) = t.queue.get_mut(at) {
                q.text = text;
                if let Some(a) = attachments {
                    q.attachments = a;
                }
            }
            None
        }
        Command::QueueRemove { .. } => {
            t.queue.remove(at);
            None
        }
        Command::QueueMove { index, .. } => {
            let item = t.queue.remove(at);
            let index = index.min(t.queue.len());
            t.queue.insert(index, item);
            None
        }
        Command::QueueSteer { .. } => Some((t.id.clone(), t.queue.remove(at), SendMode::Steer)),
        _ => Some((t.id.clone(), t.queue.remove(at), SendMode::Now)),
    };
    core.touch_state();
    match sent {
        Some((task, m, mode)) => Queued::Send(task, m, mode),
        None => Queued::Done,
    }
}

/// The mode a message to `t` goes in: the one asked for when it takes that now, else the nearest
/// it does (now, then steer, then queue). None: it takes none.
pub fn mode_for(t: &Task, asked: SendMode) -> Option<SendMode> {
    let order: &[SendMode] = match asked {
        SendMode::Now => &[SendMode::Now, SendMode::Steer, SendMode::Queue],
        SendMode::Steer => &[SendMode::Steer, SendMode::Queue],
        SendMode::Queue => &[SendMode::Queue],
    };
    order.iter().copied().find(|m| t.takes.contains(m))
}

/// The person's message, shown in the subagent's thread (among its spawning call's events),
/// where it stands as `d`.
pub fn show(core: &mut Core, task: &str, m: &QueueItem, d: Delivery) {
    let parent = core.state.tasks.iter().find(|t| t.id == task).and_then(|t| t.tool_id.clone());
    let (images, attachments) = super::model::attachment_summary(&m.attachments);
    core.state.delivery.insert(m.id.clone(), d);
    core.note_sent(&m.id);
    core.upsert(&m.id, parent, Body::User { text: m.text.clone(), images, client_id: Some(m.id.clone()), origin: Some("human".into()), attachments, reply_to: m.reply_to.clone() });
    core.touch_state();
}

/// Where a message shown in a subagent's thread stands now (a later word on it, from its harness).
pub fn delivered(core: &mut Core, id: &str, d: Delivery) {
    if core.state.delivery.contains_key(id) {
        core.state.delivery.insert(id.to_owned(), d);
        core.touch_state();
    }
}

/// It can't go: shown in the subagent's thread as not delivered, and why.
pub fn refuse(core: &mut Core, task: &str, m: &QueueItem, why: &str) {
    show(core, task, m, Delivery::Cancelled);
    let parent = core.state.tasks.iter().find(|t| t.id == task).and_then(|t| t.tool_id.clone());
    core.upsert(&format!("subagent-unsent-{}", m.id), parent, Body::Notice { level: Level::Warn, text: format!("Not sent to the subagent: {why}") });
}

/// It waits in the subagent's queue for it to finish what it's doing. False: no such subagent.
pub fn hold(core: &mut Core, task: &str, m: QueueItem) -> bool {
    let Some(t) = core.state.tasks.iter_mut().find(|t| t.id == task) else { return false };
    t.queue.push(m);
    core.touch_state();
    true
}

/// It goes before what waits for the subagent (a message sent now, its turn being stopped).
pub fn hold_first(core: &mut Core, task: &str, m: QueueItem) {
    if let Some(t) = core.state.tasks.iter_mut().find(|t| t.id == task) {
        t.queue.insert(0, m);
        core.touch_state();
    }
}

/// The next message waiting for subagent `task`, out of its queue.
pub fn next(core: &mut Core, task: &str) -> Option<QueueItem> {
    let t = core.state.tasks.iter_mut().find(|t| t.id == task).filter(|t| !t.queue.is_empty())?;
    let m = t.queue.remove(0);
    core.touch_state();
    Some(m)
}

/// The subagents something waits for, in the order they started.
pub fn waiting(core: &Core) -> Vec<String> {
    core.state.tasks.iter().filter(|t| !t.queue.is_empty()).map(|t| t.id.clone()).collect()
}

/// How a message reaches each subagent now, as its adapter says (`takes` and `via`; what isn't a
/// subagent takes none). What waited for one that takes nothing any more can't go: it's shown in
/// its thread as not delivered.
pub fn settle(core: &mut Core, of: impl Fn(&Core, &Task) -> (Vec<SendMode>, Option<&'static str>)) {
    let now: Vec<(Vec<SendMode>, Option<&'static str>)> = core.state.tasks.iter().map(|t| if t.subagent() { of(core, t) } else { (Vec::new(), None) }).collect();
    let mut stranded = Vec::new();
    let mut changed = false;
    for (t, (takes, via)) in core.state.tasks.iter_mut().zip(now) {
        if t.takes != takes || t.via.as_deref() != via {
            t.takes = takes;
            t.via = via.map(str::to_owned);
            changed = true;
        }
        if t.takes.is_empty() && !t.queue.is_empty() {
            stranded.extend(std::mem::take(&mut t.queue).into_iter().map(|m| (t.id.clone(), m)));
            changed = true;
        }
    }
    for (task, m) in stranded {
        refuse(core, &task, &m, "it ended before it could take it.");
    }
    if changed {
        core.touch_state();
    }
}

/// What kiln asks an agent to pass on to its subagent `t` (whose harness takes nothing from kiln
/// itself), `how` naming its tool: the person's words as they are, with the files they attached
/// by their paths.
pub fn relay_words(t: &Task, m: &QueueItem, how: &str) -> String {
    let name = t.description.trim();
    let name = if name.is_empty() { t.id.as_str() } else { name };
    let mut words = m.text.trim_end().to_owned();
    let files: Vec<String> = m.attachments.iter().filter_map(|a| a.path.as_ref().map(|p| format!("- {} ({}): {p}", a.name, a.mime))).collect();
    if !files.is_empty() {
        words.push_str(&format!("\n\nAttached files:\n{}", files.join("\n")));
    }
    format!(
        "The person wrote a message to your subagent “{name}” (agent id {}), which you started, through kiln, which can't reach it directly. {how} Pass it on exactly as written and don't act on it yourself; if it can't be sent, say why.\n\n<message>\n{words}\n</message>",
        t.id
    )
}

/// The message kiln writes the agent to pass the person's message `m` on (its id: `RELAY_ID`
/// then `m`'s).
pub fn relay_id(m: &QueueItem) -> String {
    format!("{RELAY_ID}{}", m.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sub(id: &str, takes: Vec<SendMode>) -> Task {
        Task { id: id.into(), tool_id: Some(format!("call-{id}")), description: "Read the parser".into(), class: Some("agent".into()), status: "running".into(), takes, ..Task::default() }
    }

    fn msg(id: &str, text: &str) -> QueueItem {
        QueueItem { id: id.into(), text: text.into(), at: 1, attachments: Vec::new(), overrides: None, reply_to: None }
    }

    #[test]
    fn a_subagents_queue_is_edited_moved_and_sent_from_where_it_waits() -> anyhow::Result<()> {
        let mut core = Core::new();
        core.state.tasks.push(sub("a1", vec![SendMode::Queue, SendMode::Steer]));
        assert!(hold(&mut core, "a1", msg("m1", "one")));
        assert!(hold(&mut core, "a1", msg("m2", "two")));
        assert!(matches!(queue_command(&mut core, Command::QueueEdit { id: "m1".into(), text: "first".into(), attachments: None }), Queued::Done));
        assert!(matches!(queue_command(&mut core, Command::QueueMove { id: "m2".into(), index: 0 }), Queued::Done));
        let order: Vec<&str> = core.state.tasks.iter().flat_map(|t| t.queue.iter().map(|q| q.text.as_str())).collect();
        assert_eq!(order, ["two", "first"]);
        let Queued::Send(task, m, mode) = queue_command(&mut core, Command::QueueSteer { id: "m1".into() }) else { anyhow::bail!("not sent") };
        assert_eq!((task.as_str(), m.text.as_str(), mode), ("a1", "first", SendMode::Steer));
        // One in the agent's own queue is its adapter's.
        assert!(matches!(queue_command(&mut core, Command::QueueRemove { id: "elsewhere".into() }), Queued::Not(_)));
        assert!(matches!(queue_command(&mut core, Command::QueueRemove { id: "m2".into() }), Queued::Done));
        assert!(core.state.tasks.iter().all(|t| t.queue.is_empty()));
        Ok(())
    }

    #[test]
    fn what_waited_for_a_subagent_that_ended_is_said_not_sent() {
        let mut core = Core::new();
        core.state.tasks.push(sub("a1", vec![SendMode::Queue]));
        hold(&mut core, "a1", msg("m1", "later"));
        settle(&mut core, |_, _| (Vec::new(), None));
        let t = core.state.tasks.first().cloned().unwrap_or_default();
        assert!(t.takes.is_empty() && t.queue.is_empty());
        assert_eq!(core.state.delivery.get("m1"), Some(&Delivery::Cancelled));
        assert!(core.get("m1").is_some_and(|e| e.parent.as_deref() == Some("call-a1")));
        assert!(core.get("subagent-unsent-m1").is_some_and(|e| matches!(&e.body, Body::Notice { text, .. } if text.contains("ended"))));
    }

    #[test]
    fn a_mode_a_subagent_doesnt_take_comes_down_to_one_it_does() {
        let t = sub("a1", vec![SendMode::Queue, SendMode::Steer]);
        assert_eq!(mode_for(&t, SendMode::Now), Some(SendMode::Steer));
        assert_eq!(mode_for(&t, SendMode::Queue), Some(SendMode::Queue));
        assert_eq!(mode_for(&sub("a2", Vec::new()), SendMode::Steer), None);
    }

    #[test]
    fn a_relay_carries_the_words_as_written_and_names_the_subagent() {
        let t = sub("a1", Vec::new());
        let mut m = msg("m1", "use the v2 parser\n");
        m.attachments.push(super::super::model::Attachment { name: "log.txt".into(), mime: "text/plain".into(), data_base64: String::new(), file_id: None, path: Some("/tmp/log.txt".into()) });
        let w = relay_words(&t, &m, "Send it with SendMessage (to: \"a1\").");
        assert!(w.contains("“Read the parser” (agent id a1)") && w.contains("SendMessage (to: \"a1\")"));
        assert!(w.contains("<message>\nuse the v2 parser\n\nAttached files:\n- log.txt (text/plain): /tmp/log.txt\n</message>"));
        assert_eq!(relay_id(&m), "kiln-relay-m1");
    }
}
