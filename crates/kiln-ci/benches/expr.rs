//! W01-AC07: evaluations per second of a typical condition (`cargo bench -p kiln-ci --bench expr`).

use std::time::Instant;

use kiln_ci::expr::{Budget, Dialect, Expr, MapContexts, Value, eval};

fn main() {
    let rate = kiln_ci_bench_expr();
    println!("expr: {rate:.0} evaluations/s of `github.event_name == 'push' && contains(github.ref, 'refs/heads/')`");
}

fn kiln_ci_bench_expr() -> f64 {
    let github = Value::object(vec![("event_name".to_owned(), Value::str("push")), ("ref".to_owned(), Value::str("refs/heads/main"))]);
    let cx = MapContexts::default().with("github", github);
    let Ok(e) = Expr::parse("github.event_name == 'push' && contains(github.ref, 'refs/heads/')", Dialect::Gha) else { return 0.0 };
    let n = 2_000_000u32;
    let start = Instant::now();
    let mut truthy = 0u32;
    for _ in 0..n {
        let mut b = Budget::default();
        if eval(&e, &cx, &mut b).is_ok_and(|v| v.truthy()) {
            truthy += 1;
        }
    }
    let secs = start.elapsed().as_secs_f64();
    assert_eq!(truthy, n);
    f64::from(n) / secs
}
