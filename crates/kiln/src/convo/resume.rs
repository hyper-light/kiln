//! A session going on on another machine from the server's store (`docs/conversations.md`,
//! "Resuming on another machine"): its machine is gone, so the package a move makes there
//! (`harness::moving`) is made here instead, from what the store kept: what the session is, its
//! state, its timeline, and its harness's own record of the conversation.

use super::store::Store;
use crate::harness::core::MAX_EVENTS;
use crate::harness::kinds::{Kind, Protocol};
use crate::harness::model::{Body, Event, State};
use crate::harness::moving::{self, Carry, Package};
use crate::harness::session::{Info, Spec};

/// Lines of a record read from the store at once.
const PAGE: usize = 4096;
/// Events read at once looking for the ids of the messages a session was sent.
const IDS_PAGE: usize = 1000;
/// Most events looked at for them (a session sent few messages among very many events carries
/// fewer).
const IDS_LOOK_MAX: usize = 200_000;

/// Every line the store has of `session`'s record from `source`, in order.
fn lines_of(store: &Store, session: &str, source: &str) -> anyhow::Result<Vec<String>> {
    let mut out = Vec::new();
    loop {
        let from = out.len() as u64;
        let page = store.record(session, source, from, PAGE)?;
        let n = page.len();
        for (i, (num, line)) in page.into_iter().enumerate() {
            // Numbered from 0 without gaps: a gap is a line the store never got.
            if num != from + i as u64 {
                anyhow::bail!("the store's copy of {source} is missing line {}", from + i as u64);
            }
            out.push(line);
        }
        if n < PAGE {
            return Ok(out);
        }
    }
}

/// The ids of the newest messages `session` was sent, as the store's timeline of it shows them (at
/// most [`crate::harness::inbox::KEEP`]), the oldest of them first: what its inbox took, as near as
/// the server knows (the machine it was on is gone, and its inbox with it). Blocking.
fn sent_ids(store: &Store, session: &str) -> anyhow::Result<Vec<String>> {
    let most = usize::try_from(crate::harness::inbox::KEEP).unwrap_or(usize::MAX);
    let (mut ids, mut before, mut looked) = (Vec::new(), None, 0usize);
    while ids.len() < most && looked < IDS_LOOK_MAX {
        let page = store.events(session, before, IDS_PAGE)?;
        let Some(first) = page.first() else { break };
        before = Some(first.seq);
        looked = looked.saturating_add(page.len());
        for row in page.iter().rev().filter(|r| r.kind == "user") {
            if ids.len() >= most {
                break;
            }
            if let Ok(Event { body: Body::User { client_id: Some(id), .. }, .. }) = serde_json::from_str::<Event>(&row.body) {
                ids.push(id);
            }
        }
        if page.len() < IDS_PAGE {
            break;
        }
    }
    ids.reverse();
    Ok(ids)
}

/// The harness's record of `session` as the harness on the machine it goes to takes it; `events`,
/// kiln's timeline, for a harness whose own record kiln doesn't keep.
fn carry(store: &Store, session: &str, kind: &Kind, harness_session: Option<&str>, events: &[Event], note: &str, in_place: bool) -> anyhow::Result<Carry> {
    // Resumed in place (a crashed session, on its own machine): the harness's own record of the
    // conversation is there, so every harness that keeps one goes on natively by its id; an ACP
    // agent has the words too, for when it turns out not to load sessions (Ada, 2026-11-13).
    if in_place
        && let Some(own) = harness_session.filter(|h| !h.is_empty())
        && kind.protocol != Protocol::Terminal
    {
        let words = match (kind.protocol, moving::words(events, note)) {
            (Protocol::Acp(_), Carry::Words { text }) => Some(text),
            _ => None,
        };
        return Ok(Carry::Native { session: own.to_owned(), words });
    }
    if matches!(kind.protocol, Protocol::Acp(_) | Protocol::AgyStreamJson | Protocol::PiRpc) {
        return Ok(moving::words(events, note));
    }
    let Some(own) = harness_session.filter(|h| !h.is_empty()) else { return Ok(Carry::Nothing) };
    match kind.protocol {
        Protocol::ClaudeStreamJson => {
            let lines = lines_of(store, session, own)?;
            if lines.is_empty() {
                anyhow::bail!("the server has no copy of Claude Code's record of this conversation (its machine never sent it)");
            }
            Ok(Carry::ClaudeLines { session: own.to_owned(), lines })
        }
        Protocol::CodexAppServer => {
            let mut files = Vec::new();
            for m in store.marks(session)? {
                files.push(lines_of(store, session, &m.source)?);
            }
            let conv = crate::harness::handoff::codex_of_lines(&files, own).ok_or_else(|| anyhow::anyhow!("the server has no copy of Codex's record of this thread (its machine never sent it)"))?;
            Ok(Carry::Seed { items: crate::harness::handoff::to_codex(&conv, Some(&format!("<kiln_handoff>\n{note}\n</kiln_handoff>"))) })
        }
        Protocol::Acp(_) | Protocol::AgyStreamJson | Protocol::PiRpc => Ok(moving::words(events, note)),
        Protocol::Terminal => anyhow::bail!("{}'s sessions can't go on on another machine yet", kind.name),
    }
}

/// The package for session `id`, to go on on another machine, in folder `cwd` (the one it worked in
/// when None), made from what `store` kept. `from` names the machine it was on; `shown`, the names
/// people see of that machine and the one it goes to, for what the conversation is told. Blocking.
pub fn package(store: &Store, id: &str, from: &str, cwd: Option<&str>, shown: (&str, &str)) -> anyhow::Result<Package> {
    package_for(store, id, from, cwd, shown, false)
}

/// [`package`], `in_place` when it goes on on the machine it was on (a crashed session resumed
/// there): natively, from its harness's own record there ([`Carry::Native`]).
pub fn package_for(store: &Store, id: &str, from: &str, cwd: Option<&str>, shown: (&str, &str), in_place: bool) -> anyhow::Result<Package> {
    let kept = store.kept(id, MAX_EVENTS)?.ok_or_else(|| anyhow::anyhow!("the server's store has nothing of this session"))?;
    let info: Info = kept.info.as_deref().and_then(|j| serde_json::from_str(j).ok()).ok_or_else(|| anyhow::anyhow!("the server's store doesn't say what this session is"))?;
    let state: State = kept.state.as_deref().and_then(|j| serde_json::from_str(j).ok()).unwrap_or_default();
    anyhow::ensure!(
        !state.view.as_ref().is_some_and(|view| !view.deferred.is_empty()),
        "the server's store has an incomplete state view; resuming requires the owner's full state record"
    );
    let events: Vec<Event> = kept.events.iter().filter_map(|e| serde_json::from_str(&e.body).ok()).collect();
    let kind = Kind::get(&info.kind).ok_or_else(|| anyhow::anyhow!("an unknown harness: {}", info.kind))?;
    let from_cwd = if state.cwd.is_empty() { info.cwd.clone() } else { state.cwd.clone() };
    let goes_on_in = cwd.map(str::trim).filter(|c| !c.is_empty()).map_or_else(|| from_cwd.clone(), str::to_owned);
    let own = state.harness_session.clone().or_else(|| info.harness_session.clone());
    let (from_shown, to_shown) = shown;
    let note = if in_place {
        format!("This conversation went on on {to_shown} after its agent's process there was lost: it was kept on the kiln server.")
    } else {
        format!("This conversation went on from {from_shown} (where it worked in {from_cwd}), which is gone, to {to_shown} (it works in {goes_on_in} now). It was kept on the kiln server.")
    };
    let carry = carry(store, id, kind, own.as_deref(), &events, &note, in_place)?;
    // What starts it there: its settings as last chosen. Its environment was its machine's, and
    // stays there (the machine it goes to runs it with its own).
    let non_empty = |s: &str| (!s.is_empty()).then(|| s.to_owned());
    let spec = Spec {
        kind: kind.id.to_owned(),
        name: non_empty(&info.name),
        model: non_empty(&state.model),
        mode: non_empty(&state.mode),
        effort: non_empty(&state.effort),
        reach: state.reach.clone(),
        // Who started it, and the plan run it works for, go along; so does a ceiling it had.
        parent: info.parent.clone(),
        plan_run: info.plan_run.clone(),
        allow_bypass: !info.no_bypass,
        ..Spec::default()
    };
    let mut pkg = moving::pack(id, spec, &goes_on_in, from, &from_cwd, carry, events, state);
    // The messages it was sent go with it, so one sent again after it goes on is answered there.
    pkg.inbox = sent_ids(store, id).unwrap_or_else(|e| {
        tracing::warn!(target: "kiln.sends", event = "carry_failed", session = %id, error = %format!("{e:#}"), "the ids of the messages a session was sent couldn't be read from the store: those its timeline shows go with it");
        Vec::new()
    });
    Ok(pkg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convo::rows::{RecordRow, Row};
    use crate::harness::core::Update;
    use crate::harness::model::Body;

    /// What a session is, as kiln makes it from a spec.
    fn info(kind: &str, own: Option<&str>) -> anyhow::Result<Info> {
        let spec = crate::harness::session::Spec { kind: kind.into(), cwd: Some("/work/parser".into()), name: Some("the parser".into()), ..Default::default() };
        let mut i = crate::harness::host::refusal("s1", &spec, "").into_iter().find_map(|u| if let Update::Open { info, .. } = u { Some(*info) } else { None }).ok_or_else(|| anyhow::anyhow!("no info"))?;
        i.harness_session = own.map(str::to_owned);
        i.exit = None;
        Ok(i)
    }

    #[test]
    fn a_claude_session_is_packed_from_the_store_with_its_transcript_and_timeline() -> anyhow::Result<()> {
        let (_d, store) = crate::convo::store::scratch()?;
        store.send("s1", &Update::Open { info: Box::new(info("claude", Some("cc-1"))?), since: None });
        store.send("s1", &Update::State { state: Box::new(State { harness_session: Some("cc-1".into()), model: "opus".into(), epoch: 2, ..State::default() }) });
        store.send("s1", &Update::Events { events: vec![Event { id: "m1".into(), seq: 1, at: 5, parent: None, by: None, body: Body::Assistant { text: "done".into(), streaming: false } }] });
        let line = |n: u64, l: &str| Row::Record(Box::new(RecordRow { session: "s1".into(), source: "cc-1".into(), n, end: (n + 1) * 10, line: l.into(), kept: None }));
        store.put(vec![line(0, "{\"a\":1}"), line(1, "{\"b\":2}")]);
        store.sync()?;
        let pkg = package(&store, "s1", "gone-box", None, ("gone-box", "here"))?;
        assert_eq!(pkg.id, "s1");
        assert_eq!(pkg.spec.kind, "claude");
        assert_eq!(pkg.spec.cwd.as_deref(), Some("/work/parser"));
        assert_eq!(pkg.spec.resume.as_deref(), Some("cc-1"));
        assert_eq!(pkg.spec.model.as_deref(), Some("opus"));
        assert_eq!(pkg.carry, Carry::ClaudeLines { session: "cc-1".into(), lines: vec!["{\"a\":1}".into(), "{\"b\":2}".into()] });
        assert_eq!(pkg.events.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), vec!["m1"]);
        // Its epoch as it was: the machine it goes to counts one more as it takes it.
        assert_eq!(pkg.state.epoch, 2);
        // Another folder there, if asked.
        assert_eq!(package(&store, "s1", "gone-box", Some("/elsewhere"), ("gone-box", "here"))?.spec.cwd.as_deref(), Some("/elsewhere"));
        Ok(())
    }

    /// Ada, 2026-11-13: a crashed session resumed in place goes on natively, by its harness's own
    /// id, for every harness kiln supports: the package names the id it resumes (`spec.resume`)
    /// and writes nothing of the record (it's there); an ACP agent has the words too, for one that
    /// can't load sessions. Off its machine, as before.
    #[test]
    fn resume_in_place_every_harness_goes_on_natively_by_its_own_id() -> anyhow::Result<()> {
        use crate::harness::kinds::{Kind, Protocol};
        for k in Kind::all() {
            let (_d, store) = crate::convo::store::scratch()?;
            let own = format!("own-{}", k.id);
            store.send("s1", &Update::Open { info: Box::new(info(k.id, Some(&own))?), since: None });
            store.send("s1", &Update::State { state: Box::new(State { harness_session: Some(own.clone()), ..State::default() }) });
            store.send("s1", &Update::Events { events: vec![Event { id: "m1".into(), seq: 1, at: 5, parent: None, by: None, body: Body::Assistant { text: "half way there".into(), streaming: false } }] });
            store.sync()?;
            let pkg = package_for(&store, "s1", "box", None, ("box", "box"), true)?;
            assert_eq!(pkg.spec.resume.as_deref(), Some(own.as_str()), "{}: resumes its own", k.id);
            assert_eq!(pkg.version, 5, "{}", k.id);
            match (&pkg.carry, k.protocol) {
                (Carry::Native { session, words: Some(w) }, Protocol::Acp(_)) => anyhow::ensure!(*session == own && w.contains("half way there"), "{}: {w}", k.id),
                (Carry::Native { session, words: None }, Protocol::ClaudeStreamJson | Protocol::CodexAppServer | Protocol::AgyStreamJson | Protocol::PiRpc) => anyhow::ensure!(*session == own, "{}", k.id),
                (c, _) => anyhow::bail!("{}: {c:?}", k.id),
            }
            let seeded = crate::harness::moving::install(&pkg, "/work/parser").map_err(anyhow::Error::msg)?;
            anyhow::ensure!(seeded.is_some() == matches!(k.protocol, Protocol::Acp(_)), "{}: words only for ACP: {seeded:?}", k.id);
        }
        Ok(())
    }

    #[test]
    fn a_record_with_a_line_missing_is_refused() -> anyhow::Result<()> {
        let (_d, store) = crate::convo::store::scratch()?;
        store.send("s1", &Update::Open { info: Box::new(info("claude", Some("cc-1"))?), since: None });
        store.put(vec![Row::Record(Box::new(RecordRow { session: "s1".into(), source: "cc-1".into(), n: 1, end: 20, line: "{}".into(), kept: None }))]);
        store.sync()?;
        let err = package(&store, "s1", "gone-box", None, ("gone-box", "here")).err().map(|e| e.to_string()).unwrap_or_default();
        assert!(err.contains("missing line 0"), "{err}");
        // And nothing at all of it: refused too, saying why.
        let (_d2, empty) = crate::convo::store::scratch()?;
        empty.send("s2", &Update::Open { info: Box::new(info("claude", Some("cc-2"))?), since: None });
        empty.sync()?;
        assert!(package(&empty, "s2", "gone-box", None, ("gone-box", "here")).is_err());
        Ok(())
    }

    #[test]
    fn a_projected_state_is_not_resumed_until_the_full_owner_state_is_kept() -> anyhow::Result<()> {
        use crate::harness::model::QueueItem;
        use crate::harness::state_view;
        let (_d, store) = crate::convo::store::scratch()?;
        let queue = vec![QueueItem {
            id: "retained-message".into(), text: "kept body".repeat(state_view::FRAME / 8), at: 1,
            attachments: Vec::new(), overrides: None, reply_to: None,
        }];
        let full = State { harness_session: Some("cc-1".into()), queue: queue.clone(), ..State::default() };
        let projected: State = serde_json::from_value(state_view::view(&full, state_view::FRAME)?.into_state()?)?;
        assert!(projected.view.as_ref().is_some_and(|view| view.deferred.contains_key("queue")));
        assert!(projected.queue.is_empty());
        store.send("s1", &Update::Open { info: Box::new(info("claude", Some("cc-1"))?), since: None });
        store.send("s1", &Update::State { state: Box::new(projected) });
        let body = "{\"type\":\"assistant\",\"message\":\"retained transcript\"}";
        store.put(vec![Row::Record(Box::new(RecordRow {
            session: "s1".into(), source: "cc-1".into(), n: 0, end: 10, line: body.into(), kept: None,
        }))]);
        store.sync()?;
        let before = store.kept("s1", MAX_EVENTS)?.and_then(|kept| kept.state);
        let error = package(&store, "s1", "gone-box", None, ("gone-box", "here")).err().map(|e| e.to_string()).unwrap_or_default();
        assert!(error.contains("full state record"), "{error}");
        assert_eq!(store.kept("s1", MAX_EVENTS)?.and_then(|kept| kept.state), before);
        assert_eq!(store.record("s1", "cc-1", 0, 1)?, vec![(0, body.to_owned())]);
        store.send("s1", &Update::State { state: Box::new(full) });
        store.sync()?;
        assert_eq!(package(&store, "s1", "gone-box", None, ("gone-box", "here"))?.state.queue, queue);
        Ok(())
    }
}
