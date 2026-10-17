//! W01's unit tests: the scenarios (W01-S01 to S08), the coercion table (W01-AC02), templates
//! (AC04), `if:` (AC05), the kiln dialect (AC08), references (R12) and bounds (R13). GitHub's own
//! test vectors run in `tests/oracle.rs` (AC01).

use super::*;

fn obj(pairs: &[(&str, Value)]) -> Value {
    Value::object(pairs.iter().map(|(k, v)| ((*k).to_owned(), v.clone())))
}

fn github() -> Value {
    obj(&[
        ("event_name", Value::str("push")),
        ("ref", Value::str("refs/heads/main")),
        ("ref_name", Value::str("main")),
        ("repository", Value::str("hyper-light/kiln")),
        ("sha", Value::str("abc123")),
        ("event", obj(&[("pull_request", obj(&[("labels", Value::array(vec![obj(&[("name", Value::str("bug"))]), obj(&[("name", Value::str("ci"))])]))]))])),
    ])
}

fn cx() -> MapContexts {
    MapContexts::default()
        .with("github", github())
        .with("matrix", obj(&[("os", Value::str("linux"))]))
        .with("inputs", obj(&[("flag", Value::Bool(false))]))
        .with("needs", obj(&[("plan", obj(&[("outputs", obj(&[("shards", Value::str("[\"core\",\"cli\",\"ui\"]"))]))]))]))
}

fn ev(src: &str) -> Result<Value, ExprError> {
    eval(&Expr::parse(src, Dialect::Gha)?, &cx(), &mut Budget::default())
}

fn ev_kiln(src: &str, c: &dyn Contexts) -> Result<Value, ExprError> {
    eval(&Expr::parse(src, Dialect::Kiln)?, c, &mut Budget::default())
}

#[test]
fn w01_s01_strings_compare_ignoring_case() -> Result<(), ExprError> {
    assert_eq!(ev("github.event_name == 'PUSH'")?, Value::Bool(true));
    assert_eq!(ev("GitHub.Event_Name == 'push'")?, Value::Bool(true), "contexts and properties ignore case");
    Ok(())
}

#[test]
fn w01_s02_filters_and_contains() -> Result<(), ExprError> {
    assert_eq!(ev("contains(github.event.pull_request.labels.*.name, 'Bug')")?, Value::Bool(true));
    assert_eq!(ev("contains(github.event.pull_request.labels.*.name, 'docs')")?, Value::Bool(false));
    assert_eq!(ev("join(github.event.pull_request.labels.*.name, ' + ')")?, Value::str("bug + ci"));
    Ok(())
}

#[test]
fn w01_s03_format_escapes() -> Result<(), ExprError> {
    assert_eq!(ev("format('{0}-{{x}}', 'a')")?, Value::str("a-{x}"));
    assert_eq!(ev("format('{0} {1} {0}', 'a', 1.5)")?, Value::str("a 1.5 a"));
    assert!(ev("format('{1}', 'a')").is_err());
    Ok(())
}

#[test]
fn w01_s04_loose_equality() -> Result<(), ExprError> {
    assert_eq!(ev("'1' == 1")?, Value::Bool(true));
    assert_eq!(ev("null == 0")?, Value::Bool(true));
    assert_eq!(ev("'' == 0")?, Value::Bool(true));
    assert_eq!(ev("'abc' < 'abd'")?, Value::Bool(true));
    assert_eq!(ev("fromJSON('[1]') == fromJSON('[1]')")?, Value::Bool(false), "arrays equal only themselves");
    assert_eq!(ev("github.event == github.event")?, Value::Bool(true));
    Ok(())
}

#[test]
fn w01_s05_logic_returns_operands() -> Result<(), ExprError> {
    assert_eq!(ev("inputs.flag && 'yes' || 'no'")?, Value::str("no"));
    assert_eq!(ev("matrix.os && 'yes' || 'no'")?, Value::str("yes"));
    assert_eq!(ev("matrix.missing || matrix.os")?, Value::str("linux"));
    Ok(())
}

#[test]
fn w01_s06_literal_text_in_if_is_a_string() -> Result<(), ExprError> {
    let opts = ParseOptions::permissive(Dialect::Gha);
    let c = Condition::parse("${{ github.ref }} == 'refs/heads/other'", &opts)?;
    assert!(c.has_literal_text());
    assert!(eval_condition(&c, &cx(), &mut Budget::default())?, "a non-empty string is truthy: the pitfall");
    let c = Condition::parse("${{ github.ref == 'refs/heads/other' }}", &opts)?;
    assert!(!c.has_literal_text());
    assert!(!eval_condition(&c, &cx(), &mut Budget::default())?);
    Ok(())
}

#[test]
fn w01_s07_to_json_pretty_prints() -> Result<(), ExprError> {
    assert_eq!(ev("toJSON(matrix)")?, Value::str("{\n  \"os\": \"linux\"\n}"));
    assert_eq!(ev("toJSON(fromJSON('[]'))")?, Value::str("[]"));
    Ok(())
}

struct Changed;

impl Contexts for Changed {
    fn get(&self, name: &str) -> Option<Value> {
        (name.eq_ignore_ascii_case("github")).then(github)
    }
    fn status(&self) -> StatusView {
        StatusView::default()
    }
    fn changed(&self, globs: &[String]) -> Result<bool, ExprError> {
        Ok(globs.iter().any(|g| g.starts_with("crates")))
    }
}

#[test]
fn w01_s08_kiln_changed_and_bare_names() -> Result<(), ExprError> {
    assert_eq!(ev_kiln("changed('crates/**') && branch == 'main'", &Changed)?, Value::Bool(true));
    assert_eq!(ev_kiln("event == \"push\" && repo == 'hyper-light/kiln' && sha == 'abc123'", &Changed)?, Value::Bool(true));
    Ok(())
}

#[test]
fn w01_ac02_coercion_table() -> Result<(), ExprError> {
    // Rows and columns: null, true, 1, '1', an array, an object. Each array and object is the
    // same instance on both sides of the diagonal.
    let a = Value::array(vec![Value::Number(1.0)]);
    let o = obj(&[("a", Value::Number(1.0))]);
    let vals = [Value::Null, Value::Bool(true), Value::Number(1.0), Value::str("1"), a, o];
    #[rustfmt::skip]
    let eq = [
        [1, 0, 0, 0, 0, 0],
        [0, 1, 1, 1, 0, 0],
        [0, 1, 1, 1, 0, 0],
        [0, 1, 1, 1, 0, 0],
        [0, 0, 0, 0, 1, 0],
        [0, 0, 0, 0, 0, 1],
    ];
    #[rustfmt::skip]
    let lt = [
        [0, 1, 1, 1, 0, 0],
        [0, 0, 0, 0, 0, 0],
        [0, 0, 0, 0, 0, 0],
        [0, 0, 0, 0, 0, 0],
        [0, 0, 0, 0, 0, 0],
        [0, 0, 0, 0, 0, 0],
    ];
    #[rustfmt::skip]
    let gt = [
        [0, 0, 0, 0, 0, 0],
        [1, 0, 0, 0, 0, 0],
        [1, 0, 0, 0, 0, 0],
        [1, 0, 0, 0, 0, 0],
        [0, 0, 0, 0, 0, 0],
        [0, 0, 0, 0, 0, 0],
    ];
    for (i, l) in vals.iter().enumerate() {
        for (j, r) in vals.iter().enumerate() {
            let at = |m: &[[u8; 6]; 6]| m.get(i).and_then(|row| row.get(j)).copied() == Some(1);
            let c = MapContexts::default().with("l", l.clone()).with("r", r.clone());
            let run = |op: &str| -> Result<bool, ExprError> { Ok(eval(&Expr::parse(&format!("l {op} r"), Dialect::Gha)?, &c, &mut Budget::default())?.truthy()) };
            assert_eq!(run("==")?, at(&eq), "{l:?} == {r:?}");
            assert_eq!(run("!=")?, !at(&eq), "{l:?} != {r:?}");
            assert_eq!(run("<")?, at(&lt), "{l:?} < {r:?}");
            assert_eq!(run(">")?, at(&gt), "{l:?} > {r:?}");
            assert_eq!(run("<=")?, at(&eq) || at(&lt), "{l:?} <= {r:?}");
            assert_eq!(run(">=")?, at(&eq) || at(&gt), "{l:?} >= {r:?}");
        }
    }
    // NaN compares false with everything, including itself; `!=` is then true.
    assert_eq!(ev("NaN == NaN")?, Value::Bool(false));
    assert_eq!(ev("NaN != NaN")?, Value::Bool(true));
    assert_eq!(ev("'abc' == 0")?, Value::Bool(false));
    Ok(())
}

#[test]
fn w01_ac04_templates() -> Result<(), ExprError> {
    let t = Template::parse("a ${{ matrix.os }} b ${{ 'x}}y' }} c", Dialect::Gha)?;
    assert_eq!(render(&t, &cx(), &mut Budget::default())?, "a linux b x}}y c");
    let err = Template::parse("x ${{ open", Dialect::Gha).err();
    assert_eq!(err.map(|e| (e.code, e.column)), Some(("KCI2002", Some(3))));
    let single = Template::parse("${{ fromJSON(needs.plan.outputs.shards) }}", Dialect::Gha)?;
    let v = eval_value(&single, &cx(), &mut Budget::default())?;
    assert_eq!(v.items().map(<[Value]>::len), Some(3), "a single expression keeps its type");
    assert_eq!(render(&Template::parse("${{ 1.0 }}|${{ true }}|${{ null }}|${{ matrix }}", Dialect::Gha)?, &cx(), &mut Budget::default())?, "1|true||Object");
    Ok(())
}

#[test]
fn w01_ac05_if_semantics() -> Result<(), ExprError> {
    let failed = MapContexts { status: StatusView { failed: true, ..StatusView::default() }, ..cx() };
    let cancelled = MapContexts { status: StatusView { cancelled: true, ..StatusView::default() }, ..cx() };
    let p = |s: &str| Expr::parse(s, Dialect::Gha);
    let b = &mut Budget::default();
    assert!(eval_if(&p("true")?, &cx(), b)?);
    assert!(!eval_if(&p("true")?, &failed, b)?, "success() is implied");
    assert!(eval_if(&p("always()")?, &failed, b)?);
    assert!(eval_if(&p("failure()")?, &failed, b)?);
    assert!(!eval_if(&p("failure()")?, &cancelled, b)?);
    assert!(eval_if(&p("cancelled()")?, &cancelled, b)?);
    assert!(eval_if(&p("!cancelled() || true")?, &cancelled, b)?, "a status function anywhere disables the implied success()");
    assert!(!eval_if(&p("success()")?, &cancelled, b)?);
    Ok(())
}

#[test]
fn w01_ac08_kiln_functions_only_in_the_kiln_dialect() {
    for src in ["changed('x')", "matches('a', 'a')", "coalesce(null, 'a')", "split('a,b', ',')", "replace('a', 'a', 'b')", "lower('A')", "upper('a')", "trim(' a ')", "fromYAML('a: 1')", "toYAML(1)"] {
        let gha = Expr::parse(src, Dialect::Gha).err().map(|e| e.code);
        assert_eq!(gha, Some("KCI5001"), "{src}");
        assert!(Expr::parse(src, Dialect::Kiln).is_ok(), "{src}");
    }
    assert!(Expr::parse("\"x\" == 'x'", Dialect::Gha).is_err(), "double quotes are kiln's");
    assert!(Expr::parse("\"x\" == 'x'", Dialect::Kiln).is_ok());
}

#[test]
fn kiln_functions_compute() -> Result<(), ExprError> {
    let c = MapContexts::default();
    assert_eq!(ev_kiln("coalesce(null, '', 'a', 'b')", &c)?, Value::str("a"));
    assert_eq!(ev_kiln("join(split('a,b,c', ','), '|')", &c)?, Value::str("a|b|c"));
    assert_eq!(ev_kiln("replace('a-b-c', '-', '_')", &c)?, Value::str("a_b_c"));
    assert_eq!(ev_kiln("matches('v1.2.3', '^v[0-9]+')", &c)?, Value::Bool(true));
    assert_eq!(ev_kiln("fromYAML('a: [1, 2]').a[1]", &c)?, Value::Number(2.0));
    assert_eq!(ev_kiln("trim(upper(' x '))", &c)?, Value::str("X"));
    Ok(())
}

#[test]
fn case_picks_the_first_true_predicate() -> Result<(), ExprError> {
    assert_eq!(ev("case(matrix.os == 'macos', 'mac', matrix.os == 'linux', 'lin', 'other')")?, Value::str("lin"));
    assert_eq!(ev("case(false, 1, 2)")?, Value::Number(2.0));
    assert!(ev("case('x', 1, 2)").is_err());
    assert_eq!(Expr::parse("case(true, 1)", Dialect::Gha).err().map(|e| e.code), Some("KCI2007"), "too few comes first");
    assert_eq!(Expr::parse("case(true, 1, false, 2)", Dialect::Gha).err().map(|e| e.code), Some("KCI2009"));
    Ok(())
}

#[test]
fn w01_r12_references() -> Result<(), ExprError> {
    let e = Expr::parse("secrets.NPM_TOKEN != '' && needs.build.outputs['digest'] && contains(github.event.*.name, matrix[inputs.key])", Dialect::Gha)?;
    let refs: Vec<String> = e
        .references()
        .iter()
        .map(|p| {
            p.0.iter()
                .map(|s| match s {
                    PathSeg::Name(n) => n.clone(),
                    PathSeg::Index(i) => i.to_string(),
                    PathSeg::Any => "*".to_owned(),
                })
                .collect::<Vec<_>>()
                .join(".")
        })
        .collect();
    for want in ["secrets.NPM_TOKEN", "needs.build.outputs.digest", "github.event.*.name", "matrix.*", "inputs.key"] {
        assert!(refs.iter().any(|r| r == want), "{want} in {refs:?}");
    }
    assert!(!e.uses_status_function());
    assert!(Expr::parse("success() || x", Dialect::Gha)?.uses_status_function());
    Ok(())
}

#[test]
fn w01_r13_bounds() -> Result<(), ExprError> {
    let long = "1".repeat(21_001);
    assert_eq!(Expr::parse(&long, Dialect::Gha).err().map(|e| e.code), Some("KCI2001"));
    let deep = format!("{}1{}", "(".repeat(60), ")".repeat(60));
    assert_eq!(Expr::parse(&deep, Dialect::Gha).err().map(|e| e.code), Some("KCI2012"));
    let e = Expr::parse("contains('a', 'b') || contains('a', 'b') || contains('a', 'b')", Dialect::Gha)?;
    let mut tiny = Budget { steps: 3, result_bytes: 1 << 20 };
    assert_eq!(eval(&e, &cx(), &mut tiny).err().map(|e| e.code), Some("KCI2021"));
    Ok(())
}

#[test]
fn contexts_can_be_limited_per_key() -> Result<(), ExprError> {
    let opts = ParseOptions { dialect: Dialect::Gha, contexts: Some(vec!["github".into(), "needs".into()]), status_functions: true, hash_files: false };
    assert!(Expr::parse_with("github.ref && needs.a.result", &opts).is_ok());
    let err = Expr::parse_with("secrets.X == ''", &opts).err();
    assert_eq!(err.as_ref().map(|e| e.code), Some("KCI3001"));
    assert_eq!(err.map(|e| e.located("secrets.X == ''")).as_deref(), Some("Unrecognized named-value: 'secrets'. Located at position 1 within expression: secrets.X == ''"));
    assert_eq!(Expr::parse_with("hashFiles('x')", &opts).err().map(|e| e.code), Some("KCI2006"));
    Ok(())
}

#[test]
fn printing_parses_back_to_the_same_tree() -> Result<(), ExprError> {
    for src in ["a.b[0].c == 'x''y' && !(b || c) || d", "contains(fromJSON('[1,2]'), 0x02)", "x.*.y[*] >= -1.5e-7", "case(a, 1, b, 2, 3)"] {
        let e = Expr::parse(src, Dialect::Gha)?;
        let again = Expr::parse(&e.print(), Dialect::Gha)?;
        assert_eq!(e.ast(), again.ast(), "{src} → {}", e.print());
    }
    Ok(())
}
