//! Mission control's attention snapshot, lean (docs/ci.md W21-R02; docs/agents.md, "The snapshot, lean"): what
//! the list's page reads of a snapshot, each agent's id written once. A snapshot names every agent four times
//! over (its mark's key, its place in the order, and its place in each of the visit's two orders), each time by
//! its 32 characters, and carries what the page never reads: 137 agents' is 37.6 KB whole and 5.1 KB lean, 11.7 KB
//! with the last words of the 23 rows a page 900 px tall opens on, each as long as a summary gets (`kiln-lab
//! bar`, 2026-11-14).
//!
//! A lean snapshot is the whole one but for:
//!
//! - `ids`: every agent it names, once, the order's first (a column: `crate::wire::columns::pack_column`);
//! - `order`, `request_order`, and `pair`'s `order_a` and `order_b`: each agent by its place in `ids`, and a
//!   list that is `ids`' first n in their order is the number n;
//! - `agents`: the marks as columns (`crate::wire::columns`), each without its score (`rank`), with `at`: whose
//!   they are, by place in `ids`, when they aren't its first `rows` in their order;
//! - `archived`: the archived agents as columns;
//! - `said`, when the page asks for the rows it opens on (`said=<n>`: the list's first n, and its last, which
//!   the page keeps in it for Tab to end on): their last words, `{"at", "v"}` by place in `ids`: what a lean list
//!   of the sessions leaves out of them (`GET /api/harness/sessions?lean=1`);
//! - `without`: what it leaves out ([`WITHOUT`]).
//!
//! Every other key is as the whole snapshot has it, one this doesn't know of too. [`whole`] reads one back (as
//! `ui/src/api/attention-lean.ts` does).

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value, json};

use crate::wire::columns;

/// What a lean snapshot leaves out: the learned order's report, the examination's and the urgency's (the
/// Settings page reads the first from a replay, and no page the others), and each agent's score.
pub const WITHOUT: [&str; 4] = ["model", "examination", "urgency", "agents.rank"];

/// The agents a snapshot names, each once: its place is where it was first named.
#[derive(Default)]
struct Ids<'a> {
    all: Vec<&'a str>,
    place: HashMap<&'a str, usize>,
}

impl<'a> Ids<'a> {
    fn place(&mut self, id: &'a str) -> usize {
        if let Some(at) = self.place.get(id) {
            return *at;
        }
        let at = self.all.len();
        self.all.push(id);
        self.place.insert(id, at);
        at
    }

    /// `ids` (a list of them) by place.
    fn places(&mut self, ids: &'a Value) -> Option<Vec<usize>> {
        ids.as_array()?.iter().map(|id| id.as_str().map(|id| self.place(id))).collect()
    }
}

/// Places as they're written: the first n in their order as n, else each.
fn written(places: &[usize]) -> Value {
    if places.iter().enumerate().all(|(i, at)| i == *at) { json!(places.len()) } else { json!(places) }
}

/// `snapshot` lean, with the last words of the `rows` rows the list shows first when `rows` isn't zero. `words`:
/// an agent's last words (empty: it has said none), or none for one the list doesn't show as it opens (one that
/// has ended, or isn't there). `now`: the time a park is over by. None when `snapshot` isn't a snapshot.
pub fn lean(snapshot: &Value, rows: usize, now: i64, words: impl Fn(&str) -> Option<String>) -> Option<Value> {
    let whole = snapshot.as_object()?;
    let marks = whole.get("agents")?.as_object()?;
    let mut ids = Ids::default();
    let order = ids.places(whole.get("order")?)?;
    // The marks in their agents' places, those of the order first.
    let mut marked: Vec<(usize, Map<String, Value>)> = marks
        .iter()
        .map(|(id, mark)| {
            let mut mark = mark.as_object()?.clone();
            mark.remove("rank");
            Some((ids.place(id), mark))
        })
        .collect::<Option<_>>()?;
    marked.sort_by_key(|(at, _)| *at);
    let mut agents = columns::pack(&marked.iter().map(|(_, mark)| mark).collect::<Vec<_>>());
    let whose: Vec<usize> = marked.iter().map(|(at, _)| *at).collect();
    if let (Some(fields), false) = (agents.as_object_mut(), whose.iter().enumerate().all(|(i, at)| i == *at)) {
        fields.insert("at".to_owned(), json!(whose));
    }
    let mut lean = Map::new();
    for (key, value) in whole {
        let value = match key.as_str() {
            "model" | "examination" | "urgency" | "order" | "agents" => continue,
            "request_order" => written(&ids.places(value)?),
            "pair" => match value.as_object() {
                Some(pair) => {
                    let mut pair = pair.clone();
                    for side in ["order_a", "order_b"] {
                        if let Some(shown) = whole.get("pair").and_then(|pair| pair.get(side)) {
                            pair.insert(side.to_owned(), written(&ids.places(shown)?));
                        }
                    }
                    Value::Object(pair)
                }
                None => value.clone(),
            },
            "archived" => columns::pack(&value.as_array()?.iter().map(Value::as_object).collect::<Option<Vec<_>>>()?),
            _ => value.clone(),
        };
        lean.insert(key.clone(), value);
    }
    lean.insert("order".to_owned(), written(&order));
    lean.insert("agents".to_owned(), agents);
    if rows > 0 {
        let (at, v): (Vec<usize>, Vec<String>) = first_said(&ids, &order, marks, rows, now, words).into_iter().unzip();
        lean.insert("said".to_owned(), json!({ "at": at, "v": v }));
    }
    lean.insert("ids".to_owned(), columns::pack_column(&ids.all.iter().map(|id| json!(id)).collect::<Vec<Value>>().iter().collect::<Vec<_>>()));
    lean.insert("without".to_owned(), json!(WITHOUT));
    Some(Value::Object(lean))
}

/// The last words of the rows the list has in the page as it opens, each by its agent's place in `ids`: its
/// first `rows` (your pins in their order, then the order's) and its last (the page keeps the row Tab ends on
/// in it wherever it's scrolled: ui/src/lib/list-window.ts), without the parked and the archived (they're in lists
/// of their own) and those `words` knows nothing of. A row whose agent has said nothing takes its place among
/// the `rows` and has no words here. (The list puts the agents that ask you something first, wherever the order
/// has them: those rows show their requests, not their last words, so the rows after them are these or fewer.)
fn first_said(ids: &Ids<'_>, order: &[usize], marks: &Map<String, Value>, rows: usize, now: i64, words: impl Fn(&str) -> Option<String>) -> Vec<(usize, String)> {
    let mark = |at: usize| ids.all.get(at).and_then(|id| marks.get(*id));
    let away = |at: usize| mark(at).is_some_and(|m| m.get("archived") == Some(&Value::Bool(true)) || m.get("parked_until").and_then(Value::as_i64).is_some_and(|until| until > now));
    // A row the list shows as it opens, with what its agent last said.
    let shown = |at: usize| if away(at) { None } else { ids.all.get(at).and_then(|id| words(id)) };
    let pin = |at: usize| mark(at).and_then(|m| m.get("pinned")).and_then(Value::as_i64);
    let mut pinned: Vec<(i64, &str, usize)> = ids.all.iter().enumerate().filter_map(|(at, id)| Some((pin(at)?, *id, at))).collect();
    pinned.sort_unstable();
    let mut met = HashSet::new();
    let mut said = Vec::new();
    let mut taken = 0_usize;
    for at in pinned.into_iter().map(|(_, _, at)| at).chain(order.iter().copied()) {
        if taken >= rows {
            break;
        }
        if !met.insert(at) {
            continue;
        }
        let Some(text) = shown(at) else { continue };
        taken = taken.saturating_add(1);
        if !text.is_empty() {
            said.push((at, text));
        }
    }
    // The list's last row: the order's last that isn't one of your pins, which come first.
    if let Some((at, text)) = order.iter().rev().copied().filter(|at| pin(*at).is_none()).find_map(|at| shown(at).map(|text| (at, text)))
        && met.insert(at)
        && !text.is_empty()
    {
        said.push((at, text));
    }
    said
}

/// The snapshot `lean` holds, as it was but for what a lean one leaves out ([`WITHOUT`]), and the last words it
/// carries by agent: none when it isn't a lean snapshot. As the UI reads one (`ui/src/api/attention-lean.ts`),
/// here for the tests that hold [`lean`] to it.
pub fn whole(lean: &Value) -> Option<(Value, Map<String, Value>)> {
    let fields = lean.as_object()?;
    let listed = fields.get("ids")?;
    let n = listed.get("v")?.as_array()?.len();
    let ids: Vec<String> = columns::unpack_column(listed, n)?.iter().map(|id| id.as_str().map(str::to_owned)).collect::<Option<_>>()?;
    let named = |places: &Value| -> Option<Vec<&str>> {
        match places.as_u64() {
            Some(n) => ids.get(..usize::try_from(n).ok()?).map(|first| first.iter().map(String::as_str).collect()),
            None => places.as_array()?.iter().map(|at| ids.get(usize::try_from(at.as_u64()?).ok()?).map(String::as_str)).collect(),
        }
    };
    let mut whole = Map::new();
    let mut said = Map::new();
    for (key, value) in fields {
        let value = match key.as_str() {
            "ids" | "without" => continue,
            "said" => {
                for (at, text) in value.get("at")?.as_array()?.iter().zip(value.get("v")?.as_array()?) {
                    said.insert(ids.get(usize::try_from(at.as_u64()?).ok()?)?.clone(), text.clone());
                }
                continue;
            }
            "order" | "request_order" => json!(named(value)?),
            "pair" => match value.as_object() {
                Some(pair) => {
                    let mut pair = pair.clone();
                    for side in ["order_a", "order_b"] {
                        if let Some(shown) = value.get(side) {
                            pair.insert(side.to_owned(), json!(named(shown)?));
                        }
                    }
                    Value::Object(pair)
                }
                None => value.clone(),
            },
            "agents" => {
                let marks = columns::unpack(value)?;
                let whose = match value.get("at") {
                    Some(at) => named(at)?,
                    None => named(&json!(marks.len()))?,
                };
                if whose.len() != marks.len() {
                    return None;
                }
                Value::Object(whose.into_iter().map(str::to_owned).zip(marks.into_iter().map(Value::Object)).collect())
            }
            "archived" => Value::Array(columns::unpack(value)?.into_iter().map(Value::Object).collect()),
            _ => value.clone(),
        };
        whole.insert(key.clone(), value);
    }
    Some((Value::Object(whole), said))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A snapshot as `GET /api/harness/attention` answers one: six agents in the order, `c` pinned, `d` parked
    /// until 2,000, one archived (`z`, marked and named but not in the order), a mark for one that isn't a row
    /// (`s`, a subagent you looked at), a request from `e`, and the visit's two orders, the second the order's own.
    fn snapshot() -> Value {
        json!({
            "you": "ada",
            "agents": {
                "01a1dd4b70467679af99118ba53a4a5a": {"revision": 0, "rank": 3.75, "rank_reason": "2 interactions from you"},
                "01a1dd4b73c9728cba77f3dd42afc3db": {"revision": 0, "rank": 2.5, "rank_reason": "1 interactions from you"},
                "01a1dd4b8a1e7c33a5d09f6e2b7c4d1c": {"revision": 3, "rank": 2.25, "rank_reason": "1 interactions from you", "pinned": 1},
                "01a1dd4bf09172899e19421c5a82acdd": {"revision": 5, "rank": null, "rank_reason": "Added during this visit", "parked_until": 2000, "parked_at": 900, "reason": "Parked by you"},
                "01a1dd4c05d27e41b8f3a6c9d1e0f2ae": {"revision": 0, "rank": 1.0, "rank_reason": "1 interactions from you", "urgency": {"score": null, "stale": false, "known": false, "since": 950, "waited": null, "expected_wait": null, "observations": 0, "reason": "Oldest first"}},
                "01a1dd4c1bc77178a68be541fc9ab6ff": {"revision": 0, "rank": 0.5, "rank_reason": "1 interactions from you"},
                "01a1dd4c2f0a7d55b1c2d3e4f5a6b7c5": {"revision": 1, "updated_seen": 940},
                "01a1dd4c3a1b7e66c2d3e4f5a6b7c8dz": {"revision": 2, "archived": true, "archive_status": "archived"}
            },
            "order": ["01a1dd4b70467679af99118ba53a4a5a", "01a1dd4b73c9728cba77f3dd42afc3db", "01a1dd4b8a1e7c33a5d09f6e2b7c4d1c", "01a1dd4bf09172899e19421c5a82acdd", "01a1dd4c05d27e41b8f3a6c9d1e0f2ae", "01a1dd4c1bc77178a68be541fc9ab6ff"],
            "settings": {"sort": "learned", "auto_park_days": null, "archive_after_days": 7, "auto_archive": true, "park_later_minutes": 60, "park_evening_hour": 18, "park_morning_hour": 9},
            "settings_defaults": {"auto_park_days": 3},
            "archived": [{"id": "01a1dd4c3a1b7e66c2d3e4f5a6b7c8dz", "kind": "claude", "name": "Claude Code", "title": "An old one", "summary": "Put away", "status": "exited", "approvals": []}],
            "model": {"observations": 159, "ignored": 137, "parameters": [{"name": "decay", "value": 0.07, "standard_error": 0.28}]},
            "exposure": "visit-1",
            "pair": {
                "key": "visit-1",
                "order_a": ["01a1dd4b73c9728cba77f3dd42afc3db", "01a1dd4b70467679af99118ba53a4a5a", "01a1dd4c1bc77178a68be541fc9ab6ff", "01a1dd4b8a1e7c33a5d09f6e2b7c4d1c", "01a1dd4c05d27e41b8f3a6c9d1e0f2ae", "01a1dd4bf09172899e19421c5a82acdd", "01a1dd4c4b2c7f77d3e4f5a6b7c8d9eg"],
                "order_b": ["01a1dd4b70467679af99118ba53a4a5a", "01a1dd4b73c9728cba77f3dd42afc3db", "01a1dd4b8a1e7c33a5d09f6e2b7c4d1c", "01a1dd4bf09172899e19421c5a82acdd", "01a1dd4c05d27e41b8f3a6c9d1e0f2ae", "01a1dd4c1bc77178a68be541fc9ab6ff"],
                "arm": 1,
                "model_order": true
            },
            "examination": {"curves": [], "exposures": 0},
            "request_order": ["01a1dd4c05d27e41b8f3a6c9d1e0f2ae"],
            "urgency": {"requests": [], "learned_order": false, "reason": "Oldest first"}
        })
    }

    /// Each agent's last words, by the last character of its id: `a` and `c` have some, `b` has said nothing,
    /// `f` has ended (the list doesn't show it as it opens), and the rest have some too.
    fn words(id: &str) -> Option<String> {
        match id.chars().last()? {
            'b' => Some(String::new()),
            'f' => None,
            last => Some(format!("Done: {last}")),
        }
    }

    /// `snapshot` without what a lean one leaves out.
    fn without(snapshot: &Value) -> Option<Value> {
        let mut whole = snapshot.as_object()?.clone();
        for key in ["model", "examination", "urgency"] {
            whole.remove(key);
        }
        for mark in whole.get_mut("agents")?.as_object_mut()?.values_mut() {
            mark.as_object_mut()?.remove("rank");
        }
        Some(Value::Object(whole))
    }

    /// W21-R02: a lean snapshot read back is the snapshot but for what a lean one leaves out: every mark its
    /// agent's (those of agents the order doesn't have too), every order its agents in their places (one that
    /// names an agent nothing else does too), the archived as they were, and what it leaves out said.
    #[test]
    fn w21_r02_a_lean_snapshot_read_back_is_the_snapshot() -> anyhow::Result<()> {
        let snapshot = snapshot();
        let lean = lean(&snapshot, 0, 1_000, words).ok_or_else(|| anyhow::anyhow!("not a snapshot"))?;
        let (read, said) = whole(&lean).ok_or_else(|| anyhow::anyhow!("not a lean snapshot: {lean}"))?;
        assert_eq!(Some(read), without(&snapshot));
        anyhow::ensure!(said.is_empty() && lean.get("said").is_none(), "no last words unless they're asked for");
        assert_eq!(lean.get("without"), Some(&json!(WITHOUT)));
        // The order is its ids' first six, and the visit's second order is the order: both are said as six.
        assert_eq!(lean.get("order"), Some(&json!(6)));
        assert_eq!(lean.pointer("/pair/order_b"), Some(&json!(6)));
        assert_eq!(lean.pointer("/pair/order_a"), Some(&json!([1, 0, 5, 2, 4, 3, 8])));
        assert_eq!(lean.get("request_order"), Some(&json!([4])));
        // The marks are the order's six and two more: said by place only because they are.
        assert_eq!(lean.pointer("/agents/rows"), Some(&json!(8)));
        assert_eq!(lean.pointer("/agents/at"), None);
        anyhow::ensure!(serde_json::to_vec(&lean)?.len() < serde_json::to_vec(&snapshot)?.len(), "lean, it's shorter");
        // A snapshot with nothing in it, and one whose marks aren't its order's first: both read back.
        for snapshot in [json!({"you": "ada", "agents": {}, "order": [], "archived": [], "pair": null, "request_order": []}), json!({"agents": {"b": {"revision": 1}}, "order": ["a", "b"]})] {
            let lean = super::lean(&snapshot, 0, 0, words).ok_or_else(|| anyhow::anyhow!("not a snapshot: {snapshot}"))?;
            assert_eq!(whole(&lean).map(|(read, _)| read), Some(snapshot.clone()), "{lean}");
        }
        assert_eq!(super::lean(&json!({"order": []}), 0, 0, words), None, "no marks: not a snapshot");
        assert_eq!(whole(&snapshot), None, "a whole snapshot isn't a lean one");
        Ok(())
    }

    /// W21-R02: asked for the rows the list opens on, a lean snapshot carries their last words: your pins first,
    /// then the order's, each row taking a place whether it has said anything or not, and the list's last row,
    /// without the parked, the archived and the ended; a park that's over no longer keeps its agent out.
    #[test]
    fn w21_r02_a_lean_snapshot_carries_the_last_words_of_the_rows_the_list_opens_on() -> anyhow::Result<()> {
        let snapshot = snapshot();
        let said_by = |snapshot: &Value, rows: usize, now: i64, words: fn(&str) -> Option<String>| -> anyhow::Result<Vec<(String, String)>> {
            let lean = lean(snapshot, rows, now, words).ok_or_else(|| anyhow::anyhow!("not a snapshot"))?;
            let (_, said) = whole(&lean).ok_or_else(|| anyhow::anyhow!("not a lean snapshot: {lean}"))?;
            let mut said: Vec<(String, String)> = said.into_iter().map(|(id, text)| (id.chars().last().map(String::from).unwrap_or_default(), text.as_str().unwrap_or_default().to_owned())).collect();
            said.sort();
            Ok(said)
        };
        let said = |rows: usize, now: i64| said_by(&snapshot, rows, now, words);
        let of = |all: &[(&str, &str)]| all.iter().map(|(id, text)| ((*id).to_owned(), (*text).to_owned())).collect::<Vec<_>>();
        // One row: the pin, wherever the order has it; and the list's last row, `e` (`f`, after it, has ended).
        assert_eq!(said(1, 1_000)?, of(&[("c", "Done: c"), ("e", "Done: e")]));
        // Three: the pin, `a`, and `b`, which has said nothing; and the last.
        assert_eq!(said(3, 1_000)?, of(&[("a", "Done: a"), ("c", "Done: c"), ("e", "Done: e")]));
        // Every row there is: `d` is parked until 2,000 and `f` has ended; `z` is archived and `s` isn't in the order.
        // The last row is among them: said once.
        assert_eq!(said(100, 1_000)?, of(&[("a", "Done: a"), ("c", "Done: c"), ("e", "Done: e")]));
        // Once its park is over, `d` is a row again, in its place in the order: the fourth of the rows shown.
        assert_eq!(said(4, 2_000)?, of(&[("a", "Done: a"), ("c", "Done: c"), ("d", "Done: d"), ("e", "Done: e")]));
        // A last row that has said nothing has no words to carry; and a pin is never the last row, wherever the order has it.
        let quiet_last: fn(&str) -> Option<String> = |id| if id.ends_with('e') { Some(String::new()) } else { words(id) };
        assert_eq!(said_by(&snapshot, 1, 1_000, quiet_last)?, of(&[("c", "Done: c")]));
        let mut pinned_last = snapshot.clone();
        if let Some(mark) = pinned_last.pointer_mut("/agents/01a1dd4c05d27e41b8f3a6c9d1e0f2ae").and_then(Value::as_object_mut) {
            mark.insert("pinned".to_owned(), json!(2));
        }
        // `c` and `e` are pinned (the list's first two): with one row asked for, `c`; the last row is then `d`, parked: `b`, which has said nothing.
        assert_eq!(said_by(&pinned_last, 1, 1_000, words)?, of(&[("c", "Done: c")]));
        assert_eq!(said_by(&pinned_last, 1, 2_000, words)?, of(&[("c", "Done: c"), ("d", "Done: d")]));
        Ok(())
    }

    /// W21-R02: the fixture the UI's reader is tested on is what this writes of this snapshot: written by this
    /// test when `KILN_WRITE_FIXTURES` is set, compared with it otherwise, so the two stay each other's.
    #[test]
    fn w21_r02_the_uis_lean_snapshot_fixture_is_what_this_writes() -> anyhow::Result<()> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ui/tests/fixtures/attention-lean.json");
        let snapshot = snapshot();
        let lean = lean(&snapshot, 3, 1_000, words).ok_or_else(|| anyhow::anyhow!("not a snapshot"))?;
        let (read, said) = whole(&lean).ok_or_else(|| anyhow::anyhow!("not a lean snapshot: {lean}"))?;
        let fixture = json!({ "lean": lean, "snapshot": read, "said": said });
        if std::env::var_os("KILN_WRITE_FIXTURES").is_some() {
            std::fs::write(&path, format!("{}\n", serde_json::to_string_pretty(&fixture)?))?;
        }
        let kept: Value = serde_json::from_str(&std::fs::read_to_string(&path).map_err(|e| anyhow::anyhow!("{}: {e} (write it with KILN_WRITE_FIXTURES=1)", path.display()))?)?;
        assert_eq!(kept, fixture, "ui/tests/fixtures/attention-lean.json isn't what `lean` writes of this snapshot: write it again with KILN_WRITE_FIXTURES=1");
        Ok(())
    }
}
