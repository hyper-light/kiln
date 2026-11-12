//! A runner's state too large for its frame comes as a bounded view (CP-T3.2, `super::state_view`): its largest fields
//! left out, each named in `view.deferred` at the revision its runner has it. The server fills them in itself, so its
//! copy, the pages it feeds, its store and the rest of kiln keep a session's whole state: a left-out field keeps the
//! value the copy had for it ([`merged`]) while the server reads the runner's, part by part ([`fetch`]), and takes it in
//! once read ([`filled`]). A page never sees a field left out (`super::state_doc` keeps `view` off what it's sent).

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use futures::channel::oneshot;
use futures::future::{Either, select};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{Map, Value};

use super::core::Update;
use super::model::{Command, State};
use super::registry::ANSWER_WAIT;
use super::state_reads::Failure;
use super::state_view::{self, Counts, Error, Kind, Metadata, Read, Reply, Step};
use crate::hub::Hub;

/// Most reads one pass of a session's fetch makes, each a frame at most: past them it gives up the pass.
const MOST_READS: usize = 4096;
/// How deep a value's parts may nest.
const DEEPEST: usize = 64;
/// Passes in a row that fail before a session's fetch stops; its runner's next view starts it again.
const TRIES: u32 = 6;

/// Where a session's left-out fields are read from: the runner that sent its view.
#[derive(Clone)]
pub struct Source {
    pub peer: hyper_transport::PeerId,
    pub agent: Arc<str>,
    pub shipped: bool,
}

/// What each session's copy waits for, and the reads the server asked for itself.
#[derive(Default)]
pub struct Fills(Mutex<Held>);

#[derive(Default)]
struct Held {
    /// By session: its copy's left-out fields, each at its runner's revision, and where to read them.
    wanted: HashMap<String, Wanted>,
    /// The sessions a fetch runs for.
    running: HashSet<String>,
    /// The server's own reads waiting for their answers, by client id.
    asked: HashMap<String, oneshot::Sender<Value>>,
}

#[derive(Clone)]
struct Wanted {
    fields: BTreeMap<String, (String, Kind)>,
    source: Source,
}

impl Fills {
    /// `state` is its session's copy now: what it still lacks is fetched (a fetch started for it where none runs), and
    /// nothing is once it lacks nothing.
    fn want(&self, hub: &Arc<Hub>, session: &str, state: &State, source: Source) {
        let fields: BTreeMap<String, (String, Kind)> = state
            .view
            .iter()
            .flat_map(|v| v.deferred.iter().filter_map(|(f, d)| v.revisions.get(f).map(|r| (f.clone(), (r.clone(), d.kind)))))
            .collect();
        let mut held = self.0.lock();
        if fields.is_empty() {
            held.wanted.remove(session);
            return;
        }
        held.wanted.insert(session.to_owned(), Wanted { fields, source });
        if held.running.insert(session.to_owned()) {
            drop(held);
            let (hub, session) = (Arc::clone(hub), session.to_owned());
            drop(crate::rt::spawn(fetch(hub, session)));
        }
    }

    /// The session is gone: nothing more of it is fetched.
    fn forget(&self, session: &str) {
        self.0.lock().wanted.remove(session);
    }

    /// What a session's fetch reads next; none, and it ends (a later [`Fills::want`] starts another).
    fn next(&self, session: &str) -> Option<Wanted> {
        let mut held = self.0.lock();
        match held.wanted.get(session) {
            Some(w) => Some(w.clone()),
            None => {
                held.running.remove(session);
                None
            }
        }
    }

    /// The fields a pass read (or found changed since), at the revisions it read them: not wanted again unless a newer
    /// view says so.
    fn done(&self, session: &str, read: &[(String, String)]) {
        let mut held = self.0.lock();
        let Some(w) = held.wanted.get_mut(session) else { return };
        for (field, revision) in read {
            if w.fields.get(field).is_some_and(|(r, _)| r == revision) {
                w.fields.remove(field);
            }
        }
        if w.fields.is_empty() {
            held.wanted.remove(session);
        }
    }

    /// A session's fetch gives up: its runner's next view starts another.
    fn give_up(&self, session: &str) {
        let mut held = self.0.lock();
        held.wanted.remove(session);
        held.running.remove(session);
    }

    /// A runner's answer to one of the server's own reads goes to that read, not to the session's viewers: back, an
    /// update that isn't one.
    pub fn answered(&self, update: Box<Update>) -> Option<Box<Update>> {
        let Update::Reply { client, .. } = &*update else { return Some(update) };
        let Some(tx) = self.0.lock().asked.remove(client) else { return Some(update) };
        if let Update::Reply { value, .. } = *update {
            let _ = tx.send(value);
        }
        None
    }
}

/// A session's update from its runner, or the fields the server read for it (`fill`), as its copy takes it: a state with
/// fields left out keeps the copy's for them and has them fetched; fields read come in where the copy still lacks them at
/// the revision read. None: a fill nothing of which is wanted any more.
pub(crate) fn taken(hub: &Arc<Hub>, last: Option<Cow<'_, State>>, session: &str, update: Update, fill: bool, source: Source) -> Option<Update> {
    // Only the machine a session runs on speaks for it: the rest of its word is the copy's judge's to drop, and what was
    // read from it is dropped here (never taken for a whole state).
    let runs = hub.harness.runs_on(session, &source.agent);
    match update {
        Update::State { .. } if fill && !runs => None,
        Update::State { state } if runs => {
            let state = if fill { filled(last.as_deref(), &state)? } else { merged(last.as_deref(), state) };
            hub.harness.fills.want(hub, session, &state, source);
            Some(Update::State { state })
        }
        Update::Closed => {
            hub.harness.fills.forget(session);
            Some(Update::Closed)
        }
        u => Some(u),
    }
}

/// `last`'s revision of `field` where its value is that revision's (its runner's word for one it didn't leave out, else
/// kiln's own hash of it), none where it's one the copy still waits for.
fn current(last: &State, field: &str) -> Option<String> {
    match &last.view {
        Some(v) if v.deferred.contains_key(field) => None,
        Some(v) => v.revisions.get(field).cloned().or_else(|| state_view::field_revision(last, field).ok()),
        None => state_view::field_revision(last, field).ok(),
    }
}

/// What a view still leaves out: none, it leaves out nothing (and its queue's index goes with its queue).
fn rest(mut view: Metadata) -> Option<Metadata> {
    if !view.deferred.contains_key("queue") {
        view.queue_index = None;
    }
    (!view.deferred.is_empty()).then_some(view)
}

/// A runner's state `next` as the copy takes it after `last`: a field its view left out keeps `last`'s value, and stays
/// wanted unless that's of the revision the runner has. A whole state is taken as it is.
pub(crate) fn merged(last: Option<&State>, mut next: Box<State>) -> Box<State> {
    let Some(view) = next.view.take() else { return next };
    if view.deferred.is_empty() {
        return next;
    }
    let Some(last) = last else {
        next.view = Some(view);
        return next;
    };
    let kept: BTreeSet<String> = view.deferred.keys().cloned().collect();
    let mut still = view.clone();
    still.deferred.retain(|f, _| current(last, f).as_ref() != view.revisions.get(f));
    match with_fields(&next, last, &kept) {
        Some(mut state) => {
            state.view = rest(still);
            Box::new(state)
        }
        None => {
            tracing::warn!("a session's state view couldn't be filled in from its copy: taken as it came");
            next.view = Some(view);
            next
        }
    }
}

/// `to` with `from`'s fields named in `fields` (where `from` leaves one out of its JSON, an empty one, `to`'s is left
/// out too). None: it couldn't be made.
fn with_fields(to: &State, from: &State, fields: &BTreeSet<String>) -> Option<State> {
    let Value::Object(mut doc) = serde_json::to_value(to).ok()? else { return None };
    let mut got = state_view::fields_of(from, fields).ok()?;
    for f in fields {
        match got.remove(f) {
            Some(v) => doc.insert(f.clone(), v),
            None => doc.remove(f),
        };
    }
    serde_json::from_value(Value::Object(doc)).ok()
}

/// The fields `fill` read, taken into `last` where it still lacks them at the revision read. None: none of them.
pub(crate) fn filled(last: Option<&State>, fill: &State) -> Option<Box<State>> {
    let last = last?;
    let had = last.view.as_ref()?;
    let read = fill.view.as_ref()?;
    let taken: BTreeSet<String> = read.revisions.iter().filter(|(f, r)| had.deferred.contains_key(*f) && had.revisions.get(*f) == Some(*r)).map(|(f, _)| f.clone()).collect();
    if taken.is_empty() {
        return None;
    }
    let mut state = with_fields(last, fill, &taken)?;
    let mut view = had.clone();
    view.deferred.retain(|f, _| !taken.contains(f));
    state.view = rest(view);
    Some(Box::new(state))
}

/// The fields read, as a fill of them at the revisions read.
fn fill_of(got: Map<String, Value>, revisions: BTreeMap<String, String>) -> Option<Box<State>> {
    let Value::Object(mut doc) = serde_json::to_value(State::default()).ok()? else { return None };
    doc.extend(got);
    let mut state: State = serde_json::from_value(Value::Object(doc)).ok()?;
    state.view = Some(Metadata { revision: String::new(), revisions, deferred: BTreeMap::new(), counts: Counts::default(), queue_index: None });
    Some(Box::new(state))
}

/// Reads a session's left-out fields from its runner, a pass at a time, while its copy lacks any: each read whole is
/// handed to the copy's owner as a fill. A pass that fails is tried again later, [`TRIES`] in a row at most.
async fn fetch(hub: Arc<Hub>, session: String) {
    let mut failed = 0u32;
    while let Some(want) = hub.harness.fills.next(&session) {
        let owner = Owner { hub: &hub, session: &session, reads: AtomicUsize::new(0) };
        let (mut got, mut revisions, mut done) = (Map::new(), BTreeMap::new(), Vec::new());
        let mut short = false;
        for (field, (revision, kind)) in &want.fields {
            match whole(&owner, field, revision, Vec::new(), *kind, 0).await {
                Ok(v) => {
                    got.insert(field.clone(), v);
                    revisions.insert(field.clone(), revision.clone());
                    done.push((field.clone(), revision.clone()));
                }
                // Changed since: the view that says so wants it at its new revision.
                Err(Failure::Source(Error::Stale)) => done.push((field.clone(), revision.clone())),
                Err(e) => {
                    tracing::debug!(%session, %field, error = ?e, "a session's left-out field couldn't be read from its runner");
                    short = true;
                }
            }
        }
        if !got.is_empty() {
            match fill_of(got, revisions) {
                Some(state) => {
                    let r = super::copies::RunnerUpdate {
                        peer: want.source.peer,
                        agent: Arc::clone(&want.source.agent),
                        session: session.clone(),
                        update: Update::State { state },
                        shipped: want.source.shipped,
                        arrived: None,
                        handed: Some(Instant::now()),
                        taken: None,
                        fill: true,
                    };
                    hub.harness.from_runner(r).await;
                }
                None => tracing::warn!(%session, "a session's fields read from its runner didn't make a state"),
            }
        }
        hub.harness.fills.done(&session, &done);
        if !short {
            failed = 0;
            continue;
        }
        failed += 1;
        if failed >= TRIES {
            tracing::warn!(%session, "a session's left-out fields couldn't be read from its runner: its next state tries again");
            hub.harness.fills.give_up(&session);
            return;
        }
        hub.clock.sleep(Duration::from_secs(1 << failed)).await;
    }
}

type Pending<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What a field's parts are read from.
trait Reader: Sync {
    fn read(&self, read: Read) -> Pending<'_, Result<Reply, Failure>>;
}

/// A session's runner, read through its session's commands, its answers taken off its updates ([`Fills::answered`]).
struct Owner<'a> {
    hub: &'a Arc<Hub>,
    session: &'a str,
    reads: AtomicUsize,
}

impl Reader for Owner<'_> {
    fn read(&self, read: Read) -> Pending<'_, Result<Reply, Failure>> {
        Box::pin(async move {
            if self.reads.fetch_add(1, Ordering::Relaxed) >= MOST_READS {
                return Err(Failure::Source(Error::Budget));
            }
            ask(self.hub, self.session, read).await
        })
    }
}

/// The server's own read of `session`'s state from its runner, answered to it alone, in [`ANSWER_WAIT`].
async fn ask(hub: &Arc<Hub>, session: &str, read: Read) -> Result<Reply, Failure> {
    let expected = read.revision.clone();
    let client = crate::util::new_id();
    let (tx, rx) = oneshot::channel();
    hub.harness.fills.0.lock().asked.insert(client.clone(), tx);
    let answer = async {
        if !hub.harness.command(hub, session, Command::StateRead { client: client.clone(), read }).await.went() {
            return Err(Failure::Unreachable);
        }
        rx.await.map_err(|_| Failure::FeedLost)
    };
    let deadline = hub.clock.sleep(ANSWER_WAIT);
    futures::pin_mut!(answer, deadline);
    let answer = match select(answer, deadline).await {
        Either::Left((answer, _)) => answer,
        Either::Right(_) => Err(Failure::TimedOut),
    };
    hub.harness.fills.0.lock().asked.remove(&client);
    reply(answer?, &expected)
}

/// A runner's answer to a read: its reply at the revision asked, or what it refused it for.
fn reply(value: Value, revision: &str) -> Result<Reply, Failure> {
    #[derive(Deserialize)]
    struct Refusal {
        code: Error,
    }
    if let Some(error) = value.get("error") {
        return Err(serde_json::from_value::<Refusal>(error.clone()).map_or(Failure::Malformed, |r| Failure::Source(r.code)));
    }
    let reply: Reply = serde_json::from_value(value.get("result").cloned().ok_or(Failure::Malformed)?).map_err(|_| Failure::Malformed)?;
    let (Reply::Page { revision: r, .. } | Reply::Text { revision: r, .. } | Reply::Value { revision: r, .. } | Reply::Node { revision: r, .. }) = &reply;
    if r != revision {
        return Err(Failure::Malformed);
    }
    Ok(reply)
}

/// Field `field`'s value at `path`, of kind `kind`, read whole from `from`: its pages, a text's parts, and each part
/// too large for a page read on its own, as `serde_json` writes the value.
fn whole<'a>(from: &'a dyn Reader, field: &'a str, revision: &'a str, path: Vec<Step>, kind: Kind, depth: usize) -> Pending<'a, Result<Value, Failure>> {
    Box::pin(async move {
        if depth > DEEPEST {
            return Err(Failure::Source(Error::Budget));
        }
        let at = |offset: usize| Read { revision: revision.to_owned(), field: field.to_owned(), path: path.clone(), offset };
        match from.read(at(0)).await? {
            Reply::Value { value, .. } => Ok(value),
            // A single value larger than a frame: there's no reading it.
            Reply::Node { .. } => Err(Failure::Source(Error::Budget)),
            Reply::Text { text, next, .. } => {
                let mut whole = text;
                let mut next = next;
                while let Some(offset) = next {
                    let Reply::Text { text, next: after, .. } = from.read(at(offset)).await? else { return Err(Failure::Malformed) };
                    if text.is_empty() || after.is_some_and(|n| n <= offset) {
                        return Err(Failure::Malformed);
                    }
                    whole.push_str(&text);
                    next = after;
                }
                Ok(Value::String(whole))
            }
            Reply::Page { entries, next, .. } => {
                let mut all = Vec::new();
                let (mut entries, mut next, mut offset) = (entries, next, 0);
                loop {
                    for e in entries {
                        let v = match (e.value, e.node) {
                            (Some(v), None) => v,
                            (None, Some(node)) => {
                                let mut inner = path.clone();
                                inner.push(e.key.clone());
                                whole(from, field, revision, inner, node.kind, depth + 1).await?
                            }
                            _ => return Err(Failure::Malformed),
                        };
                        all.push((e.key, v));
                    }
                    match next {
                        None => break,
                        Some(n) if n > offset => {
                            offset = n;
                            let Reply::Page { entries: more, next: after, .. } = from.read(at(offset)).await? else { return Err(Failure::Malformed) };
                            (entries, next) = (more, after);
                        }
                        Some(_) => return Err(Failure::Malformed),
                    }
                }
                built(all, kind)
            }
        }
    })
}

/// A page's entries as the value they make: an object's by key; else, by index, an array's elements or a map's entries
/// (`state_view` lists a map's as `{"key", "value"}` each).
fn built(entries: Vec<(Step, Value)>, kind: Kind) -> Result<Value, Failure> {
    if entries.first().is_some_and(|(k, _)| matches!(k, Step::Key { .. })) {
        let mut object = Map::new();
        for (k, v) in entries {
            let Step::Key { key } = k else { return Err(Failure::Malformed) };
            object.insert(key, v);
        }
        return Ok(Value::Object(object));
    }
    let mut out = Vec::with_capacity(entries.len());
    for (i, (k, v)) in entries.into_iter().enumerate() {
        if k != (Step::Index { index: i }) {
            return Err(Failure::Malformed);
        }
        out.push(v);
    }
    match kind {
        Kind::Array => Ok(Value::Array(out)),
        Kind::Object => {
            let mut object = Map::new();
            for entry in out {
                let Value::Object(mut entry) = entry else { return Err(Failure::Malformed) };
                let key = match entry.remove("key") {
                    Some(Value::String(s)) => s,
                    Some(Value::Number(n)) => n.to_string(),
                    Some(Value::Bool(b)) => b.to_string(),
                    _ => return Err(Failure::Malformed),
                };
                object.insert(key, entry.remove("value").ok_or(Failure::Malformed)?);
            }
            Ok(Value::Object(object))
        }
        Kind::Text | Kind::Value => Err(Failure::Malformed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::model::{QueueItem, Status};

    /// A state at hand, read as its runner reads its own (`state_view::read`) within `budget`.
    struct Local<'a> {
        state: &'a State,
        budget: usize,
    }

    impl Reader for Local<'_> {
        fn read(&self, read: Read) -> Pending<'_, Result<Reply, Failure>> {
            let reply = state_view::read(self.state, &read, self.budget).map_err(Failure::Source);
            Box::pin(async move { reply })
        }
    }

    fn queued(i: usize, text: String) -> QueueItem {
        QueueItem { id: format!("q{i}"), text, at: i64::try_from(i).unwrap_or_default(), attachments: Vec::new(), overrides: None, reply_to: None }
    }

    /// A state larger than a frame: a long queue (one message itself larger than a frame, with a multi-byte character
    /// across its parts' edges), a summary of its own past a frame, and the rest as a session has them.
    fn large() -> State {
        let mut queue: Vec<QueueItem> = (0..300).map(|i| queued(i, format!("message {i}: ") + &"peer report line ".repeat(120))).collect();
        queue.push(queued(300, "é".repeat(state_view::FRAME)));
        State { status: Status::Running, epoch: 3, queue, summary: "s".repeat(state_view::FRAME + 17), title: "big".into(), ..State::default() }
    }

    fn read_whole(state: &State, budget: usize, field: &str) -> anyhow::Result<Value> {
        let view = state_view::view(state, budget)?;
        let revision = view.revisions.get(field).ok_or_else(|| anyhow::anyhow!("no revision for {field}"))?;
        let kind = state_view::fields_of(state, &BTreeSet::from([field.to_owned()]))?.get(field).map_or(Kind::Value, |v| match v {
            Value::Array(_) => Kind::Array,
            Value::Object(_) => Kind::Object,
            Value::String(_) => Kind::Text,
            _ => Kind::Value,
        });
        let local = Local { state, budget };
        futures::executor::block_on(whole(&local, field, revision, Vec::new(), kind, 0)).map_err(|e| anyhow::anyhow!("{e:?}"))
    }

    /// CP-T3.2: every field a view leaves out reads back whole, exactly as `serde_json` writes it, at a frame's budget
    /// and at a budget small enough that each message, and the summary, come in parts.
    #[test]
    fn cp_t3_2_left_out_fields_read_back_whole() -> anyhow::Result<()> {
        let state = large();
        let want = serde_json::to_value(&state)?;
        for budget in [state_view::FRAME - 2048, 4096] {
            let view = state_view::view(&state, budget)?;
            anyhow::ensure!(view.deferred.contains_key("queue") && view.deferred.contains_key("summary"), "{:?}", view.deferred);
            for field in view.deferred.keys() {
                let got = read_whole(&state, budget, field)?;
                anyhow::ensure!(Some(&got) == want.get(field), "{field} at a budget of {budget} read back otherwise");
            }
        }
        Ok(())
    }

    /// A map's entries, an object's fields and a struct variant read back as `serde_json` writes them.
    #[test]
    fn cp_t3_2_maps_and_objects_read_back_whole() -> anyhow::Result<()> {
        let mut state = State { status: Status::Idle, ..State::default() };
        let big: Map<String, Value> = (0..400).map(|i| (format!("key-{i}"), serde_json::json!({"n": i, "text": "v".repeat(300)}))).collect();
        state.control_values = big.into_iter().collect();
        let want = serde_json::to_value(&state)?;
        let got = read_whole(&state, 4096, "control_values")?;
        anyhow::ensure!(Some(&got) == want.get("control_values"));
        Ok(())
    }

    /// The copy keeps what it had for a field a view leaves out: wanted still where it changed, not where it's the
    /// revision the runner has. A fill takes in what's wanted at its revision, and nothing else; once nothing is left
    /// out, the state has no view.
    #[test]
    fn cp_t3_2_left_out_fields_keep_the_copys_until_read() -> anyhow::Result<()> {
        let before = large();
        let last = merged(None, Box::new(before.clone()));
        anyhow::ensure!(last.view.is_none() && last.queue == before.queue, "a whole state is taken as it is");
        // The queue changes; the summary doesn't.
        let mut now = before.clone();
        now.queue.push(queued(301, "one more".into()));
        now.status = Status::Waiting;
        let view: State = serde_json::from_value(state_view::view(&now, state_view::FRAME - 2048)?.into_state()?)?;
        anyhow::ensure!(view.queue.is_empty() && view.summary.is_empty(), "the view leaves both out");
        let took = merged(Some(&last), Box::new(view.clone()));
        anyhow::ensure!(took.status == Status::Waiting, "a view's own fields count");
        anyhow::ensure!(took.queue == before.queue && took.summary == before.summary, "a left-out field keeps the copy's");
        let still = took.view.as_ref().ok_or_else(|| anyhow::anyhow!("the queue is still wanted"))?;
        anyhow::ensure!(still.deferred.contains_key("queue") && !still.deferred.contains_key("summary"), "{:?}", still.deferred);
        anyhow::ensure!(still.queue_index.is_some(), "a queue still wanted keeps its index");
        // A fill of the queue at the revision wanted is taken; at another, it isn't.
        let revision = still.revisions.get("queue").cloned().unwrap_or_default();
        let mut got = Map::new();
        got.insert("queue".into(), serde_json::to_value(&now.queue)?);
        let stale = fill_of(got.clone(), BTreeMap::from([("queue".to_owned(), "another".to_owned())])).ok_or_else(|| anyhow::anyhow!("no fill"))?;
        anyhow::ensure!(filled(Some(&took), &stale).is_none(), "a fill of another revision is dropped");
        let fill = fill_of(got, BTreeMap::from([("queue".to_owned(), revision)])).ok_or_else(|| anyhow::anyhow!("no fill"))?;
        let whole = filled(Some(&took), &fill).ok_or_else(|| anyhow::anyhow!("the fill wasn't taken"))?;
        anyhow::ensure!(whole.queue == now.queue && whole.status == Status::Waiting && whole.summary == now.summary);
        anyhow::ensure!(whole.view.is_none(), "nothing left out: no view");
        // A view after a whole copy whose left-out fields didn't change wants nothing.
        let again: State = serde_json::from_value(state_view::view(&now, state_view::FRAME - 2048)?.into_state()?)?;
        let same = merged(Some(&whole), Box::new(again));
        anyhow::ensure!(same.view.is_none() && same.queue == now.queue, "{:?}", same.view.map(|v| v.deferred));
        Ok(())
    }

    /// A fill of a field the copy no longer lacks, or before it had any state, is dropped; a view before any state keeps
    /// every field it left out wanted.
    #[test]
    fn cp_t3_2_fills_only_what_is_wanted() -> anyhow::Result<()> {
        let state = large();
        let view: State = serde_json::from_value(state_view::view(&state, state_view::FRAME - 2048)?.into_state()?)?;
        let first = merged(None, Box::new(view.clone()));
        let wanted = first.view.as_ref().map(|v| v.deferred.keys().cloned().collect::<Vec<_>>()).unwrap_or_default();
        anyhow::ensure!(wanted.contains(&"queue".to_owned()), "{wanted:?}");
        let mut got = Map::new();
        got.insert("summary".into(), Value::String(state.summary.clone()));
        let revision = view.view.as_ref().and_then(|v| v.revisions.get("summary").cloned()).unwrap_or_default();
        let fill = fill_of(got, BTreeMap::from([("summary".to_owned(), revision)])).ok_or_else(|| anyhow::anyhow!("no fill"))?;
        anyhow::ensure!(filled(None, &fill).is_none(), "no copy, nothing to fill");
        let whole = merged(None, Box::new(state.clone()));
        anyhow::ensure!(filled(Some(&whole), &fill).is_none(), "a whole copy wants nothing");
        Ok(())
    }

    /// A runner's refusal reads as the failure it says; an answer at another revision is malformed.
    #[test]
    fn cp_t3_2_answers_read_as_said() {
        assert_eq!(reply(serde_json::json!({"error": {"code": "stale"}}), "r").err(), Some(Failure::Source(Error::Stale)));
        let page = serde_json::json!({"result": {"type": "page", "revision": "other", "entries": [], "next": null}});
        assert_eq!(reply(page, "r").err(), Some(Failure::Malformed));
        let ok = serde_json::json!({"result": {"type": "value", "revision": "r", "value": 1}});
        assert!(matches!(reply(ok, "r"), Ok(Reply::Value { .. })));
    }
}
