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
fn carry(store: &Store, session: &str, kind: &Kind, harness_session: Option<&str>, events: &[Event], note: &str) -> anyhow::Result<Carry> {
    if matches!(kind.protocol, Protocol::Acp(_) | Protocol::AgyStreamJson) {
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
        Protocol::Acp(_) | Protocol::AgyStreamJson => Ok(moving::words(events, note)),
        Protocol::Terminal => anyhow::bail!("{}'s sessions can't go on on another machine yet", kind.name),
    }
}

/// The package for session `id`, to go on on machine `to`, in folder `cwd` (the one it worked in
/// when None), made from what `store` kept. `from` names the machine it was on. Blocking.
pub fn package(store: &Store, id: &str, from: &str, to: &str, cwd: Option<&str>) -> anyhow::Result<Package> {
    let kept = store.kept(id, MAX_EVENTS)?.ok_or_else(|| anyhow::anyhow!("the server's store has nothing of this session"))?;
    let info: Info = kept.info.as_deref().and_then(|j| serde_json::from_str(j).ok()).ok_or_else(|| anyhow::anyhow!("the server's store doesn't say what this session is"))?;
    let state: State = kept.state.as_deref().and_then(|j| serde_json::from_str(j).ok()).unwrap_or_default();
    let events: Vec<Event> = kept.events.iter().filter_map(|e| serde_json::from_str(&e.body).ok()).collect();
    let kind = Kind::get(&info.kind).ok_or_else(|| anyhow::anyhow!("an unknown harness: {}", info.kind))?;
    let from_cwd = if state.cwd.is_empty() { info.cwd.clone() } else { state.cwd.clone() };
    let goes_on_in = cwd.map(str::trim).filter(|c| !c.is_empty()).map_or_else(|| from_cwd.clone(), str::to_owned);
    let own = state.harness_session.clone().or_else(|| info.harness_session.clone());
    let note = format!("This conversation went on from {from} (where it worked in {from_cwd}), which is gone, to {to} (it works in {goes_on_in} now). It was kept on the kiln server.");
    let carry = carry(store, id, kind, own.as_deref(), &events, &note)?;
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
        let pkg = package(&store, "s1", "gone-box", "here", None)?;
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
        assert_eq!(package(&store, "s1", "gone-box", "here", Some("/elsewhere"))?.spec.cwd.as_deref(), Some("/elsewhere"));
        Ok(())
    }

    #[test]
    fn a_record_with_a_line_missing_is_refused() -> anyhow::Result<()> {
        let (_d, store) = crate::convo::store::scratch()?;
        store.send("s1", &Update::Open { info: Box::new(info("claude", Some("cc-1"))?), since: None });
        store.put(vec![Row::Record(Box::new(RecordRow { session: "s1".into(), source: "cc-1".into(), n: 1, end: 20, line: "{}".into(), kept: None }))]);
        store.sync()?;
        let err = package(&store, "s1", "gone-box", "here", None).err().map(|e| e.to_string()).unwrap_or_default();
        assert!(err.contains("missing line 0"), "{err}");
        // And nothing at all of it: refused too, saying why.
        let (_d2, empty) = crate::convo::store::scratch()?;
        empty.send("s2", &Update::Open { info: Box::new(info("claude", Some("cc-2"))?), since: None });
        empty.sync()?;
        assert!(package(&empty, "s2", "gone-box", "here", None).is_err());
        Ok(())
    }
}
