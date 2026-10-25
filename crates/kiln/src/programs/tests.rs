//! CP-M3's programs (docs/programs.md): how programs are found and given, the rotation, and
//! the usage rules.

use std::sync::Arc;

use serde_json::{Value, json};

use super::*;

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| (*x).to_owned()).collect()
}

/// A master plan's §5, cut down: the tracks table with its weights, then a table that isn't one.
const MASTER_BODY: &str = "# A program\n\n## 5. Tracks, in parallel\n\n| Track | Repository | Machines | Weight | Starts |\n|---|---|---|---|---|\n| T1 Amphora | hyper-light/amphora (new) | adacachy | 5 | wave 1 |\n| T2 Quiver | hyper-light/quiver | adacachy | 3 | by rotation |\n| T10 Mantle in kiln | kiln | adacachy | 3 | wave 1 |\n\n| Product | Keeps |\n|---|---|\n| T3 Not a weight | 7 |\n";

/// CP-M3: a program from its plans' tags (`master`, the program's, `t<n>`), each track's name from
/// its plan's title, and the weights its master plan's table gives.
#[test]
fn a_program_is_found_from_its_plans_tags_its_weights_from_its_table() {
    assert_eq!(track_number(&s(&["critical-path", "t7", "hex"])), Some(7));
    assert_eq!(track_number(&s(&["critical-path", "master"])), None);
    assert_eq!(track_number(&s(&["tea", "t"])), None);
    assert_eq!(program_tag(&s(&["critical-path", "master"])).as_deref(), Some("critical-path"));
    assert_eq!(track_name("Critical path T1: Amphora (KilnDB made a product), its own engine", &s(&["critical-path", "t1", "amphora"]), "critical-path"), "Amphora");
    assert_eq!(track_name("Critical path T10: Mantle in kiln: every byte kiln keeps", &s(&["critical-path", "t10", "mantle"]), "critical-path"), "Mantle in kiln");
    assert_eq!(track_name("No colon here", &s(&["critical-path", "t4", "isolation"]), "critical-path"), "isolation");
    assert_eq!(plan_weights(MASTER_BODY), BTreeMap::from([("T1".to_owned(), 5), ("T10".to_owned(), 3), ("T2".to_owned(), 3)]));
}

/// CP-M3: goals are a track's by their ids (`CP-T7.1`); a track's goals share its key's prefix
/// (`CP-T7.`), which `CP-T70.1`'s isn't.
#[test]
fn goals_are_matched_to_tracks_by_their_ids() {
    assert_eq!(goal_key("CP-T7.1 Hex set up; the node runtime"), Some(("CP-T7.1".to_owned(), 7)));
    assert_eq!(goal_key("CP-T10.12 Many goals in"), Some(("CP-T10.12".to_owned(), 10)));
    for not in ["CP-M3 Work this goal stack's track goals", "cp-T7.1 lower case", "CP-T7 no step", "CP-T.1 no track", "T7.1 no program", ""] {
        assert_eq!(goal_key(not), None, "{not}");
    }
    assert_eq!(track_prefix("CP-T7.1"), Some("CP-T7."));
    assert_eq!(track_prefix("CP-T10.12"), Some("CP-T10."));
    assert!(!"CP-T70.1 Another track".starts_with("CP-T7."));
}

/// CP-M3: usage is an account's weekly window (any harness's `7d`), the highest of them.
#[test]
fn usage_is_the_fullest_weekly_window() {
    let l = |id: &str, window: &str, pct: f64| Limit { id: id.into(), label: id.into(), used_percent: Some(pct), window: Some(window.into()), account: Some("a@x".into()), ..Limit::default() };
    let limits = [l("five_hour", "5h", 99.0), l("seven_day", "7d", 42.0), l("seven_day_opus", "7d", 81.0), l("codex.secondary", "7d", 12.0)];
    assert_eq!(weekly(&limits).map(|u| u.pct), Some(81.0), "the session window isn't the weekly one");
    assert_eq!(weekly(&limits[..1]), None);
    assert_eq!(weekly(&[]), None);
}

/// CP-M3 (the master plan's §5): above 80% of a weekly window no new goal starts; above 95% every
/// stack pauses; not known, nothing is held.
#[test]
fn the_usage_rules_hold_above_80_and_pause_above_95() {
    assert_eq!(Rule::at(None), Rule::Ok);
    assert_eq!(Rule::at(Some(80.0)), Rule::Ok);
    assert!(matches!(Rule::at(Some(80.5)), Rule::Hold { .. }));
    assert!(matches!(Rule::at(Some(95.0)), Rule::Hold { .. }));
    let paused = Rule::at(Some(96.0));
    assert!(matches!(&paused, Rule::Pause { why } if why.starts_with(PAUSED_FOR_USAGE)), "{paused:?}");
    assert!(!Rule::Ok.holds_new_goals() && Rule::at(Some(81.0)).holds_new_goals() && Rule::at(Some(99.0)).holds_new_goals());
}

fn t(number: u32, weight: u32, met: u32, running: bool, done: bool) -> Slotting {
    Slotting { number, weight, pass: pass(met, weight), running, done }
}

/// CP-M3: tracks at work keep their slots; the free ones go to the lowest pass, then the heavier,
/// then the lower number; a track done never gets one.
#[test]
fn the_rotation_gives_free_slots_to_the_lowest_pass() {
    let tracks = [t(1, 5, 2, true, false), t(2, 3, 1, false, false), t(3, 2, 0, false, false), t(4, 4, 0, false, false), t(5, 1, 9, false, true)];
    // Two slots, one at work: the next is the lowest pass, T3 and T4 at 0, the heavier first.
    assert_eq!(rotate(&tracks, 2), ["running", "waiting", "waiting", "next", "done"]);
    assert_eq!(rotate(&tracks, 3), ["running", "waiting", "next", "next", "done"]);
    // Fewer slots than tracks at work: none is stopped mid-goal, and none is next.
    assert_eq!(rotate(&tracks, 0), ["running", "waiting", "waiting", "waiting", "done"]);
}

/// CP-M3 (the master plan's §5): the lowest-weight track at work pauses at 80% (of those as light,
/// the one furthest ahead, then the higher number); none at work, none does.
#[test]
fn the_lowest_weight_track_at_work_is_the_one_held_to_80() {
    let tracks = [t(1, 5, 2, true, false), t(2, 2, 0, false, false), t(3, 3, 1, true, false), t(7, 3, 3, true, false)];
    // T2 is lighter, but not at work; T3 and T7 weigh 3, and T7 is further ahead.
    assert_eq!(lowest_weight(&tracks), Some(7));
    assert_eq!(lowest_weight(&[t(4, 2, 1, true, false), t(9, 2, 1, true, false)]), Some(9), "as light and as far: the higher number");
    assert_eq!(lowest_weight(&[t(1, 1, 0, false, false)]), None);
    assert_eq!(lowest_weight(&[]), None);
}

/// CP-M3: kiln sets a worker's threshold when it has none or still has the one kiln set last; one
/// a person set since is theirs and stays; one kiln no longer wants is taken back, unless it's a
/// person's.
#[test]
fn kiln_sets_a_worker_s_threshold_but_leaves_one_a_person_set() {
    assert_eq!(threshold_change(None, None, Some(95)), Some(Some(95)), "none yet: kiln's");
    assert_eq!(threshold_change(Some(95), Some(95), Some(80)), Some(Some(80)), "kiln's, changed");
    assert_eq!(threshold_change(Some(80), Some(80), Some(80)), None, "as it should be");
    assert_eq!(threshold_change(Some(90), Some(95), Some(80)), None, "a person's since: theirs");
    assert_eq!(threshold_change(Some(90), None, Some(95)), None, "a person's from before: theirs");
    assert_eq!(threshold_change(Some(80), Some(80), None), Some(None), "no longer a worker: kiln's taken back");
    assert_eq!(threshold_change(Some(90), Some(80), None), None, "no longer a worker: a person's stays");
    assert_eq!(threshold_change(None, Some(80), None), None);
}

/// The goals each track of a lab program gets (each round, every track the rotation names next
/// takes one goal and meets it), `goals` in all.
fn lab(weights: &[u32], slots: u32, goals: u32) -> Vec<u32> {
    let mut met = vec![0_u32; weights.len()];
    let mut given = 0;
    while given < goals {
        let tracks: Vec<Slotting> = weights.iter().zip(&met).zip(1_u32..).map(|((w, m), n)| t(n, *w, *m, false, false)).collect();
        let next: Vec<usize> = rotate(&tracks, slots).iter().enumerate().filter(|(_, s)| **s == "next").map(|(i, _)| i).collect();
        assert!(!next.is_empty(), "a free slot goes to a ready track");
        for i in next {
            if given < goals
                && let Some(m) = met.get_mut(i)
            {
                *m += 1;
                given += 1;
            }
        }
    }
    met
}

/// CP-M3's test: a lab program of stub tracks weighted 5, 3 and 2 over 30 goals: each track's share
/// of the goals is within one goal of its weight's share (15, 9 and 6), with one slot or two.
#[test]
fn a_lab_of_tracks_weighted_5_3_and_2_shares_30_goals_by_weight() {
    let weights = [5, 3, 2];
    for slots in [1, 2] {
        let got = lab(&weights, slots, 30);
        // Said, so the gate's log shows the shares (`--show-output`).
        println!("the lab: weights {weights:?} over 30 goals with {slots} slot(s) gave {got:?} (their shares: [15, 9, 6])");
        let sum: u32 = weights.iter().sum();
        for (g, w) in got.iter().zip(&weights) {
            let share = 30 * w / sum;
            assert!(g.abs_diff(share) <= 1, "{slots} slots: weight {w} got {g} of 30, its share {share}: {got:?}");
        }
        assert_eq!(got.iter().sum::<u32>(), 30);
    }
}

/// A plan, as kiln keeps it, straight into its store.
fn plan_row(c: &rusqlite::Connection, id: &str, title: &str, body: &str, tags: &[&str], at: i64) -> anyhow::Result<()> {
    c.execute(
        "INSERT INTO plans (id, title, body, tags, items, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, '[]', ?5, ?5)",
        rusqlite::params![id, title, body, serde_json::to_string(tags)?, at],
    )?;
    Ok(())
}


/// Waits (up to 10 s) for `holds`: what a session does with a command comes a moment after it.
async fn until(what: &str, holds: impl Fn() -> bool) -> anyhow::Result<()> {
    for _ in 0..1_000 {
        if holds() {
            return Ok(());
        }
        crate::rt::sleep(std::time::Duration::from_millis(10)).await;
    }
    anyhow::bail!("{what}: not in 10 s")
}

/// Session `id`'s state changed as its machine would report it (its usage, a pause).
fn report(hub: &Hub, id: &str, f: impl FnOnce(&mut crate::harness::model::State)) -> anyhow::Result<()> {
    let mut st = hub.harness.state(id).ok_or_else(|| anyhow::anyhow!("no session {id}"))?;
    f(&mut st);
    hub.harness.ingest(hub, &crate::harness::registry::Location::Local, id, crate::harness::core::Update::State { state: Box::new(st) });
    Ok(())
}

fn weekly_at(pct: f64) -> Vec<Limit> {
    vec![Limit { id: "seven_day".into(), label: "Weekly".into(), used_percent: Some(pct), window: Some("7d".into()), ..Limit::default() }]
}

/// CP-M3: a program as the view shows it, from kiln's records: its master plan run as goals on a
/// coordinator, its tracks from their plans, goals from the coordinator's stack by their ids. Each
/// track's worker is assigned (by the coordinator or the person: any agent of the same person,
/// its child or not), its goal under way the coordinator's handed to it (shown in that stack as
/// under way there) or its own; its session's pause shows. Workers are held to their thresholds
/// (95, the lowest-weight track at work's 80), a person's left as it is. The person's settings
/// change the slots and weights (seeded once from the master plan), its coordinator told; above
/// 80% of a weekly window no new goal is put under way. An agent can't change the settings.
#[test]
fn a_program_shows_each_track_s_worker_goal_pass_and_slot_and_holds_the_usage_rules() -> anyhow::Result<()> {
    use crate::authz::Caller;
    use crate::harness::session::Spec;
    crate::rt::block_on(async {
        let hub = crate::hub::tests::scratch_hub_with(|c| {
            c.terminal.enabled = true;
            c.conversations.off = true;
        })
        .await?;
        // What the sessions say of themselves reaches kiln's record of them.
        let h = Arc::clone(&hub);
        crate::rt::spawn(async move {
            let _ = crate::harness::registry::Registry::run_local(h).await;
        });
        let dir = tempfile::tempdir()?;
        let spec = |name: &str, parent: Option<&str>| Spec {
            kind: "claude".into(),
            name: Some(name.to_owned()),
            cwd: Some(dir.path().to_string_lossy().into_owned()),
            program: Some(dir.path().join("no-such-claude").to_string_lossy().into_owned()),
            parent: parent.map(str::to_owned),
            ..Spec::default()
        };
        let open = |name: &'static str, parent: Option<String>| {
            let hub = Arc::clone(&hub);
            let spec = spec(name, parent.as_deref());
            async move {
                let opened = hub.harness.open(&hub, "local", spec, 1).await.map_err(|e| anyhow::anyhow!("{e:#}"))?;
                let id = opened.into_iter().next().map(|i| i.id).ok_or_else(|| anyhow::anyhow!("nothing opened"))?;
                // Settled: its harness (there's none) didn't start, so what it says next is what
                // the test reports for it.
                until("the session settled", || hub.harness.info(&id).is_some_and(|i| i.status != Status::Starting)).await?;
                anyhow::Ok(id)
            }
        };
        let coordinator = open("Coordinator", None).await?;
        // Hex T7 isn't the coordinator's child (as T1's Amphora isn't); Sends T3 is.
        let hex = open("Hex T7", None).await?;
        let sends = open("Sends T3", Some(coordinator.clone())).await?;
        let stranger = open("Someone else's", None).await?;
        hub.store.kv_set(crate::mcp::opener_key(&stranger), "user:u2".into()).await?;
        let body = "| Track | Weight |\n|---|---|\n| T3 Sends | 5 |\n| T7 Hex | 4 |\n";
        let c2 = coordinator.clone();
        hub.store
            .call(move |c| {
                plan_row(c, "m1", "A program, master", body, &["lab", "master"], 3)?;
                plan_row(c, "p3", "A program T3: Sends, acknowledged", "", &["lab", "t3", "sends"], 2)?;
                plan_row(c, "p4", "A program T4: Isolation, by machine", "", &["lab", "t4", "isolation"], 2)?;
                plan_row(c, "p7", "A program T7: Hex, the scheduler", "", &["lab", "t7", "hex"], 2)?;
                plan_row(c, "other", "Not the program's", "", &["elsewhere", "t9"], 1)?;
                c.execute("INSERT INTO plan_runs (id, plan_id, version, session, mode, at, by) VALUES ('r1', 'm1', 1, ?1, 'goals', 5, 'admin')", rusqlite::params![c2])?;
                Ok(())
            })
            .await?;
        let given = |t: &str| crate::goals::Given { id: None, text: t.into() };
        crate::goals::set(&hub, &coordinator, vec![given("LAB-M1 The master step under way"), given("LAB-T7.1 Hex set up"), given("LAB-T7.2 Hex's scheduler"), given("LAB-T7.3 Hex's queue"), given("LAB-T3.1 Sends deduplicated"), given("LAB-T4.1 Isolation by machine")]).await?;
        crate::goals::set(&hub, &sends, vec![given("LAB-T3.1 Sends deduplicated")]).await?;
        let (c3, s3) = (coordinator.clone(), sends.clone());
        hub.store
            .call(move |c| {
                c.execute("UPDATE goal_items SET status = 'met' WHERE agent = ?1 AND text LIKE 'LAB-T7.1%'", rusqlite::params![c3])?;
                c.execute("UPDATE goal_items SET status = 'active' WHERE agent = ?1 AND text LIKE 'LAB-M1%'", rusqlite::params![c3])?;
                c.execute("UPDATE goal_items SET status = 'active' WHERE agent = ?1", rusqlite::params![s3])?;
                c.execute("UPDATE goal_stacks SET state = 'running' WHERE agent = ?1", rusqlite::params![s3])?;
                Ok(())
            })
            .await?;
        let stack = |agent: &str| {
            let (hub, agent) = (Arc::clone(&hub), agent.to_owned());
            async move { crate::goals::get(&hub, &agent).await.map_err(|e| anyhow::anyhow!("{e}")) }
        };
        let item = |items: &[crate::goals::Item], key: &str| items.iter().find(|i| i.text.starts_with(key)).cloned().ok_or_else(|| anyhow::anyhow!("no goal {key}"));
        let goals = stack(&coordinator).await?.items;
        let (t7_2, t7_3, t3_1) = (item(&goals, "LAB-T7.2")?.id, item(&goals, "LAB-T7.3")?.id, item(&goals, "LAB-T3.1")?.id);
        let program = || {
            let hub = Arc::clone(&hub);
            async move { get(&hub, "m1").await?.ok_or_else(|| anyhow::anyhow!("no program")) }
        };
        let track = |p: &Program, id: &str| p.tracks.iter().find(|t| t.id == id).cloned().ok_or_else(|| anyhow::anyhow!("no track {id}"));

        // Before any worker's assigned: every track ready, none at work.
        let p = program().await?;
        assert_eq!((p.tag.as_str(), p.slots, p.coordinator.as_ref().map(|c| c.id.as_str()), p.coordinator_id.as_deref()), ("lab", SLOTS, Some(coordinator.as_str()), Some(coordinator.as_str())));
        assert_eq!(p.goal.as_ref().map(|g| (g.key.as_deref(), g.status.as_str())), Some((None, "active")), "the master step, no track's");
        let ids: Vec<&str> = p.tracks.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["T3", "T4", "T7"], "its tracks, in order; another program's plan isn't one");
        let t7 = track(&p, "T7")?;
        assert_eq!((t7.name.as_str(), t7.weight, t7.met, t7.total, t7.worker.is_none(), t7.slot.as_str()), ("Hex", 4, 1, 3, true, "next"));
        assert_eq!(t7.goal.as_ref().map(|g| (g.key.as_deref(), g.status.as_str())), Some((Some("LAB-T7.2"), "pending")));
        assert!((t7.pass - 0.25).abs() < 1e-9, "one goal met at weight 4: {}", t7.pass);
        assert_eq!((p.met, p.total, p.running, p.rule.clone()), (1, 5, 0, Rule::Ok));

        // The coordinator assigns T7's worker, its child or not, and hands it T7's next goal: under
        // way there, and shown so in its stack; the person assigns T3's, at work on its own goal.
        let session = |agent: &str| {
            let mut c = Caller::owner();
            c.name = format!("agent:{}", agent.chars().take(8).collect::<String>());
            c.agent = Some(agent.to_owned());
            c
        };
        let run = |caller: Caller, op: &'static str, v: Value| {
            let cx = crate::ops::Cx { hub: Arc::clone(&hub), caller };
            async move { crate::ops::run_own(cx, op, v).await }
        };
        let ok = |r: Result<Value, crate::ops::Fail>| r.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"));
        ok(run(session(&coordinator), "programs.assign", json!({"program": "m1", "track": "T7", "agent": hex, "goal": t7_2})).await)?;
        ok(run(Caller::owner(), "programs.assign", json!({"program": "m1", "track": "T3", "agent": sends})).await)?;
        assert_eq!(item(&stack(&coordinator).await?.items, "LAB-T7.2")?.on.as_deref(), Some(hex.as_str()), "under way on Hex T7, in the coordinator's stack");
        let p = program().await?;
        let (t3, t4, t7) = (track(&p, "T3")?, track(&p, "T4")?, track(&p, "T7")?);
        assert_eq!((t7.worker.as_ref().map(|w| w.name.as_str()), t7.assigned.as_deref(), t7.slot.as_str()), (Some("Hex T7"), Some(hex.as_str()), "running"));
        let g = t7.goal.clone().ok_or_else(|| anyhow::anyhow!("T7's goal"))?;
        assert_eq!((g.key.as_deref(), g.on.as_deref(), g.on_name.as_deref()), (Some("LAB-T7.2"), Some(hex.as_str()), Some("Hex T7")));
        assert_eq!((t3.worker.as_ref().map(|w| w.id.as_str()), t3.goal.as_ref().and_then(|g| g.key.as_deref()), t3.slot.as_str()), (Some(sends.as_str()), Some("LAB-T3.1"), "running"), "at work on its own stack's goal");
        assert_eq!((t4.worker.is_none(), t4.slot.as_str(), t4.wants), (true, "next", None));
        // Its thresholds: 95, and 80 for the lowest-weight track at work (T7, at 4 to T3's 5).
        assert_eq!((t7.wants, t3.wants, p.running), (Some(LOWEST_PAUSE_AT), Some(WORKER_PAUSE_AT), 2));

        // Refused: another agent assigning; a worker of another person's; a goal of another
        // track's; an agent kiln doesn't know.
        let refused = |r: Result<Value, crate::ops::Fail>| r.err().map(|(c, _)| c);
        assert_eq!(refused(run(session(&sends), "programs.assign", json!({"program": "m1", "track": "T4", "agent": sends})).await), Some(http::StatusCode::FORBIDDEN));
        assert_eq!(refused(run(Caller::owner(), "programs.assign", json!({"program": "m1", "track": "T4", "agent": stranger})).await), Some(http::StatusCode::FORBIDDEN));
        assert_eq!(refused(run(Caller::owner(), "programs.assign", json!({"program": "m1", "track": "T7", "goal": t3_1})).await), Some(http::StatusCode::BAD_REQUEST));
        assert_eq!(refused(run(Caller::owner(), "programs.assign", json!({"program": "m1", "track": "T4", "agent": "no-such-agent"})).await), Some(http::StatusCode::NOT_FOUND));
        assert!(!assignments(&hub.store, "m1").await?.contains_key("T4"), "nothing kept");
        // An assignee kiln doesn't know any more (its session closed) holds no slot: its row says
        // whom it's assigned to, and the goal handed to it stays out until it's assigned again.
        let t4_1 = item(&goals, "LAB-T4.1")?.id;
        let mut kept = assignments(&hub.store, "m1").await?;
        kept.insert("T4".into(), Assignment { agent: "gone-agent".into(), goal: Some(t4_1.clone()), at: 1, by: "admin".into() });
        save_assignments(&hub.store, "m1", &kept).await?;
        assert!(crate::goals::delegate(&hub.store, &coordinator, "LAB-T4.", Some(t4_1.as_str()), Some("gone-agent"), false, 1).await?);
        let t4 = track(&program().await?, "T4")?;
        assert_eq!((t4.worker.is_none(), t4.assigned.as_deref(), t4.goal.as_ref().and_then(|g| g.on.as_deref()), t4.slot.as_str()), (true, Some("gone-agent"), Some("gone-agent"), "next"));
        kept.remove("T4");
        save_assignments(&hub.store, "m1", &kept).await?;
        assert!(crate::goals::delegate(&hub.store, &coordinator, "LAB-T4.", None, None, false, 2).await?);

        // The thresholds, held: kiln's set; one a person sets stays theirs.
        let pause_at = |agent: &str| hub.harness.info(agent).and_then(|i| i.pause_at);
        enforce(&hub).await;
        until("the workers' thresholds", || pause_at(&hex) == Some(80) && pause_at(&sends) == Some(95)).await?;
        let kept: BTreeMap<String, u8> = kv_json(&hub.store, thresholds_key("m1")).await?;
        assert_eq!(kept, BTreeMap::from([(hex.clone(), 80), (sends.clone(), 95)]));
        assert!(matches!(hub.harness.command(&hub, &hex, Command::PauseAt { percent: Some(90) }).await, crate::harness::registry::Sent::Taken));
        until("the person's threshold", || pause_at(&hex) == Some(90)).await?;
        enforce(&hub).await;
        assert_eq!((pause_at(&hex), pause_at(&sends)), (Some(90), Some(95)), "the person's stays");
        let sent: BTreeMap<String, u8> = kv_json(&hub.store, thresholds_key("m1")).await?;
        assert_eq!(sent, BTreeMap::from([(hex.clone(), 80), (sends.clone(), 95)]), "and kiln sent nothing (it keeps each threshold it sends)");

        // The person's settings: one slot, and T3 lighter. Both tracks at work keep theirs (none is
        // stopped mid-goal); T3 is now the lowest-weight one at work. Its coordinator is told.
        let set = ok(run(Caller::owner(), "programs.set", json!({"program": "m1", "slots": 1, "weights": {"T3": 2}})).await)?;
        let at = |p: &str| set.pointer(p).cloned().unwrap_or(Value::Null);
        assert_eq!((at("/slots"), at("/running"), at("/tracks/0/weight"), at("/tracks/0/wants"), at("/tracks/1/slot")), (json!(1), json!(2), json!(2), json!(80), json!("waiting")));
        let kept = settings(&hub, "m1").await?;
        assert_eq!((kept.slots, kept.weights.get("T3").copied(), kept.weights.get("T7").copied(), kept.seeded, kept.by.as_str()), (Some(1), Some(2), Some(4), true, "admin"));
        let notice = kept.notice.clone().ok_or_else(|| anyhow::anyhow!("no notice"))?;
        assert!(notice.text.contains("slots 6 → 1") && notice.text.contains("T3's weight 5 → 2") && notice.text.contains("programs.get"), "{}", notice.text);
        assert!(notice.delivered && notice.message.is_some() && notice.why.is_none(), "taken by the coordinator's session: {notice:?}");
        enforce(&hub).await;
        until("T3's worker held to 80", || pause_at(&sends) == Some(80)).await?;
        let sent: BTreeMap<String, u8> = kv_json(&hub.store, thresholds_key("m1")).await?;
        assert_eq!((pause_at(&hex), sent.get(&hex).copied(), sent.get(&sends).copied()), (Some(90), Some(80), Some(80)), "the person's still, nothing sent to it");
        // A notice that didn't reach the coordinator is sent again (every 15 s, by the driver), and
        // kept as delivered once its session takes it.
        let mut s = settings(&hub, "m1").await?;
        s.notice = Some(Notice { delivered: false, message: None, why: Some("the coordinator's machine isn't connected right now".into()), ..notice.clone() });
        save_settings(&hub, "m1", &s).await?;
        enforce(&hub).await;
        let again = settings(&hub, "m1").await?.notice.ok_or_else(|| anyhow::anyhow!("no notice"))?;
        assert!(again.delivered && again.message.is_some() && again.why.is_none() && again.text == notice.text, "{again:?}");
        let kept = settings(&hub, "m1").await?;
        // Out of bounds, or a track it hasn't: refused, nothing kept.
        for bad in [json!({"program": "m1", "slots": 0}), json!({"program": "m1", "slots": 33}), json!({"program": "m1", "weights": {"T3": 0}}), json!({"program": "m1", "weights": {"T9": 2}})] {
            assert_eq!(refused(run(Caller::owner(), "programs.set", bad.clone()).await), Some(http::StatusCode::BAD_REQUEST), "{bad}");
        }
        assert_eq!(settings(&hub, "m1").await?, kept);
        // An agent reads them, and can't change them: as a session, or signed in as an agent.
        assert_eq!(refused(run(session(&coordinator), "programs.set", json!({"program": "m1", "slots": 6})).await), Some(http::StatusCode::FORBIDDEN));
        let signed_in = Caller::with(crate::authz::gate::Kind::Agent, "agent:token", None, None);
        assert_eq!(refused(run(signed_in, "programs.set", json!({"program": "m1", "weights": {"T7": 9}})).await), Some(http::StatusCode::FORBIDDEN));
        let read = ok(run(session(&coordinator), "programs.get", json!({"program": "m1"})).await)?;
        assert_eq!(read.get("slots").and_then(Value::as_u64), Some(1));
        // The weights were seeded once: an edit of the master plan's table changes none.
        hub.store.call(|c| Ok(c.execute("UPDATE plans SET body = '| Track | Weight |\n|---|---|\n| T7 Hex | 9 |\n' WHERE id = 'm1'", [])?)).await?;
        assert_eq!(track(&program().await?, "T7")?.weight, 4);
        ok(run(Caller::owner(), "programs.set", json!({"program": "m1", "slots": 6})).await)?;

        // Above 80% of a weekly window (Sends T3's account): no new goal is put under way, and the
        // track ready for one is held; above 95%, the rule pauses.
        report(&hub, &sends, |s| s.limits = weekly_at(85.0))?;
        let p = program().await?;
        assert!(matches!(&p.rule, Rule::Hold { .. }) && p.usage.as_ref().is_some_and(|u| (u.pct - 85.0).abs() < 1e-9), "{:?} {:?}", p.rule, p.usage);
        assert_eq!(track(&p, "T4")?.slot, "held");
        let e = run(session(&coordinator), "programs.assign", json!({"program": "m1", "track": "T7", "goal": t7_3})).await.err();
        assert!(e.as_ref().is_some_and(|(c, m)| *c == http::StatusCode::CONFLICT && m.contains("no new goal starts")), "{e:?}");
        report(&hub, &sends, |s| s.limits = weekly_at(96.0))?;
        assert!(matches!(&program().await?.rule, Rule::Pause { why } if why.starts_with(PAUSED_FOR_USAGE)));
        // Hex T7 paused at its session limit: T7 says so, with the window and when it starts over.
        let until_ms = crate::util::now_ms() + 3_600_000;
        report(&hub, &hex, |s| {
            s.pause = Some(crate::harness::model::Pause { reason: "limit".into(), window: Some("five_hour".into()), label: Some("Session".into()), until: Some(until_ms), by: "harness".into(), ..Default::default() });
        })?;
        let t7 = track(&program().await?, "T7")?;
        let pause = t7.worker.as_ref().and_then(|w| w.pause.clone()).ok_or_else(|| anyhow::anyhow!("T7's worker isn't paused"))?;
        assert_eq!((t7.slot.as_str(), pause.reason.as_str(), pause.window.as_deref(), pause.until), ("paused", "limit", Some("five_hour"), Some(until_ms)));

        // T7's goal met, then its worker unassigned: the goal's met in the coordinator's stack; the
        // threshold the person set stays, and kiln forgets its own.
        ok(run(session(&coordinator), "programs.assign", json!({"program": "m1", "track": "T7", "met": true})).await)?;
        let done = item(&stack(&coordinator).await?.items, "LAB-T7.2")?;
        assert_eq!((done.status.as_str(), done.on.as_deref()), ("met", None));
        assert_eq!(track(&program().await?, "T7")?.met, 2);
        let e = run(session(&coordinator), "programs.assign", json!({"program": "m1", "track": "T7", "met": true})).await.err();
        assert!(e.as_ref().is_some_and(|(c, m)| *c == http::StatusCode::CONFLICT && m.contains("no goal under way")), "nothing under way to say met: {e:?}");
        // Under 80% again: a goal of T7's that's met isn't handed over again, and its next is. Said
        // met in the coordinator's stack itself (as an agent's `done` says it), a change of worker
        // isn't refused for it, and the track has none under way.
        report(&hub, &sends, |s| s.limits = weekly_at(40.0))?;
        let e = run(session(&coordinator), "programs.assign", json!({"program": "m1", "track": "T7", "goal": t7_2})).await.err();
        assert!(e.as_ref().is_some_and(|(c, m)| *c == http::StatusCode::CONFLICT && m.contains("isn't one still to come")), "{e:?}");
        ok(run(session(&coordinator), "programs.assign", json!({"program": "m1", "track": "T7", "goal": t7_3})).await)?;
        let (c4, g4) = (coordinator.clone(), t7_3.clone());
        hub.store.call(move |c| Ok(c.execute("UPDATE goal_items SET status = 'met' WHERE agent = ?1 AND id = ?2", rusqlite::params![c4, g4])?)).await?;
        ok(run(session(&coordinator), "programs.assign", json!({"program": "m1", "track": "T7", "agent": hex})).await)?;
        assert_eq!(assignments(&hub.store, "m1").await?.get("T7").map(|a| (a.agent.as_str(), a.goal.clone())), Some((hex.as_str(), None)));
        ok(run(session(&coordinator), "programs.assign", json!({"program": "m1", "track": "T7", "agent": null})).await)?;
        assert!(!assignments(&hub.store, "m1").await?.contains_key("T7"));
        assert!(track(&program().await?, "T7")?.worker.is_none());
        enforce(&hub).await;
        let kept: BTreeMap<String, u8> = kv_json(&hub.store, thresholds_key("m1")).await?;
        assert_eq!((kept.get(&hex), pause_at(&hex)), (None, Some(90)));

        // A track whose goals all ended, met or not (T4's skipped), is done: no slot goes to it.
        let c5 = coordinator.clone();
        hub.store.call(move |c| Ok(c.execute("UPDATE goal_items SET status = 'skipped' WHERE agent = ?1 AND text LIKE 'LAB-T4.%'", rusqlite::params![c5])?)).await?;
        assert_eq!(track(&program().await?, "T4")?.slot, "done");
        // A program with no coordinator yet: the person assigning is whom its workers must work for.
        hub.store
            .call(|c| {
                plan_row(c, "m2", "Another program, master", "", &["other", "master"], 4)?;
                plan_row(c, "q1", "Another program T1: Docs", "", &["other", "t1", "docs"], 4)
            })
            .await?;
        let e = run(Caller::owner(), "programs.assign", json!({"program": "m2", "track": "T1", "agent": stranger})).await.err();
        assert!(e.as_ref().is_some_and(|(c, m)| *c == http::StatusCode::FORBIDDEN && m.contains("another person than you")), "{e:?}");
        ok(run(Caller::owner(), "programs.assign", json!({"program": "m2", "track": "T1", "agent": hex})).await)?;
        // Its master run as goals on a session kiln doesn't list any more: the program still knows
        // its coordinator by its id (and its notices go to it).
        hub.store.call(|c| Ok(c.execute("INSERT INTO plan_runs (id, plan_id, version, session, mode, at, by) VALUES ('r2', 'm2', 1, 'gone-coordinator', 'goals', 6, 'admin')", [])?)).await?;
        let m2 = get(&hub, "m2").await?.ok_or_else(|| anyhow::anyhow!("no program m2"))?;
        assert_eq!((m2.coordinator.is_none(), m2.coordinator_id.as_deref()), (true, Some("gone-coordinator")));
        Ok(())
    })
}
