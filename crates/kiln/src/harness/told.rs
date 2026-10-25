//! A parent hears from its children: when an agent another agent started settles (its turn over,
//! a person needed, stopped, ended, paused at a usage limit), kiln tells its parent, as Claude
//! Code tells an agent of its own background subagents: a message after the parent's turn, which
//! starts one if the parent is idle. So a parent needn't wait for its children: it can start
//! them, end its turn, and take up what each concluded when it's told. `agents_wait` is for the
//! parent that would rather wait; what it saw that way (or through `agents_result`) isn't told
//! again. Children that settle together are told of in one message.
//!
//! The news waits for the parent in that one message (`super::queued`): a child that settles
//! again while it waits changes its line in it, one that settles anew joins it, and what the parent
//! sees itself comes out of it (the message too, once nothing is left). So a parent that spends
//! hours in one turn, its children settling again and again, finds each child's latest line when
//! the turn ends, once, rather than every settle since. A server that started since finds the news
//! a server before it left waiting (its id says what it is) and adds to that one.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::LazyLock;

use parking_lot::Mutex;

use super::model::{Command, SendMode, Status};
use super::queued::{Kept, NEWS, Now};
use super::session::Info;
use super::wait::{settled, stand};

/// How long a child has stood settled before its parent is told: a wait under way for it returns
/// first, and one only between two steps isn't reported.
const SETTLE_MS: i64 = 3_000;
/// The most of a child's last words a message carries (`agents_result` has them whole).
const SAID_MAX: usize = 400;
/// The most children a parent's news names, those that settled last: past them it says how many
/// more did.
const LINES_MAX: usize = 64;

/// What was last seen of a child.
#[derive(Debug, Clone, Default)]
struct Seen {
    /// Its stand and the end of its last turn.
    key: String,
    /// Since when it has stood so.
    since: i64,
    /// Seen at work since its parent was last told: only a settle after work is told of.
    worked: bool,
    /// The settle its parent was told of, or saw itself.
    told: String,
}

/// A parent's news, waiting for it: each child's latest line, by child, the latest last.
#[derive(Debug, Default)]
struct News {
    /// The message that holds it, while it may still wait in the parent's queue (none: not sent
    /// yet, the parent's machine away).
    msg: Option<Kept>,
    /// What that message said when this server found it waiting, queued before it started: said
    /// first, as it was.
    before: Option<String>,
    lines: Vec<(String, String)>,
    /// Children whose lines went to make room (`LINES_MAX`).
    more: usize,
    /// The lines the message says, as kiln last had it say them (as `Kept::text`).
    said: Vec<(String, String)>,
    /// The lines that reach the parent should the message go now (as `Kept::held`).
    held: Vec<(String, String)>,
}

impl News {
    fn text(&self) -> String {
        text_of(self.before.as_deref(), &self.lines, self.more)
    }
}

/// What kiln knows of children and of their parents' news: the server's is [`TOLD`].
#[derive(Debug, Default)]
struct Told {
    /// Each child, by id.
    seen: HashMap<String, Seen>,
    /// Each parent's news, by parent.
    news: HashMap<String, News>,
}

static TOLD: LazyLock<Mutex<Told>> = LazyLock::new(Default::default);
/// One sender of news at a time (`tick`, `saw`): what each decides goes to the parent's machine
/// before the next decides, so two edits of one message can't cross on their way.
static SENDING: async_lock::Mutex<()> = async_lock::Mutex::new(());

fn key_of(info: &Info) -> String {
    format!("{}:{}", stand(info), info.last_turn.as_ref().map_or(0, |t| t.at))
}

/// Its parent saw `child` as it stands now (`agents_wait`, `agents_result`): it isn't told of it
/// again, and its line comes out of the parent's news waiting in its queue (the message with it,
/// once no line is left).
pub async fn saw(hub: &crate::hub::Hub, child: &Info) {
    let _one = SENDING.lock().await;
    let taken = TOLD.lock().saw(child);
    if let Some((parent, c)) = taken {
        let _ = hub.harness.command(hub, &parent, c).await;
    }
}

/// Tells parents of their children that settled: in the one message of news waiting for each
/// (`Told::plan`), queued after the parent's turn. `driven`: the agents a goal stack drives now;
/// `quiet`: those paused for usage (docs/programs.md), which get no new message, since one would
/// start their turn: their news waits, and goes once they go on.
pub async fn tick(hub: &crate::hub::Hub, all: &[Info], now: i64, driven: &[String], quiet: &[String]) {
    let _one = SENDING.lock().await;
    // News a server before this one queued, waiting still: this one adds to it.
    for p in all.iter().filter(|p| !matches!(p.status, Status::Exited | Status::Disconnected) && p.queue.iter().any(|q| super::queued::from_before(&q.id, NEWS))) {
        if TOLD.lock().news.contains_key(&p.id) {
            continue;
        }
        if let Some(q) = hub.harness.queue(&p.id).into_iter().find(|q| super::queued::from_before(&q.id, NEWS)) {
            TOLD.lock().found(&p.id, q.id, q.text, now);
        }
    }
    let plan = TOLD.lock().plan(all, now, driven, quiet);
    for (parent, c) in plan {
        if matches!(c, Command::Send { .. })
            && let Some(p) = all.iter().find(|i| i.id == parent)
        {
            super::wait::sent(&p.id, p.last_turn.as_ref().map(|t| t.at));
        }
        if !matches!(hub.harness.command(hub, &parent, c.clone()).await, super::registry::Sent::Ok) {
            tracing::info!(agent = %parent, "news of the agents it started didn't reach it");
            TOLD.lock().unsent(&parent, &c);
        }
    }
}

impl Told {
    /// News message `id` was found waiting for `parent` at `now`, saying `text`, queued before this
    /// server started: kept as its news, said first.
    fn found(&mut self, parent: &str, id: String, text: String, now: i64) {
        self.news.entry(parent.to_owned()).or_insert_with(|| News { msg: Some(Kept::found(id, text.clone(), now)), before: Some(text), ..News::default() });
    }

    /// [`saw`]: what goes to the parent for it, the line taken out (an edit, or the message removed).
    fn saw(&mut self, child: &Info) -> Option<(String, Command)> {
        self.seen.entry(child.id.clone()).or_default().told = key_of(child);
        let parent = child.parent.clone()?;
        let n = self.news.get_mut(&parent)?;
        let had = n.lines.len();
        n.lines.retain(|(c, _)| *c != child.id);
        if n.lines.len() == had {
            return None;
        }
        let (empty, text) = (n.lines.is_empty() && n.before.is_none(), n.text());
        let c = match n.msg.as_mut() {
            Some(m) if empty => Some(Command::QueueRemove { id: m.id.clone() }),
            Some(m) => {
                m.text = text;
                n.said.clone_from(&n.lines);
                Some(Command::QueueEdit { id: m.id.clone(), text: m.text.clone(), attachments: None })
            }
            None => None,
        };
        if empty {
            self.news.remove(&parent);
        }
        c.map(|c| (parent, c))
    }

    /// Each child that settled since it was last looked at, `SETTLE_MS` on: its line, by parent.
    /// Children first seen settled (the server just started) aren't told of, nor ones a goal stack
    /// drives (`driven`: they settle between goals; their parent is told once no stack drives them).
    fn due(&mut self, all: &[Info], now: i64, driven: &[String]) -> HashMap<String, Vec<(String, String)>> {
        let mut by_parent: HashMap<String, Vec<(String, String)>> = HashMap::new();
        for c in all {
            let Some(parent) = c.parent.as_deref() else { continue };
            let (st, key) = (stand(c), key_of(c));
            let e = self.seen.entry(c.id.clone()).or_insert_with(|| Seen { key: key.clone(), since: now, worked: false, told: key.clone() });
            if e.key != key {
                e.key.clone_from(&key);
                e.since = now;
            }
            if st == "working" {
                e.worked = true;
                continue;
            }
            if driven.contains(&c.id) {
                continue;
            }
            if !settled(st) || !e.worked || e.told == key || now - e.since < SETTLE_MS {
                continue;
            }
            e.told = key;
            e.worked = false;
            by_parent.entry(parent.to_owned()).or_default().push((c.id.clone(), line(c)));
        }
        let live: HashSet<&str> = all.iter().map(|i| i.id.as_str()).collect();
        self.seen.retain(|id, _| live.contains(id.as_str()));
        by_parent
    }

    /// What goes to each parent now, at `now`, for its children that settled (`due`): its news
    /// waiting in its queue, edited to hold each child's latest line; or, none waiting, a message
    /// with them. What a message that went may not have said (an edit that came too late) goes in
    /// the next. An agent that has ended isn't sent anything; one whose machine is away, once it's
    /// back; one paused for usage (`quiet`), no new message (it would start its turn), its news
    /// waiting here until it goes on.
    fn plan(&mut self, all: &[Info], now: i64, driven: &[String], quiet: &[String]) -> Vec<(String, Command)> {
        let mut due = self.due(all, now, driven);
        self.news.retain(|p, _| all.iter().any(|i| i.id == *p));
        let parents: BTreeSet<String> = self.news.keys().chain(due.keys()).cloned().collect();
        let mut out = Vec::new();
        for parent in parents {
            let Some(info) = all.iter().find(|i| i.id == parent) else { continue };
            let n = self.news.entry(parent.clone()).or_default();
            if let Some(m) = n.msg.as_mut() {
                let at = m.look(Some(info), now);
                if !m.lost() {
                    n.held.clone_from(&n.said);
                }
                if at == Now::Gone {
                    n.lines.retain(|l| !n.held.contains(l));
                    n.msg = None;
                    n.before = None;
                    n.said.clear();
                    n.held.clear();
                    n.more = 0;
                }
            }
            for (child, l) in due.remove(&parent).unwrap_or_default() {
                n.lines.retain(|(c, _)| *c != child);
                n.lines.push((child, l));
            }
            let over = n.lines.len().saturating_sub(LINES_MAX);
            if over > 0 {
                n.lines.drain(..over);
                n.more = n.more.saturating_add(over);
            }
            if (n.lines.is_empty() && n.before.is_none()) || info.status == Status::Exited {
                self.news.remove(&parent);
                continue;
            }
            if info.status == Status::Disconnected {
                continue;
            }
            let text = n.text();
            match n.msg.as_mut() {
                Some(m) if m.text == text => {}
                Some(m) => {
                    out.push((parent, Command::QueueEdit { id: m.id.clone(), text: text.clone(), attachments: None }));
                    m.text = text;
                    n.said.clone_from(&n.lines);
                }
                None if quiet.contains(&parent) => {}
                None => {
                    let id = super::queued::made(NEWS);
                    out.push((parent, Command::Send { id: id.clone(), text: text.clone(), mode: SendMode::Queue, attachments: Vec::new(), overrides: None, reply_to: None}));
                    n.msg = Some(Kept::sent(id, text, info, now));
                    n.said.clone_from(&n.lines);
                    n.held.clone_from(&n.lines);
                }
            }
        }
        out
    }

    /// Command `c` from `plan` didn't reach `parent`: what it said goes at the next look (an edit
    /// that didn't go says nothing the message holds, so the lines it carried aren't taken as held).
    fn unsent(&mut self, parent: &str, c: &Command) {
        let Some(n) = self.news.get_mut(parent) else { return };
        match (c, n.msg.as_mut()) {
            (Command::Send { id, .. }, Some(m)) if m.id == *id => {
                n.msg = None;
                n.held.clear();
            }
            (Command::QueueEdit { id, .. }, Some(m)) if m.id == *id => m.text.clear(),
            _ => {}
        }
    }
}

/// A parent's news as one message: one child's line, or each child's under one heading (and how
/// many more settled, past the most it names); after what a message found waiting said (`before`).
fn text_of(before: Option<&str>, lines: &[(String, String)], more: usize) -> String {
    let mut text = match (lines, more) {
        ([], 0) => String::new(),
        ([(_, one)], 0) => one.clone(),
        _ => format!("Agents you started:\n{}", lines.iter().map(|(_, l)| format!("- {l}")).collect::<Vec<_>>().join("\n")),
    };
    if more > 0 {
        text.push_str(&format!("\n- and {more} more that settled before these (agents_list with `parent` lists them all)."));
    }
    match before {
        Some(b) if text.is_empty() => b.to_owned(),
        Some(b) => format!("{b}\n\n{text}"),
        None => text,
    }
}

/// One child as its parent is told of it.
fn line(c: &Info) -> String {
    let short: String = c.id.chars().take(8).collect();
    let who = if c.name.trim().is_empty() { format!("Agent {short}") } else { format!("Agent \u{201c}{}\u{201d} ({short})", c.name.trim()) };
    let said = super::model::clip_middle(c.summary.trim(), SAID_MAX).0;
    match stand(c) {
        "idle" => {
            let how = match c.last_turn.as_ref().map(|t| t.how.as_str()) {
                Some("cut") => " part-way",
                Some("refused") => ", refusing",
                Some("limit") => " at its turn limit",
                _ => "",
            };
            let said = if said.is_empty() { String::new() } else { format!(": {said}") };
            format!("{who}, which you started, ended its turn{how}{said}. agents_result {{\"agent\": \"{}\"}} has its whole reply and what it changed.", c.id)
        }
        "needs_you" => {
            let asks: Vec<String> = c.approvals.iter().map(|a| a.title.clone()).filter(|t| !t.trim().is_empty()).collect();
            let asks = if asks.is_empty() { String::new() } else { format!(" ({})", asks.join("; ")) };
            format!("{who}, which you started, waits on a person{asks}: tell the person what it asks and where (its page in kiln). It goes on once they answer.")
        }
        "stopped" => {
            let why = c.exit.as_ref().and_then(|e| e.error.clone()).map(|e| format!(": {}", super::model::clip_middle(e.trim(), SAID_MAX).0)).unwrap_or_default();
            format!("{who}, which you started, stopped{why}.")
        }
        "paused" => format!("{who}, which you started, is paused at a usage limit: it goes on when the limit resets."),
        _ => format!("{who}, which you started, ended."),
    }
}

/// Whether `agent` has children at work: an agent that ended its turn to let them work isn't
/// prodded to go on (it's told when each settles).
pub fn children_working(all: &[Info], agent: &str) -> bool {
    all.iter().any(|c| c.parent.as_deref() == Some(agent) && stand(c) == "working")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::model::{Status, TurnEnd};
    use crate::harness::queued::Queued;

    fn asks(title: &str) -> anyhow::Result<crate::harness::session::ApprovalBrief> {
        Ok(crate::harness::session::ApprovalBrief { id: "r1".into(), tool: crate::harness::model::ToolKind::Shell, tool_name: "Bash".into(), title: title.into(), at: 1 })
    }

    fn child(id: &str, status: Status, ended_at: i64, summary: &str) -> Info {
        Info {
            id: id.into(),
            name: format!("Child {id}"),
            parent: Some("p1".into()),
            status,
            summary: summary.into(),
            last_turn: (ended_at > 0).then(|| TurnEnd { at: ended_at, how: "done".into(), ..TurnEnd::default() }),
            ..Info::default()
        }
    }

    /// What's due, as the messages it makes: by parent, with its children's lines.
    fn told(t: &mut Told, all: &[Info], now: i64, driven: &[String]) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = t.due(all, now, driven).into_iter().map(|(p, lines)| (p, text_of(None, &lines, 0))).collect();
        out.sort();
        out
    }

    /// A child that worked and settled is told of once, after it has stood settled a while; one
    /// first seen settled (a restart) isn't; children that settle together go in one message; what
    /// the parent saw itself isn't told again.
    #[test]
    fn a_parent_is_told_when_its_children_settle_and_needn_t_wait() -> anyhow::Result<()> {
        let mut t = Told::default();
        let t0 = 1_000_000;
        // First seen: one already done (a restart), one at work.
        let done_before = child("c0", Status::Idle, 10, "old news");
        assert!(told(&mut t, &[done_before.clone(), child("c1", Status::Running, 0, "")], t0, &[]).is_empty());
        assert!(children_working(&[child("c1", Status::Running, 0, "")], "p1"));
        // c1 and c2 at work, then both done.
        assert!(told(&mut t, &[done_before.clone(), child("c1", Status::Running, 0, ""), child("c2", Status::Running, 0, "")], t0 + 100, &[]).is_empty());
        let settled = [done_before.clone(), child("c1", Status::Idle, t0 + 200, "Fixed the parser; tests pass."), child("c2", Status::Idle, t0 + 300, "Docs updated.")];
        assert!(told(&mut t, &settled, t0 + 400, &[]).is_empty(), "not before it has stood settled a while");
        let news = told(&mut t, &settled, t0 + 400 + SETTLE_MS, &[]);
        assert_eq!(news.len(), 1, "one message for the parent: {news:?}");
        let (parent, text) = news.first().cloned().unwrap_or_default();
        assert_eq!(parent, "p1");
        assert!(text.contains("Agents you started") && text.contains("Fixed the parser; tests pass.") && text.contains("Docs updated.") && text.contains("agents_result"), "{text}");
        assert!(!text.contains("old news"), "what was done before the server saw it isn't told");
        assert!(told(&mut t, &settled, t0 + 10_000, &[]).is_empty(), "told once");
        assert!(!children_working(&settled, "p1"));
        // c1 at work again, then waiting on a person; the parent saw that itself (agents_wait).
        let _ = told(&mut t, &[child("c1", Status::Running, t0 + 200, "")], t0 + 11_000, &[]);
        let mut asking = child("c1", Status::Waiting, t0 + 200, "");
        asking.approvals.push(asks("Run cargo publish")?);
        let _ = t.saw(&asking);
        assert!(told(&mut t, std::slice::from_ref(&asking), t0 + 20_000, &[]).is_empty());
        assert!(told(&mut t, std::slice::from_ref(&asking), t0 + 20_000 + SETTLE_MS, &[]).is_empty(), "what the parent saw isn't told again");
        // Children of no agent (a person started them) are nobody's to tell.
        let mut orphan = child("c3", Status::Running, 0, "");
        orphan.parent = None;
        let _ = told(&mut t, std::slice::from_ref(&orphan), t0, &[]);
        orphan.status = Status::Idle;
        assert!(told(&mut t, &[orphan], t0 + 30_000, &[]).is_empty());
        // A child its goal stack drives settles between goals unannounced; once no stack drives
        // it, its parent is told.
        let driven = vec!["c4".to_owned()];
        let _ = told(&mut t, &[child("c4", Status::Running, 0, "")], t0 + 40_000, &driven);
        let between = child("c4", Status::Idle, t0 + 41_000, "Goal 1 met.");
        assert!(told(&mut t, std::slice::from_ref(&between), t0 + 50_000, &driven).is_empty(), "between goals");
        let news = told(&mut t, std::slice::from_ref(&between), t0 + 60_000, &[]);
        assert!(news.first().is_some_and(|(p, t)| p == "p1" && t.contains("Goal 1 met.")), "once left to itself: {news:?}");
        Ok(())
    }

    /// CP-M3 (docs/programs.md, "Usage"): a parent paused for usage (`quiet`) gets no new message
    /// of news, since one would start its turn: what its children did waits, and goes once it goes
    /// on.
    #[test]
    fn a_parent_paused_for_usage_gets_its_news_once_it_goes_on() -> anyhow::Result<()> {
        let mut t = Told::default();
        let p = coordinator("q9");
        let mut kids = helpers("q9", &["k9"]);
        let (now, quiet) = (3_000_000, vec!["q9".to_owned()]);
        anyhow::ensure!(t.plan(&all(&p, &kids), now, &[], &quiet).is_empty(), "seen at work first");
        if let Some(c) = kids.first_mut() {
            c.status = Status::Idle;
            "Track 7's goal met.".clone_into(&mut c.summary);
            c.last_turn = Some(TurnEnd { at: now + 1, how: "done".into(), ..TurnEnd::default() });
        }
        anyhow::ensure!(t.plan(&all(&p, &kids), now + 1, &[], &quiet).is_empty());
        anyhow::ensure!(t.plan(&all(&p, &kids), now + 1 + SETTLE_MS, &[], &quiet).is_empty(), "nothing while it's paused for usage");
        let sent = t.plan(&all(&p, &kids), now + 2 + SETTLE_MS, &[], &[]);
        anyhow::ensure!(matches!(sent.first(), Some((parent, Command::Send { text, .. })) if parent == "q9" && text.contains("Track 7's goal met.")), "once it goes on: {sent:?}");
        Ok(())
    }

    #[test]
    fn what_a_parent_is_told_says_what_its_child_needs() -> anyhow::Result<()> {
        let mut asking = child("c9", Status::Waiting, 5, "");
        asking.approvals.push(asks("Run cargo publish")?);
        let text = line(&asking);
        assert!(text.contains("waits on a person (Run cargo publish)") && text.contains("tell the person"), "{text}");
        let mut crashed = child("c8", Status::Exited, 5, "");
        crashed.exit = Some(crate::harness::model::ExitInfo { code: Some(1), error: Some("claude exited with 1".into()), ..Default::default() });
        assert!(line(&crashed).contains("stopped: claude exited with 1"), "{}", line(&crashed));
        Ok(())
    }

    /// A parent at work on one long turn, its queue empty.
    fn coordinator(id: &str) -> Info {
        Info { id: id.into(), name: "Coordinator".into(), status: Status::Running, ..Info::default() }
    }

    /// Children of `parent`, at work.
    fn helpers(parent: &str, ids: &[&str]) -> Vec<Info> {
        ids.iter().map(|id| Info { parent: Some(parent.into()), ..child(id, Status::Running, 0, "") }).collect()
    }

    fn all(p: &Info, kids: &[Info]) -> Vec<Info> {
        std::iter::once(p.clone()).chain(kids.iter().cloned()).collect()
    }

    /// What the parent's harness does with `c` behind its turn, as kiln then sees its queue.
    fn apply(p: &mut Info, c: &Command) {
        match c {
            Command::Send { id, text, .. } => p.queue.push(Queued::of(id, text)),
            Command::QueueEdit { id, text, .. } => {
                if let Some(q) = p.queue.iter_mut().find(|q| q.id == *id) {
                    *q = Queued::of(id, text);
                }
            }
            Command::QueueRemove { id } => p.queue.retain(|q| q.id != *id),
            _ => {}
        }
        p.queued = p.queue.len();
    }

    /// Children `which` (by index, with their last words) go to work, then end a turn and stand
    /// settled a while, from `now`: what goes to their parent `p` meanwhile.
    fn settle(t: &mut Told, p: &Info, kids: &mut [Info], which: &[(usize, &str)], now: i64) -> Vec<Command> {
        let mut out = Vec::new();
        for (step, at) in [(0, now), (1, now + 1), (2, now + 1 + SETTLE_MS)] {
            for (k, said) in which {
                if let Some(c) = kids.get_mut(*k) {
                    if step == 0 {
                        c.status = Status::Running;
                    } else if step == 1 {
                        c.status = Status::Idle;
                        (*said).clone_into(&mut c.summary);
                        c.last_turn = Some(TurnEnd { at: now + 1, how: "done".into(), ..TurnEnd::default() });
                    }
                }
            }
            out.extend(t.plan(&all(p, kids), at, &[], &[]).into_iter().map(|(_, c)| c));
        }
        out
    }

    /// What kiln last had the parent's news say.
    fn news_of(t: &Told, parent: &str) -> String {
        t.news.get(parent).and_then(|n| n.msg.as_ref()).map(|m| m.text.clone()).unwrap_or_default()
    }

    /// The incident: a coordinator in one turn for hours, its children settling again and again.
    /// What waits for it is one message, each child's latest line in it once; when its turn ends
    /// and it takes that, what settles next is news again, in a message of its own.
    #[test]
    fn a_parent_in_a_long_turn_has_one_message_of_news_waiting_each_child_s_latest_line_once() -> anyhow::Result<()> {
        let mut t = Told::default();
        let mut p = coordinator("q1");
        let mut kids = helpers("q1", &["k1", "k2", "k3"]);
        let mut now = 2_000_000;
        assert!(t.plan(&all(&p, &kids), now, &[], &[]).is_empty(), "seen at work first");
        // Two settle: one message, behind the parent's turn.
        now += 10_000;
        let first = settle(&mut t, &p, &mut kids, &[(0, "The lexer is done."), (1, "Docs updated.")], now);
        let Some(Command::Send { id, text, mode: SendMode::Queue, .. }) = first.first() else { anyhow::bail!("no news: {first:?}") };
        anyhow::ensure!(first.len() == 1 && text.contains("Agents you started") && text.contains("The lexer is done.") && text.contains("Docs updated."), "{first:?}");
        let id = id.clone();
        first.iter().for_each(|c| apply(&mut p, c));
        // While it waits, one settles again and another for the first time; then one again and
        // again: each time the one message is edited, never another queued.
        let rounds: [&[(usize, &str)]; 4] = [&[(0, "The parser is done."), (2, "The bench is recorded.")], &[(1, "Docs: round 1.")], &[(1, "Docs: round 2.")], &[(1, "Docs: round 3.")]];
        for which in rounds {
            now += 10_000;
            let sent = settle(&mut t, &p, &mut kids, which, now);
            anyhow::ensure!(!sent.is_empty() && sent.iter().all(|c| matches!(c, Command::QueueEdit { id: e, .. } if *e == id)), "the waiting one edited, nothing more queued: {sent:?}");
            sent.iter().for_each(|c| apply(&mut p, c));
        }
        // A look later: nothing more to say, and its queue is seen holding the last edit.
        anyhow::ensure!(t.plan(&all(&p, &kids), now + 5_000, &[], &[]).is_empty());
        let latest = news_of(&t, "q1");
        anyhow::ensure!(p.queue == [Queued::of(&id, &latest)], "its queue holds the one message, as kiln last had it: {:?}", p.queue);
        for old in ["The lexer is done.", "Docs updated.", "Docs: round 1.", "Docs: round 2."] {
            anyhow::ensure!(!latest.contains(old), "{old:?} isn't news any more: {latest}");
        }
        for now_news in ["The parser is done.", "The bench is recorded.", "Docs: round 3."] {
            anyhow::ensure!(latest.contains(now_news), "{now_news:?} missing: {latest}");
        }
        anyhow::ensure!(latest.matches("which you started").count() == 3, "each child once: {latest}");
        // Its turn ends and it takes the news; what settles next is news again, on its own.
        p.queue.clear();
        p.queued = 0;
        now += 10_000;
        let next = settle(&mut t, &p, &mut kids, &[(2, "The bench is green.")], now);
        let Some(Command::Send { id: again, text, .. }) = next.first() else { anyhow::bail!("no news after the parent took the last: {next:?}") };
        anyhow::ensure!(next.len() == 1 && *again != id && text.contains("The bench is green.") && !text.contains("Docs: round 3.") && !text.contains("Agents you started"), "{next:?}");
        Ok(())
    }

    /// An edit that reaches the parent's machine just after its turn ended, the news gone with
    /// what it said before, does nothing there: what the edit said goes again, in the next message.
    #[test]
    fn what_an_edit_too_late_said_goes_again_in_the_next_message() -> anyhow::Result<()> {
        let mut t = Told::default();
        let mut p = coordinator("q2");
        let mut kids = helpers("q2", &["j1", "j2"]);
        let mut now = 3_000_000;
        let _ = t.plan(&all(&p, &kids), now, &[], &[]);
        now += 10_000;
        let first = settle(&mut t, &p, &mut kids, &[(0, "First news.")], now);
        first.iter().for_each(|c| apply(&mut p, c));
        anyhow::ensure!(matches!(first.as_slice(), [Command::Send { .. }]), "{first:?}");
        now += 10_000;
        let edit = settle(&mut t, &p, &mut kids, &[(1, "Second news.")], now);
        anyhow::ensure!(matches!(edit.as_slice(), [Command::QueueEdit { .. }]), "{edit:?}");
        // Before the edit came, the turn ended and the message went as it was.
        p.queue.clear();
        p.queued = 0;
        let resent = t.plan(&all(&p, &kids), now + 10_000, &[], &[]);
        let Some((to, Command::Send { text, .. })) = resent.first() else { anyhow::bail!("the edit's words were lost: {resent:?}") };
        anyhow::ensure!(resent.len() == 1 && to == "q2" && text.contains("Second news.") && !text.contains("First news."), "only what didn't reach it: {resent:?}");
        Ok(())
    }

    /// News that didn't reach the parent (its machine away a moment) goes at the next look: a
    /// message again, an edit again; and should the message go meanwhile, what the edit said goes
    /// in the next.
    #[test]
    fn news_that_didn_t_reach_its_parent_goes_at_the_next_look() -> anyhow::Result<()> {
        let mut t = Told::default();
        let mut p = coordinator("q6");
        let mut kids = helpers("q6", &["w1", "w2"]);
        let mut now = 6_000_000;
        let _ = t.plan(&all(&p, &kids), now, &[], &[]);
        now += 10_000;
        let first = settle(&mut t, &p, &mut kids, &[(0, "First news.")], now);
        let Some(send) = first.first() else { anyhow::bail!("no news") };
        t.unsent("q6", send);
        let again = t.plan(&all(&p, &kids), now + 10_000, &[], &[]);
        anyhow::ensure!(matches!(again.as_slice(), [(_, Command::Send { text, .. })] if text.contains("First news.")), "{again:?}");
        again.iter().for_each(|(_, c)| apply(&mut p, c));
        now += 20_000;
        let edit = settle(&mut t, &p, &mut kids, &[(1, "Second news.")], now);
        let Some(c) = edit.first() else { anyhow::bail!("no edit") };
        t.unsent("q6", c);
        let again = t.plan(&all(&p, &kids), now + 10_000, &[], &[]);
        anyhow::ensure!(again.len() == 1 && again.first().map(|(_, c)| c) == Some(c), "the same edit again: {again:?}");
        // It didn't go either, and the parent took the message as it was.
        t.unsent("q6", c);
        p.queue.clear();
        p.queued = 0;
        let next = t.plan(&all(&p, &kids), now + 20_000, &[], &[]);
        anyhow::ensure!(matches!(next.as_slice(), [(_, Command::Send { text, .. })] if text.contains("Second news.") && !text.contains("First news.")), "{next:?}");
        Ok(())
    }

    /// While the parent's machine is away, kiln's view of its queue stands still: news that comes
    /// meanwhile waits, and isn't taken for what the message waiting said. Its turn ended during the
    /// outage and it took that message as it was; once back, it's told what came meanwhile.
    #[test]
    fn news_that_came_while_its_parent_s_machine_was_away_goes_once_it_s_back() -> anyhow::Result<()> {
        let mut t = Told::default();
        let mut p = coordinator("q7");
        let mut kids = helpers("q7", &["v1", "v2"]);
        let mut now = 7_000_000;
        let _ = t.plan(&all(&p, &kids), now, &[], &[]);
        now += 10_000;
        let first = settle(&mut t, &p, &mut kids, &[(0, "v1 is done.")], now);
        first.iter().for_each(|c| apply(&mut p, c));
        anyhow::ensure!(t.plan(&all(&p, &kids), now + 5_000, &[], &[]).is_empty(), "seen waiting");
        p.status = Status::Disconnected;
        now += 10_000;
        anyhow::ensure!(settle(&mut t, &p, &mut kids, &[(1, "v2 is done.")], now).is_empty(), "nothing goes while it's away");
        anyhow::ensure!(t.plan(&all(&p, &kids), now + 10_000, &[], &[]).is_empty());
        p.status = Status::Running;
        p.queue.clear();
        p.queued = 0;
        let back = t.plan(&all(&p, &kids), now + 20_000, &[], &[]);
        anyhow::ensure!(matches!(back.as_slice(), [(_, Command::Send { text, .. })] if text.contains("v2 is done.") && !text.contains("v1 is done.")), "{back:?}");
        Ok(())
    }

    /// A server that started since finds the news a server before it left waiting for a parent (its
    /// id says what it is): it adds to that message, what it said first, rather than queue another.
    #[test]
    fn news_a_server_before_left_waiting_is_added_to_not_queued_again() -> anyhow::Result<()> {
        let mut t = Told::default();
        let old = "news-01a16251c49a75fba41b1fae971ba8a9";
        let before = "Agent “Helper 0” (h0), which you started, ended its turn: from before the restart.";
        let mut p = Info { queue: vec![Queued::of(old, before)], queued: 1, ..coordinator("q8") };
        let mut kids = helpers("q8", &["u1"]);
        let mut now = 8_000_000;
        t.found("q8", old.into(), before.into(), now);
        anyhow::ensure!(t.plan(&all(&p, &kids), now, &[], &[]).is_empty(), "nothing new to say");
        now += 10_000;
        let sent = settle(&mut t, &p, &mut kids, &[(0, "u1 is done.")], now);
        let [Command::QueueEdit { id, text, .. }] = sent.as_slice() else { anyhow::bail!("{sent:?}") };
        anyhow::ensure!(id == old && text.starts_with(before) && text.contains("u1 is done."), "{text}");
        sent.iter().for_each(|c| apply(&mut p, c));
        // Once the parent takes it, what came before goes with it.
        anyhow::ensure!(t.plan(&all(&p, &kids), now + 10_000, &[], &[]).is_empty());
        p.queue.clear();
        p.queued = 0;
        now += 20_000;
        let next = settle(&mut t, &p, &mut kids, &[(0, "u1 again.")], now);
        anyhow::ensure!(matches!(next.as_slice(), [Command::Send { id, text, .. }] if id.starts_with(NEWS) && !text.contains("before the restart")), "{next:?}");
        Ok(())
    }

    /// What the parent sees itself (agents_wait, agents_result) comes out of its news still waiting;
    /// the last line out takes the message out of its queue.
    #[test]
    fn what_a_parent_sees_itself_comes_out_of_its_news_and_the_message_with_the_last_line() -> anyhow::Result<()> {
        let mut t = Told::default();
        let mut p = coordinator("q3");
        let mut kids = helpers("q3", &["x1", "x2", "x3"]);
        let mut now = 4_000_000;
        let _ = t.plan(&all(&p, &kids), now, &[], &[]);
        now += 10_000;
        let first = settle(&mut t, &p, &mut kids, &[(0, "x1 is done."), (1, "x2 is done."), (2, "x3 is done.")], now);
        first.iter().for_each(|c| apply(&mut p, c));
        let Some(Command::Send { id, .. }) = first.first() else { anyhow::bail!("{first:?}") };
        let id = id.clone();
        let (x1, x2, x3) = match kids.as_slice() {
            [a, b, c] => (a.clone(), b.clone(), c.clone()),
            _ => anyhow::bail!("three helpers"),
        };
        // It waited for x1: x1's line comes out, the rest stay, in the same message.
        let Some((to, c)) = t.saw(&x1) else { anyhow::bail!("x1's line stayed in the news") };
        let Command::QueueEdit { id: edited, text, .. } = &c else { anyhow::bail!("{c:?}") };
        anyhow::ensure!(to == "q3" && *edited == id && !text.contains("x1 is done.") && text.contains("x2 is done.") && text.contains("x3 is done."), "{text}");
        apply(&mut p, &c);
        // It read x2's result: one line left, said on its own.
        let Some((_, c)) = t.saw(&x2) else { anyhow::bail!("x2's line stayed in the news") };
        anyhow::ensure!(matches!(&c, Command::QueueEdit { text, .. } if text.contains("x3 is done.") && !text.contains("x2 is done.") && !text.contains("Agents you started")), "{c:?}");
        apply(&mut p, &c);
        // And x3's: nothing left, so the message goes.
        let Some((_, c)) = t.saw(&x3) else { anyhow::bail!("x3's line stayed in the news") };
        anyhow::ensure!(c == Command::QueueRemove { id: id.clone() }, "{c:?}");
        apply(&mut p, &c);
        anyhow::ensure!(p.queue.is_empty(), "{:?}", p.queue);
        anyhow::ensure!(t.saw(&x1).is_none(), "nothing of it waits");
        // Nothing comes back for what it saw.
        anyhow::ensure!(t.plan(&all(&p, &kids), now + 20_000, &[], &[]).is_empty());
        anyhow::ensure!(t.plan(&all(&p, &kids), now + 20_000 + SETTLE_MS, &[], &[]).is_empty());
        // x1 settles again: news again.
        now += 30_000;
        let again = settle(&mut t, &p, &mut kids, &[(0, "x1 is done again.")], now);
        anyhow::ensure!(matches!(again.as_slice(), [Command::Send { text, .. }] if text.contains("x1 is done again.")), "{again:?}");
        Ok(())
    }

    /// An ended parent isn't sent its children's news; one whose machine is away is, once it's
    /// back; and a parent's news names at most `LINES_MAX` children, saying how many more settled.
    #[test]
    fn news_waits_for_a_parent_s_machine_and_names_a_bounded_number_of_children() -> anyhow::Result<()> {
        let mut t = Told::default();
        let mut p = Info { status: Status::Disconnected, ..coordinator("q4") };
        let ids: Vec<String> = (0..LINES_MAX + 3).map(|n| format!("y{n}")).collect();
        let mut kids = helpers("q4", &ids.iter().map(String::as_str).collect::<Vec<_>>());
        let mut now = 5_000_000;
        let _ = t.plan(&all(&p, &kids), now, &[], &[]);
        now += 10_000;
        let which: Vec<(usize, &str)> = (0..kids.len()).map(|k| (k, "Done.")).collect();
        anyhow::ensure!(settle(&mut t, &p, &mut kids, &which, now).is_empty(), "its machine is away");
        p.status = Status::Running;
        let back = t.plan(&all(&p, &kids), now + 20_000, &[], &[]);
        let Some((_, Command::Send { text, .. })) = back.first() else { anyhow::bail!("nothing once it's back: {back:?}") };
        anyhow::ensure!(text.matches("which you started").count() == LINES_MAX && text.contains("and 3 more that settled before these"), "{text}");
        // Ended: nothing for it, and its news goes.
        let mut ended = Info { status: Status::Exited, ..coordinator("q5") };
        let mut kids = helpers("q5", &["z1"]);
        let _ = t.plan(&all(&ended, &kids), now, &[], &[]);
        anyhow::ensure!(settle(&mut t, &ended, &mut kids, &[(0, "Done.")], now + 10_000).is_empty());
        ended.status = Status::Running;
        anyhow::ensure!(t.plan(&all(&ended, &kids), now + 30_000, &[], &[]).is_empty(), "told once, while it had ended: not again");
        Ok(())
    }
}
