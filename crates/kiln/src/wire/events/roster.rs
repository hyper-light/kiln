//! The agents, lean, down an events socket that asks for them so (`?lean=1`; docs/wire.md, "The events socket";
//! docs/ci.md W21-R02). An agent's every change is a `harness` event that says it whole, 1.2 KB, to every page:
//! while one works that is three a second, of which a page shows nothing new (its last words, said as it goes,
//! which a row shows only once its turn is over; and when it was last at work). A socket that asks is sent, of
//! each agent:
//!
//! - its entry lean, as a `harness` event, the first time and after anything it may have missed: without where
//!   its harness started (`starts`, `runs`: no list reads them, as `GET /api/harness/sessions?lean=1` leaves
//!   them out) and, while its turn runs, without its last words (`summary`);
//! - from then on only what changed of that, as a `harness_patch` ([`RosterEvent`]): the fields set and those
//!   gone; and nothing at all when nothing of it changed.
//!
//! The page keeps each agent's entry as this socket last said it and applies what changed to it
//! (`ui/src/api/roster-feed.ts`), so everything that listens for `harness` events hears the entry whole, as before.
//!
//! Pure: a frame's text in, what to send out. What a [`Roster`] keeps is the lean entry its socket was last sent
//! of each agent, as its JSON text (about a kilobyte an agent), to tell the next from.

use std::borrow::Cow;
use std::collections::HashMap;

use serde::Serialize;
use serde_json::{Map, Value};

/// What a socket that takes the agents lean is sent in an agent's `harness` event's place, in a batch
/// ([`super::EventsDown::Batch`]'s `events`), once it has the agent's entry.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RosterEvent {
    /// What changed of an agent session's entry (`harness::session::Info`, lean) since this socket was last
    /// sent it: `set`, the fields that are new or have another value, each whole; `unset`, those it no longer
    /// has (none: left out).
    HarnessPatch {
        id: String,
        set: Map<String, Value>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        #[cfg_attr(test, ts(as = "Option<Vec<String>>", optional))]
        unset: Vec<String>,
    },
}

/// What a lean entry never has: where the agent's harness ran each time it started, which no list of them reads.
const NEVER: [&str; 2] = ["starts", "runs"];

/// An agent's states (`harness::model::Status`, as it's written) in which its turn runs: its last words are
/// still being said then, and no row shows them until it's over.
const TURN_RUNS: [&str; 2] = ["running", "starting"];

/// `info`, an agent's entry, lean.
fn lean(info: &mut Map<String, Value>) {
    for key in NEVER {
        info.remove(key);
    }
    if info.get("status").and_then(Value::as_str).is_some_and(|status| TURN_RUNS.contains(&status)) {
        info.remove("summary");
    }
}

/// What one socket that takes the agents lean was last sent of each.
#[derive(Debug, Default)]
pub struct Roster {
    /// The lean entry it was last sent of each agent, as written.
    last: HashMap<String, Box<str>>,
    /// Their bytes.
    bytes: usize,
}

impl Roster {
    /// What to send of `text`, an agent's `harness` frame: its entry lean the first time, then what changed of
    /// it, and none when nothing did. A frame that says no agent's entry goes as it is.
    pub fn say<'a>(&mut self, text: &'a str) -> Option<Cow<'a, str>> {
        let Ok(Value::Object(mut frame)) = serde_json::from_str::<Value>(text) else { return Some(Cow::Borrowed(text)) };
        let Some(Value::Object(mut info)) = frame.remove("info") else { return Some(Cow::Borrowed(text)) };
        let Some(id) = info.get("id").and_then(Value::as_str).map(str::to_owned) else { return Some(Cow::Borrowed(text)) };
        lean(&mut info);
        let Ok(now) = serde_json::to_string(&info) else { return Some(Cow::Borrowed(text)) };
        // As it was last said, to the letter (only what a lean entry leaves out changed): nothing, with nothing read back.
        if self.last.get(&id).is_some_and(|was| **was == *now) {
            return None;
        }
        let was = self.last.get(&id).and_then(|was| serde_json::from_str::<Map<String, Value>>(was).ok());
        let said = match was {
            // Never said to this socket (or not to be read back): whole.
            None => Some(format!(r#"{{"type":"harness","info":{now}}}"#)),
            Some(was) => {
                let set: Map<String, Value> = info.iter().filter(|(key, value)| was.get(key.as_str()) != Some(*value)).map(|(key, value)| (key.clone(), value.clone())).collect();
                let unset: Vec<String> = was.keys().filter(|key| !info.contains_key(key.as_str())).cloned().collect();
                if set.is_empty() && unset.is_empty() { None } else { serde_json::to_string(&RosterEvent::HarnessPatch { id: id.clone(), set, unset }).ok() }
            }
        };
        self.keep(id, now);
        said.map(Cow::Owned)
    }

    fn keep(&mut self, id: String, entry: String) {
        self.bytes = self.bytes.saturating_add(entry.len());
        if let Some(was) = self.last.insert(id, entry.into_boxed_str()) {
            self.bytes = self.bytes.saturating_sub(was.len());
        }
    }

    /// The agent is gone: nothing is kept of it (one that comes again under its id is said whole).
    pub fn closed(&mut self, id: &str) {
        if let Some(was) = self.last.remove(id) {
            self.bytes = self.bytes.saturating_sub(was.len());
        }
    }

    /// What it was last sent counts for nothing (it may have missed a change): each agent's next goes whole.
    pub fn clear(&mut self) {
        self.last.clear();
        self.bytes = 0;
    }

    /// How many agents it keeps an entry of.
    pub fn len(&self) -> usize {
        self.last.len()
    }

    pub fn is_empty(&self) -> bool {
        self.last.is_empty()
    }

    /// The bytes of the entries it keeps.
    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// An agent's entry as `harness::session::Info` writes one (some of its fields), as a `harness` frame.
    fn frame(info: &Value) -> String {
        json!({"type": "harness", "info": info}).to_string()
    }

    fn entry(id: &str, status: &str, summary: &str, last_activity: u64) -> Value {
        json!({"id": id, "kind": "claude", "name": "Claude Code", "status": status, "title": "The gate", "summary": summary, "queued": 0, "approvals": [], "last_activity": last_activity,
            "runs": {"pid": 4242}, "starts": [{"at": 1, "runs": {"pid": 4242}}]})
    }

    fn read(said: Option<Cow<'_, str>>) -> anyhow::Result<Value> {
        Ok(serde_json::from_str(&said.ok_or_else(|| anyhow::anyhow!("nothing was said"))?)?)
    }

    /// As the page applies one (`ui/src/api/roster-feed.ts`): the entry it had with `said`'s fields set and unset;
    /// a whole entry in its place.
    fn applied(had: Option<Value>, said: &Value) -> anyhow::Result<Value> {
        if said.get("type") == Some(&json!("harness")) {
            return said.get("info").cloned().ok_or_else(|| anyhow::anyhow!("a harness event with no info"));
        }
        anyhow::ensure!(said.get("type") == Some(&json!("harness_patch")), "neither an entry nor what changed of one: {said}");
        let mut now = had.and_then(|had| had.as_object().cloned()).ok_or_else(|| anyhow::anyhow!("what changed of an entry never said whole"))?;
        for (key, value) in said.get("set").and_then(Value::as_object).into_iter().flatten() {
            now.insert(key.clone(), value.clone());
        }
        for key in said.get("unset").and_then(Value::as_array).into_iter().flatten() {
            now.remove(key.as_str().unwrap_or_default());
        }
        Ok(Value::Object(now))
    }

    /// The entry lean, as [`lean`] makes it.
    fn leaned(info: &Value) -> Value {
        let mut info = info.as_object().cloned().unwrap_or_default();
        lean(&mut info);
        Value::Object(info)
    }

    /// W21-R02: an agent is said whole and lean the first time, then only what changed of it: while its turn
    /// runs, never its last words; and nothing when only they changed.
    #[test]
    fn w21_r02_an_agent_is_said_lean_once_then_only_what_changed_of_it() -> anyhow::Result<()> {
        let mut roster = Roster::default();
        // First: whole, without where it started, and with its last words (its turn isn't running).
        let first = read(roster.say(&frame(&entry("a", "idle", "Done: the gate is green", 100))))?;
        assert_eq!(first, json!({"type": "harness", "info": {"id": "a", "kind": "claude", "name": "Claude Code", "status": "idle", "title": "The gate", "summary": "Done: the gate is green", "queued": 0, "approvals": [], "last_activity": 100}}));
        // Its turn starts: its status, and its last words gone from what a page keeps of it.
        let started = read(roster.say(&frame(&entry("a", "running", "Done: the gate is green", 200))))?;
        assert_eq!(started, json!({"type": "harness_patch", "id": "a", "set": {"status": "running", "last_activity": 200}, "unset": ["summary"]}));
        // As it works, what it says changes at every word: only when it was last at work is news.
        let working = read(roster.say(&frame(&entry("a", "running", "Looking at the", 300))))?;
        assert_eq!(working, json!({"type": "harness_patch", "id": "a", "set": {"last_activity": 300}}));
        // Only its words changed: nothing.
        assert_eq!(roster.say(&frame(&entry("a", "running", "Looking at the gate", 300))), None);
        // Said again as it is: nothing.
        assert_eq!(roster.say(&frame(&entry("a", "running", "Looking at the gate", 300))), None);
        // Its turn over: its status and what it said last.
        let over = read(roster.say(&frame(&entry("a", "idle", "Fixed: two tests", 400))))?;
        assert_eq!(over, json!({"type": "harness_patch", "id": "a", "set": {"status": "idle", "summary": "Fixed: two tests", "last_activity": 400}}));
        Ok(())
    }

    /// W21-R02: what a page keeps of an agent, applying each thing said in order, is the agent's entry lean, at
    /// every step: through fields that come, go and change, another agent's between, and an agent that goes and
    /// comes again under its id.
    #[test]
    fn w21_r02_what_is_said_in_order_makes_each_entry_as_it_is() -> anyhow::Result<()> {
        let mut roster = Roster::default();
        let mut page: HashMap<String, Value> = HashMap::new();
        let mut with = entry("a", "waiting", "Asks", 5);
        if let Some(fields) = with.as_object_mut() {
            fields.insert("approvals".into(), json!([{"id": "p1", "tool": "shell", "title": "cargo test", "at": 5}]));
            fields.insert("doing".into(), json!("cargo test"));
        }
        let steps = [
            entry("a", "starting", "", 1),
            entry("b", "idle", "", 1),
            entry("a", "running", "One", 2),
            entry("a", "running", "One two", 3),
            with,
            entry("b", "idle", "", 1),
            entry("a", "idle", "Done", 6),
            entry("b", "exited", "Bye", 7),
        ];
        for step in &steps {
            let id = step.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
            if let Some(said) = roster.say(&frame(step)) {
                let now = applied(page.get(&id).cloned(), &serde_json::from_str(&said)?)?;
                page.insert(id.clone(), now);
            }
            assert_eq!(page.get(&id), Some(&leaned(step)), "after {step}");
        }
        assert_eq!(roster.len(), 2);
        // Gone, and here again under its id: whole.
        roster.closed("a");
        assert_eq!(roster.len(), 1);
        let again = read(roster.say(&frame(&entry("a", "idle", "Done", 6))))?;
        assert_eq!(again.get("type"), Some(&json!("harness")));
        // After anything it may have missed, each is whole again.
        roster.clear();
        assert_eq!((roster.len(), roster.bytes()), (0, 0));
        assert_eq!(read(roster.say(&frame(&entry("b", "exited", "Bye", 7))))?.get("type"), Some(&json!("harness")));
        Ok(())
    }

    /// W21-R02: what a roster keeps is each agent's lean entry once, as long as it was written; and a frame that
    /// says no agent's entry goes as it is.
    #[test]
    fn w21_r02_a_roster_keeps_each_agents_entry_once() -> anyhow::Result<()> {
        let mut roster = Roster::default();
        let size = |info: &Value| leaned(info).to_string().len();
        let (short, long) = (entry("a", "idle", "", 1), entry("a", "idle", "Its last words, some of them", 2));
        roster.say(&frame(&short));
        assert_eq!(roster.bytes(), size(&short));
        roster.say(&frame(&long));
        assert_eq!(roster.bytes(), size(&long));
        roster.say(&frame(&entry("b", "idle", "", 1)));
        assert_eq!(roster.bytes(), size(&long) + size(&entry("b", "idle", "", 1)));
        roster.closed("a");
        roster.closed("nobody");
        assert_eq!(roster.bytes(), size(&entry("b", "idle", "", 1)));
        for odd in [r#"{"type":"harness","info":null}"#, r#"{"type":"harness","info":{"title":"no id"}}"#, "not JSON"] {
            assert_eq!(roster.say(odd), Some(Cow::Borrowed(odd)));
        }
        assert_eq!(roster.len(), 1);
        Ok(())
    }

    /// W21-R02: what changed of an entry is written as [`RosterEvent`] says, its `unset` left out when nothing went.
    #[test]
    fn w21_r02_what_changed_is_written_as_its_type_says() -> anyhow::Result<()> {
        let set: Map<String, Value> = [("queued".to_owned(), json!(2))].into_iter().collect();
        assert_eq!(serde_json::to_string(&RosterEvent::HarnessPatch { id: "a".into(), set: set.clone(), unset: Vec::new() })?, r#"{"type":"harness_patch","id":"a","set":{"queued":2}}"#);
        assert_eq!(serde_json::to_string(&RosterEvent::HarnessPatch { id: "a".into(), set, unset: vec!["doing".into()] })?, r#"{"type":"harness_patch","id":"a","set":{"queued":2},"unset":["doing"]}"#);
        Ok(())
    }
}
