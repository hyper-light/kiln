//! W01-AC07, asserted: at least 1,000,000 evaluations per second per core (release builds only;
//! debug builds are not representative).

#[cfg(not(debug_assertions))]
#[test]
fn w01_ac07_a_million_evaluations_per_second() {
    use std::time::Instant;

    use kiln_ci::expr::{Budget, Dialect, Expr, MapContexts, Value, eval};

    let github = Value::object(vec![("event_name".to_owned(), Value::str("push")), ("ref".to_owned(), Value::str("refs/heads/main"))]);
    let cx = MapContexts::default().with("github", github);
    let e = Expr::parse("github.event_name == 'push' && contains(github.ref, 'refs/heads/')", Dialect::Gha);
    assert!(e.is_ok());
    let Ok(e) = e else { return };
    let n = 1_000_000u32;
    let start = Instant::now();
    for _ in 0..n {
        let mut b = Budget::default();
        let _ = eval(&e, &cx, &mut b);
    }
    let rate = f64::from(n) / start.elapsed().as_secs_f64();
    assert!(rate >= 1_000_000.0, "{rate:.0} evaluations/s");
}
