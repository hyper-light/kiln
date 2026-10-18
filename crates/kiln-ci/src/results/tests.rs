//! W16-AC01 (every format's fixture gives its cases and outcomes; malformed ones are errors) and
//! W16-AC02 (flaky classification).

use super::*;

macro_rules! fixture {
    ($name:literal) => {
        include_str!(concat!("../../tests/fixtures/results/", $name))
    };
}

/// `(suite, name, outcome)` of each case.
fn cases(r: &Report) -> Vec<(String, String, Outcome)> {
    r.cases.iter().map(|c| (c.suite.clone(), c.name.clone(), c.outcome)).collect()
}

fn one(r: &Report, name: &str) -> Case {
    r.cases.iter().find(|c| c.name == name).cloned().unwrap_or_default()
}

fn read(text: &str, want: Format) -> Report {
    assert_eq!(detect(text), Some(want), "detected");
    let r = parse(text, None);
    assert!(r.is_ok(), "{r:?}");
    r.unwrap_or(Report { format: want, cases: Vec::new() })
}

#[test]
fn w16_ac01_junit_dialects() {
    let r = read(fixture!("surefire.xml"), Format::Junit);
    let s = |n: &str, o| ("com.acme.AppTest".to_owned(), n.to_owned(), o);
    assert_eq!(cases(&r), vec![s("adds", Outcome::Passed), s("divides", Outcome::Failed), s("connects", Outcome::Error), s("later", Outcome::Skipped), s("wobbles", Outcome::Passed), s("breaks", Outcome::Failed)]);
    let d = one(&r, "divides");
    assert_eq!((d.message.as_deref(), d.duration_ms), (Some("expected: <2> but was: <3>"), Some(10)));
    assert!(d.trace.as_deref().is_some_and(|t| t.contains("AppTest.java:21")));
    assert_eq!(one(&r, "wobbles").retried, 1, "a flakyFailure is a retry that passed");
    assert_eq!(one(&r, "later").message.as_deref(), Some("not yet"));

    let r = read(fixture!("pytest.xml"), Format::Junit);
    let d = one(&r, "test_div[2-0]");
    assert_eq!((d.suite.as_str(), d.file.as_deref(), d.line, d.outcome), ("tests.test_app", Some("tests/test_app.py"), Some(7), Outcome::Failed));
    assert_eq!(d.message.as_deref(), Some("ZeroDivisionError: division by zero"));
    assert!(d.trace.as_deref().is_some_and(|t| t.contains("tests/test_app.py:10")));
    assert_eq!(r.count(Outcome::Skipped), 1);

    let r = read(fixture!("jest-junit.xml"), Format::Junit);
    let d = one(&r, "math divides by zero");
    assert_eq!((d.suite.as_str(), d.file.as_deref(), d.outcome), ("math divides", Some("src/math.test.js"), Outcome::Failed));
    assert_eq!(d.message.as_deref(), Some("Error: expect(received).toThrow()"), "a failure without a message: its first line");

    let r = read(fixture!("go-junit-report.xml"), Format::Junit);
    assert_eq!(cases(&r), vec![("example.com/acme/calc".into(), "TestAdd".into(), Outcome::Passed), ("example.com/acme/calc".into(), "TestDiv".into(), Outcome::Failed)]);

    let r = read(fixture!("nextest.xml"), Format::Junit);
    assert_eq!(cases(&r).iter().map(|c| c.2).collect::<Vec<_>>(), vec![Outcome::Passed, Outcome::Failed, Outcome::Passed]);
    assert_eq!(one(&r, "tests::sometimes").retried, 1);
    assert_eq!(one(&r, "tests::divides").duration_ms, Some(3));
}

#[test]
fn w16_ac01_tap() {
    let r = read(fixture!("tap13.tap"), Format::Tap);
    assert_eq!(cases(&r), vec![("math".into(), "adds".into(), Outcome::Passed), ("math".into(), "divides".into(), Outcome::Failed), (String::new(), "standalone".into(), Outcome::Skipped), (String::new(), "someday".into(), Outcome::Skipped)]);
    let d = one(&r, "divides");
    assert_eq!(d.message.as_deref(), Some("Expected values to be strictly equal: 3 !== 2"));
    assert_eq!((d.file.as_deref(), d.line, d.duration_ms), (Some("/repo/test/math.test.js"), Some(9), Some(1)));
    assert!(d.trace.as_deref().is_some_and(|t| t.contains("math.test.js:10:12")));
    assert_eq!(one(&r, "standalone").message.as_deref(), Some("SKIP needs a database"));
    // Bail out! ends the run as an error.
    let r = parse("TAP version 13\nok 1 - a\nBail out! the database is gone\n", Some(Format::Tap)).unwrap_or(Report { format: Format::Tap, cases: Vec::new() });
    assert_eq!(cases(&r).last().map(|c| c.2), Some(Outcome::Error));
}

#[test]
fn w16_ac01_go_test_json() {
    let r = read(fixture!("go-test.json"), Format::GoTestJson);
    let p = |n: &str, o| ("example.com/acme/calc".to_owned(), n.to_owned(), o);
    assert_eq!(cases(&r), vec![p("TestAdd", Outcome::Passed), p("TestDiv", Outcome::Failed), p("TestDiv/zero", Outcome::Skipped), ("example.com/acme/broken".into(), "(package)".into(), Outcome::Error)]);
    let d = one(&r, "TestDiv");
    assert_eq!((d.file.as_deref(), d.line, d.message.as_deref(), d.duration_ms), (Some("calc_test.go"), Some(14), Some("got 3, want 2"), Some(2)));
    let b = one(&r, "(package)");
    assert!(b.trace.as_deref().is_some_and(|t| t.contains("syntax error")), "a package that didn't build: {b:?}");
}

#[test]
fn w16_ac01_libtest_json() {
    let r = read(fixture!("libtest.json"), Format::LibtestJson);
    assert_eq!(cases(&r).iter().map(|c| (c.1.as_str(), c.2)).collect::<Vec<_>>(), vec![("tests::adds", Outcome::Passed), ("tests::divides", Outcome::Failed), ("tests::later", Outcome::Skipped)]);
    let d = one(&r, "tests::divides");
    assert_eq!((d.file.as_deref(), d.line, d.duration_ms), (Some("src/lib.rs"), Some(20), Some(2)));
    assert!(d.message.as_deref().is_some_and(|m| m.contains("panicked at")));
    let r = read(fixture!("nextest-libtest.json"), Format::LibtestJson);
    assert_eq!(cases(&r), vec![("calc".into(), "tests::adds".into(), Outcome::Passed), ("calc".into(), "tests::divides".into(), Outcome::Failed)], "nextest's binary$test");
}

#[test]
fn w16_ac01_trx_and_xunit() {
    let r = read(fixture!("results.trx"), Format::Trx);
    let s = |n: &str, o| ("Acme.Tests.MathTests".to_owned(), n.to_owned(), o);
    assert_eq!(cases(&r), vec![s("Adds", Outcome::Passed), s("Divides", Outcome::Failed), s("Later", Outcome::Skipped)]);
    let d = one(&r, "Divides");
    assert_eq!(d.duration_ms, Some(23));
    assert!(d.message.as_deref().is_some_and(|m| m.starts_with("Assert.Equal() Failure")) && d.trace.as_deref().is_some_and(|t| t.contains("MathTests.cs:line 14")));

    let r = read(fixture!("xunit.xml"), Format::Xunit);
    assert_eq!(cases(&r), vec![s("Adds", Outcome::Passed), s("Divides", Outcome::Failed), s("Later", Outcome::Skipped)]);
    let d = one(&r, "Divides");
    assert_eq!((d.file.as_deref(), d.line, d.message.as_deref()), (Some("/repo/tests/MathTests.cs"), Some(14), Some("Assert.Equal() Failure: Values differ")));
    assert_eq!(one(&r, "Later").message.as_deref(), Some("not yet"));
}

#[test]
fn w16_ac01_mocha_and_ctrf() {
    let r = read(fixture!("mocha.json"), Format::MochaJson);
    assert_eq!(cases(&r), vec![("math".into(), "adds".into(), Outcome::Passed), ("math".into(), "divides".into(), Outcome::Failed), ("math".into(), "later".into(), Outcome::Skipped)]);
    assert_eq!(one(&r, "divides").message.as_deref(), Some("3 == 2"));

    let r = read(fixture!("ctrf.json"), Format::Ctrf);
    assert_eq!(cases(&r).iter().map(|c| c.2).collect::<Vec<_>>(), vec![Outcome::Passed, Outcome::Failed, Outcome::Skipped, Outcome::Passed]);
    assert_eq!(one(&r, "logs in").suite, "login > happy path");
    let f = one(&r, "rejects a bad password");
    assert_eq!((f.file.as_deref(), f.line, f.duration_ms, f.message.as_deref()), (Some("e2e/login.spec.ts"), Some(18), Some(340), Some("expected 401, got 500")));
    assert_eq!(one(&r, "loads the dashboard").retried, 2, "a flaky test's retries");
}

#[test]
fn w16_ac01_malformed_reports_are_errors() {
    assert!(parse(fixture!("bad-junit.xml"), None).is_err(), "an unclosed testcase");
    assert!(parse(fixture!("bad-ctrf.json"), Some(Format::Ctrf)).is_err(), "no tests");
    assert!(parse(fixture!("bad-trx.trx"), Some(Format::Trx)).is_err(), "cut short");
    assert!(parse(fixture!("empty.tap"), None).is_err(), "no results at all");
    assert!(parse("just some text", None).is_err());
    assert!(parse("{\"Action\": 1", Some(Format::GoTestJson)).is_err());
    assert!(parse("<html><body>no</body></html>", Some(Format::Junit)).is_err());
    // Every format refuses another format's report or garbage without panicking.
    for f in Format::ALL {
        for text in [fixture!("surefire.xml"), fixture!("go-test.json"), fixture!("ctrf.json"), "\u{0}\u{1}<<<{{{", ""] {
            let _ = parse(text, Some(f));
        }
    }
}

#[test]
fn w16_r02_ids_and_traces() {
    let a = Case { suite: "s".into(), name: "n".into(), file: Some("f".into()), ..Case::default() };
    let b = Case { outcome: Outcome::Failed, duration_ms: Some(5), ..a.clone() };
    assert_eq!(a.id(), b.id(), "the identity is suite, name and file only");
    assert_ne!(a.id(), Case { file: None, ..a.clone() }.id());
    let long = "é".repeat(MAX_TRACE);
    assert!(trace(&long).is_some_and(|t| t.len() <= MAX_TRACE && t.chars().all(|c| c == 'é')), "cut at a character boundary");
    assert_eq!(seconds_ms("1,234.5"), Some(1_234_500));
    assert_eq!(Format::parse("Surefire"), Some(Format::Junit));
    assert_eq!(Format::parse("nextest-json"), Some(Format::LibtestJson));
}

/// W16-AC02: a failure on attempt 1 that passes on attempt 2 (same input) is flaky; a tool's
/// retry that passed is flaky; flipping on trunk is probably flaky; failing for good after a
/// change isn't.
#[test]
fn w16_ac02_flaky() {
    let seen = |input: &str, o| Seen { input: input.into(), outcome: o, retried: 0 };
    let (p, f) = (Outcome::Passed, Outcome::Failed);
    assert!(matches!(flaky(&[seen("in1", f), seen("in1", p)], 30, 0.2), Flakiness::Flaky { .. }), "attempt 1 failed, attempt 2 passed");
    assert!(matches!(flaky(&[Seen { input: "in1".into(), outcome: p, retried: 1 }], 30, 0.2), Flakiness::Flaky { .. }), "the tool's retry passed it");
    let flips: Vec<Seen> = (0..20).map(|i| seen(&format!("c{i}"), if i % 3 == 0 { f } else { p })).collect();
    assert!(matches!(flaky(&flips, 20, 0.2), Flakiness::ProbablyFlaky { runs: 20, .. }), "{:?}", flaky(&flips, 20, 0.2));
    let broke: Vec<Seen> = (0..20).map(|i| seen(&format!("c{i}"), if i >= 10 { f } else { p })).collect();
    assert_eq!(flaky(&broke, 20, 0.04), Flakiness::Stable, "it broke at one change and stayed broken");
    assert_eq!(flaky(&[seen("a", p), seen("b", p), seen("c", Outcome::Skipped)], 20, 0.2), Flakiness::Stable);
}
