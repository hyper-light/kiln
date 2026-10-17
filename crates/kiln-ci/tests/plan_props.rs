//! docs/ci.md W04-AC02: random matrices (up to 6 axes of up to 6 values, 8 `include` and 8
//! `exclude` entries) never panic; no two legs are equal; every original combination no `exclude`
//! entry matches appears exactly once; and a matrix over the limit is an error, never more legs.

use kiln_ci::expr::Value;
use kiln_ci::plan::matrix::{Combo, expand, get, same, same_combo};
use proptest::prelude::*;

fn scalar() -> impl Strategy<Value = Value> {
    prop_oneof![(0u8..4).prop_map(|i| Value::str(&format!("x{i}"))), (0u8..4).prop_map(|i| Value::Number(f64::from(i))), any::<bool>().prop_map(Value::Bool)]
}

fn entry(keys: &'static [&'static str]) -> impl Strategy<Value = Value> {
    prop::collection::vec((prop::sample::select(keys), scalar()), 1..=3).prop_map(|kv| {
        let mut seen: Vec<(String, Value)> = Vec::new();
        for (k, v) in kv {
            if !seen.iter().any(|(x, _)| x == k) {
                seen.push((k.to_owned(), v));
            }
        }
        Value::object(seen)
    })
}

const AXES: &[&str] = &["a", "b", "c", "d", "e", "f"];
const KEYS: &[&str] = &["a", "b", "c", "d", "e", "f", "p", "q"];

fn axes() -> impl Strategy<Value = Vec<(String, Vec<Value>)>> {
    (0usize..=6).prop_flat_map(|n| {
        prop::collection::vec(prop::collection::vec(scalar(), 1..=6), n).prop_map(|vals| {
            vals.into_iter()
                .enumerate()
                .map(|(i, vs)| {
                    // Distinct values per axis: a value written twice is two legs, as on GitHub.
                    let mut d: Vec<Value> = Vec::new();
                    for v in vs {
                        if !d.iter().any(|x| same(x, &v)) {
                            d.push(v);
                        }
                    }
                    (AXES.get(i).copied().unwrap_or("z").to_owned(), d)
                })
                .collect()
        })
    })
}

fn product(axes: &[(String, Vec<Value>)]) -> Vec<Combo> {
    if axes.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<Combo> = vec![Vec::new()];
    for (k, vals) in axes {
        out = out.into_iter().flat_map(|c| vals.iter().map(move |v| { let mut c2 = c.clone(); c2.push((k.clone(), v.clone())); c2 })).collect();
    }
    out
}

proptest! {
    #![proptest_config(ProptestConfig { cases: if cfg!(debug_assertions) { 2_000 } else { 20_000 }, ..ProptestConfig::default() })]

    #[test]
    fn w04_ac02_random_matrices(axes in axes(), include in prop::collection::vec(entry(KEYS), 0..=8), exclude in prop::collection::vec(entry(AXES), 0..=8), limit in prop::sample::select(vec![8usize, 64, 256])) {
        match expand(&axes, &include, &exclude, limit) {
            Err(e) => prop_assert!(e.contains("at most"), "{e}"),
            Ok(legs) => {
                prop_assert!(legs.len() <= limit);
                for (i, a) in legs.iter().enumerate() {
                    for b in legs.iter().skip(i + 1) {
                        prop_assert!(!same_combo(a, b), "two equal legs: {a:?}");
                    }
                }
                let excluded = |c: &Combo| exclude.iter().any(|e| match e { Value::Object(m) => m.0.iter().all(|(k, v)| get(c, k).is_some_and(|x| same(x, v))), _ => false });
                for c in product(&axes).iter().filter(|c| !excluded(c)) {
                    let n = legs.iter().filter(|l| c.iter().all(|(k, v)| get(l, k).is_some_and(|x| same(x, v))) && axes.iter().all(|(k, _)| get(l, k).is_some())).count();
                    prop_assert_eq!(n, 1, "original {:?} appears {} times", c, n);
                }
            }
        }
    }
}
