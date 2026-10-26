//! W24-R31's reaper: its properties over random caches, volumes and policies (AC15's pure half).

use super::*;
use proptest::prelude::*;

fn entry() -> impl Strategy<Value = Entry> {
    (0u8..40, 0u8..5, 1u64..(8 * GIB), 0i64..(60 * 86_400), 0u64..600_000).prop_map(|(i, g, bytes, age, saved_ms)| Entry { id: format!("e{i}"), group: format!("lane{g}"), bytes, used: 100 * 86_400 - age, saved_ms })
}

fn entries() -> impl Strategy<Value = Vec<Entry>> {
    proptest::collection::vec(entry(), 0..40).prop_map(|mut v| {
        let mut seen = BTreeSet::new();
        v.retain(|e| seen.insert(e.id.clone()));
        v
    })
}

fn size() -> impl Strategy<Value = Option<Size>> {
    prop_oneof![Just(None), (0u32..5_000).prop_map(|b| Some(Size::Share(b))), (0u64..(200 * GIB)).prop_map(|b| Some(Size::Bytes(b)))]
}

fn policy() -> impl Strategy<Value = Policy> {
    (size(), size(), size(), proptest::option::of(0u32..5), proptest::option::of(0u64..(3 * 86_400)), proptest::option::of(86_400u64..(40 * 86_400))).prop_map(|(floor, ceiling, target, keep_last, keep_within, max_age)| Policy { floor, ceiling, target, keep_last, keep_within, max_age, schedule: None })
}

const NOW: i64 = 100 * 86_400;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10_000))]

    /// W24-R31: a reap never ends below the floor while anything is left to evict; keep rules hold
    /// unless the floor needs them (and then everything unprotected went first); what `max_age`
    /// drops always goes; and the same inputs, in any order, give the same evictions.
    #[test]
    fn w24_r31_floor_keep_and_determinism(es in entries(), volume in (10 * GIB)..(4_000 * GIB), used_by_others in 0u64..(4_000 * GIB), layers in proptest::collection::vec((0u8..4, policy()), 0..5)) {
        let scopes = [Scope::System, Scope::Runner, Scope::Repo, Scope::Session];
        let layers: Vec<(Scope, Policy)> = layers.into_iter().filter_map(|(s, p)| scopes.get(usize::from(s)).map(|s| (*s, p))).collect();
        let refs: Vec<(Scope, &Policy)> = layers.iter().map(|(s, p)| (*s, p)).collect();
        let r = resolve(volume, &refs);
        let held: u64 = es.iter().map(|e| e.bytes).sum();
        let free = volume.saturating_sub(held).saturating_sub(used_by_others);
        let out = plan(&r, &es, free, held, NOW);
        let freed: u64 = out.iter().map(|e| e.bytes).sum();
        let after = free.saturating_add(freed);
        prop_assert!(after >= r.floor || out.len() == es.len(), "below the floor with entries left");
        let prot = protected(&r, &es, NOW);
        let gone: BTreeSet<&str> = out.iter().map(|e| e.id.as_str()).collect();
        if out.iter().any(|e| e.reason == Reason::Floor) {
            prop_assert!(es.iter().filter(|e| !prot.contains(e.id.as_str())).all(|e| gone.contains(e.id.as_str())), "a protected entry went before an unprotected one");
        }
        for e in &out {
            if prot.contains(e.id.as_str()) {
                prop_assert_eq!(e.reason, Reason::Floor);
            }
        }
        let max_age = i64::try_from(r.max_age).unwrap_or(i64::MAX);
        for e in es.iter().filter(|e| !prot.contains(e.id.as_str()) && NOW - e.used > max_age) {
            prop_assert!(gone.contains(e.id.as_str()), "{} is past max_age", e.id);
        }
        let mut shuffled = es.clone();
        shuffled.reverse();
        prop_assert_eq!(plan(&r, &shuffled, free, held, NOW), out);
    }

    /// W24-R31's precedence: hardware limits only from system and runner, the stricter winning; for
    /// what to keep, the most specific scope that sets a key.
    #[test]
    fn w24_r31_precedence(volume in (10 * GIB)..(4_000 * GIB), layers in proptest::collection::vec((0u8..4, policy()), 0..6)) {
        let scopes = [Scope::System, Scope::Runner, Scope::Repo, Scope::Session];
        let layers: Vec<(Scope, Policy)> = layers.into_iter().filter_map(|(s, p)| scopes.get(usize::from(s)).map(|s| (*s, p))).collect();
        let refs: Vec<(Scope, &Policy)> = layers.iter().map(|(s, p)| (*s, p)).collect();
        let r = resolve(volume, &refs);
        let hw: Vec<&Policy> = layers.iter().filter(|(s, _)| matches!(s, Scope::System | Scope::Runner)).map(|(_, p)| p).collect();
        let floors: Vec<u64> = hw.iter().filter_map(|p| p.floor.map(|f| f.bytes(volume))).collect();
        prop_assert_eq!(r.floor, floors.iter().copied().max().unwrap_or((volume / 10).min(100 * GIB)));
        let ceilings: Vec<u64> = hw.iter().filter_map(|p| p.ceiling.map(|c| c.bytes(volume))).collect();
        prop_assert_eq!(r.ceiling, ceilings.iter().copied().min().unwrap_or((volume / 4).min(300 * GIB)));
        prop_assert!(r.target >= r.floor);
        let mut by_scope = layers.clone();
        by_scope.sort_by_key(|(s, _)| std::cmp::Reverse(*s));
        let want = by_scope.iter().find_map(|(_, p)| p.keep_last).unwrap_or(2);
        prop_assert_eq!(r.keep_last, want);
    }
}

#[test]
fn w24_r31_sizes_and_schedules() {
    assert_eq!(Size::parse("20GB"), Ok(Size::Bytes(20_000_000_000)));
    assert_eq!(Size::parse("1GiB"), Ok(Size::Bytes(GIB)));
    assert_eq!(Size::parse("12.5%"), Ok(Size::Share(1250)));
    assert!(Size::parse("lots").is_err() && Size::parse("120%").is_err());
    assert_eq!(Size::Share(1000).bytes(1_000), 100);
    // 2026-10-26T00:00:00Z is a Monday: daily at 03:30, then Sundays at noon.
    let t = 1_792_972_800;
    assert_eq!(next_after(&Schedule::Cron("30 3 * * *".into()), t), Some(t + 3 * 3600 + 1800));
    assert_eq!(next_after(&Schedule::Cron("0 12 * * 0".into()), t), Some(t + 6 * 86_400 + 12 * 3600));
    assert_eq!(next_after(&Schedule::Every(6 * 3600), t), Some(t + 6 * 3600));
    assert_eq!(next_after(&Schedule::Cron("61 * * * *".into()), t), None);
    let r = resolve(100 * GIB, &[]);
    assert_eq!(due(&r, r.floor - 1, 0, 0, Some(t), t), Some(Trigger::Pressure));
    assert_eq!(due(&r, r.floor, r.ceiling, 1, Some(t), t), Some(Trigger::Ceiling));
    assert_eq!(due(&r, r.floor, 0, 0, Some(t), t + 86_400), Some(Trigger::Schedule));
    assert_eq!(due(&r, r.floor, 0, 0, Some(t), t + 60), None);
}
