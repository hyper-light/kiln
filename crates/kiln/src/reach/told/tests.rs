//! The runner's side of telling agents of their runs (docs/reach.md §7): the outbox, its link
//! state, what it keeps across restarts, and what an MCP server tells.

use super::*;

/// The outbox is one per process: its tests take turns, each from an empty one.
fn fresh() -> parking_lot::MutexGuard<'static, ()> {
    static TURN: Mutex<()> = Mutex::new(());
    let g = TURN.lock();
    *STATE.lock() = State::default();
    g
}

fn news(key: &str, event: RunEvent) -> RunNews {
    RunNews { key: key.into(), id: key.into(), event, command: "make".into(), on: "box".into(), how: "run".into(), handed: 1, ..RunNews::default() }
}

fn unsent() -> Vec<(String, RunEvent)> {
    let st = STATE.lock();
    let mut e: Vec<&Entry> = st.entries.iter().filter(|e| !e.sent).collect();
    e.sort_by_key(|e| e.seq);
    e.into_iter().map(|e| (e.news.key.clone(), e.news.event)).collect()
}

/// T1: news goes only once the server welcomed this runner and said it takes news (its features
/// may come first); a disconnect clears both, so nothing is sent into a gone connection, and the
/// next connection must say it again.
#[test]
fn t01_news_goes_once_the_server_is_up_and_takes_it_and_not_after_it_went() {
    let _turn = fresh();
    server_features(&[FEATURE.to_owned()]);
    assert!(!takes(), "its Caps and features alone: not up yet");
    server_up();
    assert!(takes(), "welcomed, and it takes news");
    server_down();
    assert!(!takes(), "gone: cleared (it was never cleared)");
    server_up();
    assert!(!takes(), "back, but it hasn't said it takes news on this connection");
    assert!(will_tell(), "what's said meanwhile reaches the agent once a server that took news is back");
    server_features(&["link-features".to_owned()]);
    assert!(!takes() && !will_tell(), "a server that doesn't take news is sent none");
}

/// T4, E1: when the server takes news anew (a reconnect, a server restart), the latest news of
/// every run still followed goes again, after what waited unsent; a run over that it had isn't.
#[test]
fn t04_e1_when_the_server_takes_news_anew_every_run_still_followed_goes_again() {
    let _turn = fresh();
    say("s1", news("a", RunEvent::Going), None);
    say("s1", news("b", RunEvent::Started), None);
    say("s1", news("c", RunEvent::Ended), None);
    {
        let mut st = STATE.lock();
        for e in st.entries.iter_mut().filter(|e| e.news.key != "c") {
            e.sent = true;
        }
    }
    assert_eq!(unsent(), [("c".to_owned(), RunEvent::Ended)]);
    server_features(&[FEATURE.to_owned()]);
    server_up();
    assert_eq!(unsent(), [("c".to_owned(), RunEvent::Ended), ("a".to_owned(), RunEvent::Going), ("b".to_owned(), RunEvent::Started)], "what waited first, then each run still followed");
    answered("s1", &news("c", RunEvent::Ended), false, false);
    assert!(STATE.lock().entries.iter().all(|e| e.news.key != "c"), "a run over that the server has is followed no more");
    server_down();
}

/// T9, T10: the same news twice is one; later news of a run takes the place of what waited of
/// it, and earlier news that comes late changes nothing; what an MCP server says again of a run
/// it told was over isn't news.
#[test]
fn t09_t10_the_outbox_keeps_each_run_s_latest_news_in_order() {
    let _turn = fresh();
    say("s1", news("a", RunEvent::Going), Some(7));
    say("s1", news("a", RunEvent::Going), Some(7));
    say("s1", news("b", RunEvent::Going), Some(7));
    say("s1", news("a", RunEvent::Ended), Some(7));
    say("s1", news("a", RunEvent::Started), Some(7));
    assert_eq!(unsent(), [("b".to_owned(), RunEvent::Going), ("a".to_owned(), RunEvent::Ended)]);
    answered("s1", &news("a", RunEvent::Ended), false, false);
    say("s1", news("a", RunEvent::Ended), Some(7));
    assert_eq!(unsent(), [("b".to_owned(), RunEvent::Going)], "said again by the MCP server that told it was over");
}

/// T7: the runs an MCP server followed in the background go with it: their agents are told they
/// were stopped, once, with no id to read their output by (what knew it has gone); those over aren't.
#[test]
fn t07_the_runs_an_mcp_server_followed_go_with_it_with_no_output_id() {
    let _turn = fresh();
    let session = crate::util::new_id();
    say(&session, news("r1", RunEvent::Going), Some(4_000_000));
    say(&session, news("r2", RunEvent::Ended), Some(4_000_000));
    say(&session, news("r3", RunEvent::Started), Some(4_000_000));
    gone(Some(4_000_000));
    gone(Some(4_000_000));
    let stopped: Vec<(String, String)> = heard_of(&session).into_iter().filter(|n| n.event == RunEvent::Ended && n.error.is_some()).map(|n| (n.key, n.id)).collect();
    assert_eq!(stopped, [("r1".to_owned(), String::new()), ("r3".to_owned(), String::new())]);
}

/// T3, E2: the outbox is kept in a file and read in by the next runner process: a detached run it
/// followed on another runner is followed on (its peer and reach session kept); one that ran on
/// this machine, which can't be, is told as ended, how unknown; news the server hadn't had goes.
#[test]
fn t03_e2_detached_follows_outlive_a_runner_restart() -> anyhow::Result<()> {
    let _turn = fresh();
    let dir = tempfile::tempdir()?;
    let file = dir.path().join("told.json");
    let remote = Entry { session: "s1".into(), news: RunNews { how: DETACHED.into(), ..news("d1", RunEvent::Started) }, sent: true, seq: 1, by: None, resume: Some(Resume { peer: "mac".into(), session: "reach-s".into(), run: 7 }), tail: "building…".into(), answer: false };
    let here = Entry { news: news("d2", RunEvent::Going), seq: 2, resume: Some(Resume::default()), ..remote.clone() };
    let here_resume = here.resume.clone();
    let over = Entry { news: news("e1", RunEvent::Ended), seq: 3, sent: false, resume: None, ..remote.clone() };
    std::fs::write(&file, serde_json::to_vec(&vec![remote.clone(), here.clone(), over])?)?;
    // A runner process before this one kept them; this one heard of d1 itself already.
    say("s1", RunNews { how: DETACHED.into(), ..news("d1", RunEvent::Ended) }, None);
    let resume = merge(read(&file));
    assert_eq!(resume.iter().map(|e| e.news.key.as_str()).collect::<Vec<_>>(), ["d2"], "d1's newer news stands; d2 to follow on");
    assert!(STATE.lock().entries.iter().filter(|e| e.news.key != "d1").all(|e| !e.sent), "each goes to the server again");
    crate::rt::block_on(async move {
        follow_on(here).await;
        anyhow::Ok(())
    })?;
    let told = heard_of("s1");
    assert!(told.iter().any(|n| n.key == "d2" && n.event == RunEvent::Ended && n.error.as_deref() == Some(LOST_HERE)), "{told:?}");
    // Kept as written: the next process reads what this one writes.
    STATE.lock().file = Some(file.clone());
    let snapshot = serde_json::to_vec(&STATE.lock().entries)?;
    std::fs::write(&file, snapshot)?;
    assert_eq!(read(&file), STATE.lock().entries, "read back as written");
    assert!(read(&file).iter().any(|e| e.news.key == "d2" && e.resume == here_resume));
    Ok(())
}

/// T12, E8: news over the socket is for the session the sending process belongs to by its process
/// alone: a process in no agent session (whatever its environment says) is refused.
#[test]
fn t12_news_comes_only_from_an_agent_session_s_own_processes() -> anyhow::Result<()> {
    let json = serde_json::to_string(&news("x", RunEvent::Going))?;
    let (refused, probe) = crate::rt::block_on(async move { anyhow::Ok((heard(&json, Some(std::process::id())).await, heard("{}", None).await)) })?;
    anyhow::ensure!(matches!(refused, Ev::Error(ref e) if e.contains("agent session's own processes")), "{refused:?}");
    anyhow::ensure!(matches!(probe, Ev::Answer(_)), "news of no run asks whether news goes: {probe:?}");
    Ok(())
}

/// T6: telling the runner never waits on it: with no runner there, `tell` returns at once; an
/// attempt is bounded by the RTO (a runner that never answers doesn't hold the teller).
#[test]
fn t06_telling_never_blocks_and_each_attempt_is_bounded() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let sock = dir.path().join("reach.sock");
    let t = teller(&sock);
    let at = Instant::now();
    for n in 0..100 {
        t.tell(news(&format!("r{n}"), RunEvent::Going));
    }
    assert!(at.elapsed() < Duration::from_millis(500), "{:?}", at.elapsed());
    let _mute = std::os::unix::net::UnixListener::bind(&sock)?;
    let at = Instant::now();
    assert!(tell_runner(&sock, &news("r", RunEvent::Going), Duration::from_millis(200)).is_err());
    assert!(at.elapsed() < Duration::from_secs(2), "bounded: {:?}", at.elapsed());
    Ok(())
}

/// T2, T8, T10: what an MCP server tells of a run: handed over (each answer that returns it
/// going, again); its start as state; its end only with no call waiting on it; a look at its end
/// after, as seen; a run it stops, seen, its end no news.
#[test]
fn t02_t08_what_an_mcp_server_tells_of_a_run() {
    let mut t = Telling { news: RunNews { key: "k".into(), id: "r1".into(), command: "make".into(), how: "run".into(), ..RunNews::default() }, ..Telling::default() };
    let mut v = json!({"running": true});
    t.waits();
    t.answered(&mut v, false, "box");
    t.heard(&Ok(Some(Ev::Started { runner: "box".into(), pid: 1, cwd: String::new() })), "box", "");
    t.waits();
    t.answered(&mut v, false, "box");
    t.waits();
    t.heard(&Ok(Some(Ev::Exit { code: Some(0), error: None, lost: 0 })), "box", "ok\n");
    t.answered(&mut v, true, "box");
    let said: Vec<(RunEvent, u32)> = t.said.iter().map(|n| (n.event, n.handed)).collect();
    assert_eq!(said, [(RunEvent::Going, 1), (RunEvent::Started, 1), (RunEvent::Going, 2), (RunEvent::Seen, 2)], "a call waited on its end: no end told");
    let mut s = Telling { news: RunNews { key: "k2".into(), ..RunNews::default() }, ..Telling::default() };
    s.answered(&mut v, false, "box");
    s.stopped();
    s.heard(&Ok(Some(Ev::Exit { code: None, error: Some("killed".into()), lost: 0 })), "box", "");
    assert_eq!(s.said.iter().map(|n| n.event).collect::<Vec<_>>(), [RunEvent::Going, RunEvent::Seen]);
    let mut e = Telling { news: RunNews { key: "k3".into(), ..RunNews::default() }, ..Telling::default() };
    e.answered(&mut v, false, "box");
    e.heard(&Ok(Some(Ev::Exit { code: Some(2), error: None, lost: 0 })), "box", &"x".repeat(10 * LINE_SHARE));
    assert!(matches!(e.said.last(), Some(n) if n.event == RunEvent::Ended && n.exit_code == Some(2) && n.tail.len() <= LINE_SHARE), "{:?}", e.said.last().map(|n| n.event));
}
