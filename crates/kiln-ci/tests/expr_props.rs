//! W01-AC03: parsing and evaluating never panic or overflow, every bound is reported by name,
//! and printing a parsed tree parses back to the same tree.

use kiln_ci::expr::{Budget, Dialect, Expr, MapContexts, Template, Value, eval};
use proptest::prelude::*;

/// A small deterministic generator (splitmix64), so the large loops are reproducible.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        let n = u64::try_from(xs.len()).unwrap_or(1).max(1);
        xs.get(usize::try_from(self.next() % n).unwrap_or(0)).copied().unwrap_or("")
    }
}

const PIECES: &[&str] = &[
    "github", ".", "event", "*", "[", "]", "(", ")", "'x'", "'it''s'", "1", "-2.5e3", "0xFF", "NaN", "Infinity", "null", "true", "false",
    "==", "!=", "<", "<=", ">", ">=", "&&", "||", "!", ",", "contains(", "format(", "fromJSON(", "toJSON(", "join(", "case(", "'{0}'",
    "matrix", "steps", "needs", " ", "\n", "=", "&", "|", "@", "😊", "\"", "${{", "}}", "'unterminated", "a-b", "_x",
];

fn contexts() -> MapContexts {
    let event = Value::object(vec![("name".to_owned(), Value::str("e")), ("list".to_owned(), Value::array(vec![Value::Number(1.0), Value::str("2")]))]);
    MapContexts::default()
        .with("github", Value::object(vec![("event".to_owned(), event)]))
        .with("matrix", Value::object(vec![("os".to_owned(), Value::str("linux"))]))
}

#[test]
fn w01_ac03_random_token_soup_never_panics() {
    let mut rng = Rng(7);
    let cx = contexts();
    // 200,000 in debug builds, 1,000,000 in release ones.
    let n = if cfg!(debug_assertions) { 200_000 } else { 1_000_000 };
    for _ in 0..n {
        let len = 1 + (rng.next() % 12);
        let mut s = String::new();
        for _ in 0..len {
            s.push_str(rng.pick(PIECES));
        }
        for d in [Dialect::Gha, Dialect::Kiln] {
            if let Ok(e) = Expr::parse(&s, d) {
                let mut b = Budget { steps: 10_000, result_bytes: 1 << 16 };
                let _ = eval(&e, &cx, &mut b);
            }
            let _ = Template::parse(&s, d);
        }
    }
}

#[test]
fn w01_ac03_deep_and_long_inputs_are_bounded_by_name() {
    let deep = "!".repeat(5_000) + "true";
    assert_eq!(Expr::parse(&deep, Dialect::Gha).err().map(|e| e.code), Some("KCI2012"));
    let wide = vec!["contains('a','b')"; 2_000].join(" || ");
    assert_eq!(Expr::parse(&wide, Dialect::Gha).err().map(|e| e.code), Some("KCI2001"), "over 21,000 characters");
    let brackets = "a".to_owned() + &"[0]".repeat(100);
    assert_eq!(Expr::parse(&brackets, Dialect::Gha).err().map(|e| e.code), Some("KCI2012"));
}

/// Expressions built from the grammar, so most of them parse.
fn expr_strategy() -> impl Strategy<Value = String> {
    let leaf = prop_oneof![
        Just("null".to_owned()),
        Just("true".to_owned()),
        any::<i32>().prop_map(|n| n.to_string()),
        (-1e6f64..1e6).prop_map(|x| format!("{x}")),
        "[a-z]{0,6}".prop_map(|s| format!("'{s}'")),
        Just("github.event.name".to_owned()),
        Just("github.event.list[1]".to_owned()),
        Just("github.event.*".to_owned()),
        Just("matrix.os".to_owned()),
    ];
    leaf.prop_recursive(4, 48, 6, |inner| {
        prop_oneof![
            (inner.clone(), prop_oneof!["==", "!=", "<", "<=", ">", ">=", "&&", "||"], inner.clone()).prop_map(|(a, op, b)| format!("{a} {op} {b}")),
            inner.clone().prop_map(|a| format!("!({a})")),
            inner.clone().prop_map(|a| format!("({a})")),
            (inner.clone(), inner.clone()).prop_map(|(a, b)| format!("contains({a}, {b})")),
            (inner.clone(), inner.clone()).prop_map(|(a, b)| format!("format('{{0}}-{{1}}', {a}, {b})")),
            inner.clone().prop_map(|a| format!("toJSON({a})")),
            (inner.clone(), inner.clone(), inner).prop_map(|(a, b, c)| format!("case({a} == {b}, {b}, {c})")),
        ]
    })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 2_000, .. ProptestConfig::default() })]

    #[test]
    fn w01_ac03_printing_round_trips(src in expr_strategy()) {
        if let Ok(e) = Expr::parse(&src, Dialect::Gha) {
            let again = Expr::parse(&e.print(), Dialect::Gha);
            prop_assert!(again.is_ok(), "{} reprinted as {} fails: {:?}", src, e.print(), again.err());
            if let Ok(again) = again {
                prop_assert_eq!(e.ast(), again.ast(), "{} → {}", src, e.print());
            }
            let mut b = Budget { steps: 50_000, result_bytes: 1 << 20 };
            let _ = eval(&e, &contexts(), &mut b);
        }
    }

    #[test]
    fn w01_ac03_arbitrary_text_never_panics(s in "\\PC{0,64}") {
        let _ = Expr::parse(&s, Dialect::Gha);
        let _ = Expr::parse(&s, Dialect::Kiln);
        let _ = Template::parse(&s, Dialect::Gha);
    }
}
