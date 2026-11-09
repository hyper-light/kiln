//! Budgets' unit tests (LB08, W05-R11), and the model's sweeps with a planted mistake for each
//! property.

use super::model::{ALL, Checks, Plant, sweep};
use super::*;
use crate::sched::state::Cpus;

const HOUR: Ms = 3_600_000;

fn decl(limit_cores: u64) -> Decl {
    Decl { tenant: "acme/app".into(), per: HOUR, limit: [Some(limit_cores * 1000 * HOUR), None, None, None], require_timeout: true }
}

fn cores(n: u64) -> Resources {
    Resources { cpu: Cpus::whole(n), ..Resources::default() }
}

fn owner(me: &str, d: &Decl, shares: &[(&str, u64)]) -> Budgets {
    let mut b = Budgets::new(me, 1);
    let split = Split { key: d.key(0), shares: shares.iter().map(|(o, c)| ((*o).to_owned(), [c * 1000 * HOUR, 0, 0, 0])).collect() };
    let _ = b.declare(vec![d.clone()], vec![split], vec!["a".into(), "b".into()], 0);
    b
}

fn says(out: &[Do]) -> Vec<(String, Word)> {
    out.iter().filter_map(|d| if let Do::Say { to, word } = d { Some((to.clone(), word.clone())) } else { None }).collect()
}

fn released(out: &[Do], run: &str) -> bool {
    out.iter().any(|d| matches!(d, Do::Release { run: r } if r == run))
}

#[test]
fn lb08_a_split_is_even_with_what_doesnt_divide_to_the_first_by_name() -> Result<(), String> {
    let d = Decl { tenant: "acme".into(), per: 10, limit: [Some(10), None, None, Some(2)], require_timeout: true };
    let s = split(&d, 3, &["c".into(), "a".into(), "b".into(), "a".into()]).ok_or("a split")?;
    let shares: Vec<(String, Rights)> = s.shares.into_iter().collect();
    assert_eq!(shares, vec![("a".into(), [4, 0, 0, 1]), ("b".into(), [3, 0, 0, 1]), ("c".into(), [3, 0, 0, 0])]);
    assert_eq!(s.key, PotKey { tenant: "acme".into(), per: 10, period: 3 });
    assert!(split(&d, 3, &[]).is_none(), "no owner, no split");
    Ok(())
}

#[test]
fn w05_r11_a_budget_covers_its_repository_or_its_organizations() {
    let mut d = decl(1);
    assert!(d.covers("acme/app") && !d.covers("acme/app2") && !d.covers("acme"));
    d.tenant = "acme".into();
    assert!(d.covers("acme/app") && d.covers("acme/x") && !d.covers("acmex/app"));
}

#[test]
fn lb08_a_run_without_a_timeout_is_never_unless_the_person_said_it_may_be() {
    let d = decl(4);
    let mut a = owner("a", &d, &[("a", 4)]);
    let (gate, _) = a.ask("r1", "acme/app", &cores(1), None, 0);
    assert!(matches!(&gate, Gate::Never(Answer::Never { need, .. }) if need.contains("without a timeout")), "{gate:?}");
    let loose = Decl { require_timeout: false, ..decl(1) };
    let mut b = owner("a", &loose, &[("a", 1)]);
    // Without a timeout it reserves what it asks until the period ends: a quarter in, three
    // quarters of an hour of one CPU.
    let (gate, _) = b.ask("r1", "acme/app", &cores(1), None, i64::try_from(HOUR / 4).unwrap_or(0));
    assert_eq!(gate, Gate::Reserved);
    assert_eq!(b.reservation("r1").and_then(|r| r.holds.first()).map(|h| h.worst), Some([1000 * HOUR * 3 / 4, 0, 0, 0]));
    // Ended an hour later, past its period: what it used past its reservation is counted over.
    b.started("r1", 0);
    let _ = b.ended("r1", End::Ran, i64::try_from(2 * HOUR).unwrap_or(0));
    // It used two hours of a CPU: its reservation, then the quarter free, then three quarters over.
    let h = b.holding(&d.key(0));
    assert!(h.is_some_and(|h| h.spent[0] == 1000 * HOUR && h.over[0] == 1000 * HOUR && h.free == NONE), "{h:?}");
}

#[test]
fn lb08_a_run_whose_worst_case_is_past_the_whole_budget_is_never() {
    let d = decl(2);
    let mut a = owner("a", &d, &[("a", 2)]);
    let (gate, _) = a.ask("r1", "acme/app", &cores(3), Some(HOUR), 0);
    assert!(matches!(&gate, Gate::Never(Answer::Never { need, .. }) if need.contains("3.0 core-hours within acme/app's budget of 2.0 core-hours")), "{gate:?}");
    let (gate, _) = a.ask("r2", "other/repo", &cores(64), None, 0);
    assert_eq!(gate, Gate::Free, "no budget covers it");
}

#[test]
fn lb08_a_run_reserves_its_worst_case_and_settles_to_what_it_used() {
    let d = decl(4);
    let key = d.key(0);
    let mut a = owner("a", &d, &[("a", 4)]);
    let (gate, _) = a.ask("r1", "acme/app", &cores(2), Some(HOUR), 0);
    assert_eq!(gate, Gate::Reserved);
    assert_eq!(a.holding(&key).map(|h| h.free[0]), Some(2000 * HOUR));
    a.started("r1", 1_000);
    let _ = a.ended("r1", End::Ran, 1_000 + 600_000);
    let h = a.holding(&key);
    assert!(h.is_some_and(|h| h.spent[0] == 2000 * 600_000 && h.reserved == NONE && h.free[0] == 4000 * HOUR - 2000 * 600_000), "{h:?}");
    // Said again, the end settles nothing more.
    let _ = a.ended("r1", End::Ran, HOUR as i64);
    assert_eq!(a.holding(&key), h);
    // A lost run is charged its worst case; one never started nothing.
    let _ = a.ask("r2", "acme/app", &cores(1), Some(1_000), 0);
    let _ = a.ask("r3", "acme/app", &cores(1), Some(1_000), 0);
    let _ = a.ended("r2", End::Unknown, 5);
    a.go("r3");
    let _ = a.cancel("r3", 5);
    assert!(a.reservation("r3").is_some(), "a run said to start is settled by its end only");
    let _ = a.ended("r3", End::NotStarted, 6);
    assert_eq!(a.holding(&key).map(|h| h.spent[0]), Some(2000 * 600_000 + 1000 * 1_000));
}

#[test]
fn lb08_a_held_run_is_told_why_and_blocks_its_budgets_later_runs() {
    let d = decl(4);
    let mut a = owner("a", &d, &[("a", 3), ("b", 1)]);
    let (gate, out) = a.ask("big", "acme/app", &cores(4), Some(HOUR), 0);
    assert_eq!(gate, Gate::Held);
    let told = out.iter().find_map(|d| if let Do::Tell { run, answer } = d { (run == "big").then(|| answer.words(0)) } else { None });
    assert_eq!(told.as_deref(), Some("held: acme/app's budget: it reserves 4.0 core-hours, and 3.0 core-hours is free here this period"));
    // It asks the one peer that holds any, for its shortfall.
    let asked = says(&out);
    assert!(matches!(asked.as_slice(), [(to, Word::Need { need, since: 0, .. })] if to == "b" && need[0] == 1000 * HOUR), "{asked:?}");
    // A small run behind it waits for it, though it would fit.
    let (gate, out) = a.ask("small", "acme/app", &cores(1), Some(1_000), 1);
    assert_eq!(gate, Gate::Held);
    assert!(out.iter().any(|d| matches!(d, Do::Tell { run, answer } if run == "small" && answer.words(0).contains("1 of its runs held here before it"))));
    assert_eq!(a.timer(), Some(HOUR as i64), "the period's end is when it's due");
}

#[test]
fn lb08_shortfall_moves_rights_pairwise_the_giver_lowering_and_keeping_first() -> Result<(), String> {
    let d = decl(4);
    let key = d.key(0);
    let mut a = owner("a", &d, &[("a", 2), ("b", 2)]);
    let mut b = owner("b", &d, &[("a", 2), ("b", 2)]);
    let (_, out) = a.ask("r1", "acme/app", &cores(3), Some(HOUR), 0);
    let [(to, need)]: [(String, Word); 1] = says(&out).try_into().map_err(|w: Vec<(String, Word)>| format!("one need: {w:?}"))?;
    assert_eq!(to, "b");
    let given = b.heard("a", need, 10);
    // The giver's lowering is among what it keeps before its word goes.
    let kept = b.changes();
    assert!(kept.iter().any(|c| matches!(c, Change::Pot(k, Some(p)) if *k == key && p.gave.get("a") == Some(&[1000 * HOUR, 0, 0, 0]))), "{kept:?}");
    let [(to, give)]: [(String, Word); 1] = says(&given).try_into().map_err(|w: Vec<(String, Word)>| format!("one give: {w:?}"))?;
    assert_eq!(to, "a");
    assert_eq!(b.holding(&key).map(|h| h.free[0]), Some(1000 * HOUR));
    let out = a.heard("b", give.clone(), 20);
    assert!(released(&out, "r1"), "{out:?}");
    // Said twice, or an older total after, it changes nothing: totals merge by maximum.
    let once = a.holding(&key);
    let _ = a.heard("b", give, 30);
    let _ = a.heard("b", Word::Give { pot: key.clone(), total: NONE, free: NONE }, 31);
    assert_eq!(a.holding(&key), once);
    assert_eq!(once.map(|h| (h.got[0], h.free[0])), Some((1000 * HOUR, 0)));
    Ok(())
}

#[test]
fn lb08_the_oldest_run_gets_rights_from_a_peer_short_itself_and_no_two_wait_on_each_other() -> Result<(), String> {
    let d = decl(4);
    let mut a = owner("a", &d, &[("a", 2), ("b", 2)]);
    let mut b = owner("b", &d, &[("a", 2), ("b", 2)]);
    let (_, out_a) = a.ask("older", "acme/app", &cores(3), Some(HOUR), 0);
    let (_, out_b) = b.ask("younger", "acme/app", &cores(3), Some(HOUR), 5);
    let need_a = says(&out_a).into_iter().next().map(|(_, w)| w).ok_or("a asks")?;
    let need_b = says(&out_b).into_iter().next().map(|(_, w)| w).ok_or("b asks")?;
    // a's run is older: a keeps what it has from b's ask, and b gives a what a needs.
    let from_a = a.heard("b", need_b, 6);
    assert!(says(&from_a).iter().all(|(_, w)| matches!(w, Word::Give { total, .. } if nothing(total))), "{from_a:?}");
    let from_b = b.heard("a", need_a, 7);
    let give = says(&from_b).into_iter().next().map(|(_, w)| w).ok_or("b gives")?;
    assert!(matches!(&give, Word::Give { total, .. } if total[0] == 1000 * HOUR), "{give:?}");
    assert!(released(&a.heard("b", give, 8), "older"));
    let hoard = planted(Some(Mutant::HoardWhenShort), || {
        let mut a = owner("a", &d, &[("a", 2), ("b", 2)]);
        let mut b = owner("b", &d, &[("a", 2), ("b", 2)]);
        let (_, out_a) = a.ask("older", "acme/app", &cores(3), Some(HOUR), 0);
        let _ = b.ask("younger", "acme/app", &cores(3), Some(HOUR), 5);
        let need = says(&out_a).into_iter().next().map(|(_, w)| w);
        need.map(|n| b.heard("a", n, 7)).is_some_and(|o| says(&o).iter().all(|(_, w)| matches!(w, Word::Give { total, .. } if nothing(total))))
    });
    assert!(hoard, "planted: a peer short itself gives nothing");
    Ok(())
}

#[test]
fn lb08_a_period_boundary_is_a_due_time_and_brings_the_new_periods_share() -> Result<(), String> {
    let d = decl(4);
    let mut a = Budgets::new("a", 1);
    let splits: Vec<Split> = (0..2).filter_map(|p| split(&d, p, &["a".into()])).collect();
    let _ = a.declare(vec![d.clone()], splits, vec!["a".into()], 0);
    let _ = a.ask("r1", "acme/app", &cores(3), Some(HOUR), 0);
    let (gate, _) = a.ask("r2", "acme/app", &cores(3), Some(HOUR), 1);
    assert_eq!(gate, Gate::Held);
    let at = a.timer().ok_or("due at the period's end")?;
    assert_eq!(at, HOUR as i64);
    assert!(a.tick(at - 1).iter().all(|d| !matches!(d, Do::Release { .. })));
    let out = a.tick(at);
    assert!(released(&out, "r2"), "{out:?}");
    assert_eq!(a.reservation("r2").and_then(|r| r.holds.first()).map(|h| h.pot.period), Some(1));
    assert_eq!(a.timer(), None);
    Ok(())
}

#[test]
fn lb08_a_link_back_says_the_need_again_and_a_transfer_lost_with_it_arrives() -> Result<(), String> {
    let d = decl(4);
    let key = d.key(0);
    let mut a = owner("a", &d, &[("a", 2), ("b", 2)]);
    let mut b = owner("b", &d, &[("a", 2), ("b", 2)]);
    let (_, out) = a.ask("r1", "acme/app", &cores(3), Some(HOUR), 0);
    let need = says(&out).into_iter().next().map(|(_, w)| w).ok_or("a asks")?;
    // b gives, and the give is lost with the link.
    let _ = b.heard("a", need, 1);
    let _ = a.link("b", false, 2);
    let again = a.link("b", true, 3);
    let need = says(&again).into_iter().find(|(to, _)| to == "b").map(|(_, w)| w).ok_or("said again")?;
    let resent = b.heard("a", need, 4);
    let give = says(&resent).into_iter().next().map(|(_, w)| w).ok_or("b says what it gave")?;
    assert!(matches!(&give, Word::Give { total, .. } if total[0] == 1000 * HOUR), "{give:?}");
    assert!(released(&a.heard("b", give, 5), "r1"));
    assert_eq!(b.holding(&key).map(|h| h.gave[0]), Some(1000 * HOUR), "given once");
    Ok(())
}

#[test]
fn lb08_what_an_owner_keeps_brings_it_back_as_it_was() {
    let d = decl(4);
    let key = d.key(0);
    let mut a = owner("a", &d, &[("a", 3), ("b", 1)]);
    let _ = a.ask("r1", "acme/app", &cores(2), Some(HOUR), 0);
    let _ = a.ask("r2", "acme/app", &cores(2), Some(HOUR), 1);
    let rows: Vec<Row> = a
        .changes()
        .into_iter()
        .filter_map(|c| match c {
            Change::Pot(k, Some(p)) => Some(Row::Pot(k, p)),
            Change::Run(r, Some(x)) => Some(Row::Run(r, x)),
            Change::Held(r, Some(w)) => Some(Row::Held(r, w)),
            _ => None,
        })
        .collect();
    let back = Budgets::restore("a", 2, rows);
    assert_eq!(back.holding(&key).map(|h| (h.minted, h.reserved)), a.holding(&key).map(|h| (h.minted, h.reserved)));
    assert!(back.holds_back("r2") && back.reservation("r1").is_some());
}

#[test]
fn lb08_rights_in_words_and_the_words_on_the_wire() -> Result<(), String> {
    assert_eq!(words(&[2500 * HOUR, 0, 0, 0]), "2.5 core-hours");
    assert_eq!(words(&[1000 * 90_000, 1024 * 60_000 * 40, 0, 3 * 1000]), "2 core-minutes, 40 GiB-minutes of memory, 3 GPU-seconds");
    assert_eq!(words(&NONE), "nothing");
    let w = Word::Need { pot: PotKey { tenant: "acme".into(), per: 10, period: 2 }, epoch: 3, seq: 4, need: [1, 0, 0, 0], got: NONE, since: 5 };
    let json = serde_json::to_string(&w).map_err(|e| e.to_string())?;
    assert_eq!(json, r#"{"word":"need","pot":{"tenant":"acme","per":10,"period":2},"epoch":3,"seq":4,"need":[1,0,0,0],"got":[0,0,0,0],"since":5}"#);
    assert_eq!(serde_json::from_str::<Word>(&json).map_err(|e| e.to_string())?, w);
    Ok(())
}

const SEEDS: std::ops::Range<u64> = 0..40;
const STEPS: u32 = 400;

/// The model, clean: every property holds at every step, over seeds that hold runs back and
/// place many.
#[test]
fn lb08_model_zero_overspend_rights_conserved_and_progress_when_connected() -> Result<(), String> {
    let (held, placed) = sweep(SEEDS, STEPS, None, None, ALL)?;
    if held < 600 || placed < 600 {
        return Err(format!("too little contention to show anything: {held} steps held a run, {placed} runs placed"));
    }
    Ok(())
}

fn only(overspend: bool, conserved: bool, progress: bool) -> Checks {
    Checks { overspend, conserved, progress }
}

#[test]
fn lb08_planted_give_before_lower_overspends() {
    let r = sweep(SEEDS, STEPS, None, Some(Mutant::GiveBeforeLower), only(true, false, false));
    assert!(r.as_ref().is_err_and(|e| e.contains("overspent")), "{r:?}");
}

#[test]
fn lb08_planted_settle_twice_breaks_the_books() {
    let r = sweep(SEEDS, STEPS, None, Some(Mutant::SettleTwice), only(false, true, false));
    assert!(r.as_ref().is_err_and(|e| e.contains("books")), "{r:?}");
}

#[test]
fn lb08_planted_hoarding_when_short_stops_progress() {
    let r = sweep(SEEDS, STEPS, None, Some(Mutant::HoardWhenShort), only(false, false, true));
    assert!(r.as_ref().is_err_and(|e| e.contains("held while")), "{r:?}");
}

#[test]
fn lb08_planted_publishing_before_keeping_overspends_across_a_crash() {
    let r = sweep(SEEDS, STEPS, Some(Plant::PublishBeforeKeep), None, only(true, false, false));
    assert!(r.as_ref().is_err_and(|e| e.contains("overspent")), "{r:?}");
}
