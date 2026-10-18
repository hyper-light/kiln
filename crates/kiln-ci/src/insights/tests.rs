use super::*;

fn node(id: &str, needs: &[&str], duration: f64) -> Node {
    Node { id: id.into(), needs: needs.iter().map(|n| (*n).to_owned()).collect(), duration }
}

/// W19-AC01: for runs with known durations, the critical path is the hand-computed one.
#[test]
fn w19_ac01_critical_path() {
    // A diamond: a → b (5) and c (20) → d. By hand: a + c + d = 31, against a + b + d = 16.
    let diamond = [node("a", &[], 10.0), node("b", &["a"], 5.0), node("c", &["a"], 20.0), node("d", &["b", "c"], 1.0)];
    assert_eq!(critical_path(&diamond), ["a", "c", "d"]);
    // A long job on its own outlasts a chain: lint (100) against build → test → deploy (95).
    let apart = [node("build", &[], 30.0), node("test", &["build"], 60.0), node("deploy", &["test"], 5.0), node("lint", &[], 100.0)];
    assert_eq!(critical_path(&apart), ["lint"]);
    // …and loses to it once the chain is longer (build 40: 105).
    let longer = [node("build", &[], 40.0), node("test", &["build"], 60.0), node("deploy", &["test"], 5.0), node("lint", &[], 100.0)];
    assert_eq!(critical_path(&longer), ["build", "test", "deploy"]);
    // Ties go to the id that sorts first, the same every time.
    let tie = [node("a", &[], 10.0), node("c", &["a"], 5.0), node("b", &["a"], 5.0), node("d", &["b", "c"], 1.0)];
    assert_eq!(critical_path(&tie), ["a", "b", "d"]);
    // Fan-in from three, the middle one longest; a job that never ran counts nothing.
    let fan = [node("x", &[], 3.0), node("y", &[], 9.0), node("z", &[], 0.0), node("join", &["x", "y", "z"], 2.0), node("after", &["join"], 4.0)];
    assert_eq!(critical_path(&fan), ["y", "join", "after"]);
    // Needs that name no job are ignored; a cycle doesn't hang, and its jobs stand alone.
    let odd = [node("a", &["ghost"], 4.0), node("b", &["c"], 1.0), node("c", &["b"], 2.0)];
    assert_eq!(critical_path(&odd), ["a"]);
    assert!(critical_path(&[]).is_empty());
    // A long chain, hand-computed: 1 + 2 + … + 200.
    let chain: Vec<Node> = (0..200).map(|i| Node { id: format!("j{i:03}"), needs: if i == 0 { vec![] } else { vec![format!("j{:03}", i - 1)] }, duration: f64::from(i + 1) }).collect();
    let path = critical_path(&chain);
    assert_eq!(path.len(), 200);
    assert_eq!(path.first().map(String::as_str), Some("j000"));
    assert_eq!(path.last().map(String::as_str), Some("j199"));
}

/// splitmix64, for synthetic legs.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in (0, 1].
    fn unit(&mut self) -> f64 {
        ((self.next() >> 11) as f64 + 1.0) / (1u64 << 53) as f64
    }
}

/// The exact `q` quantile, as [`Hist::quantile`] ranks it.
fn exact(sorted: &[f64], q: f64) -> f64 {
    sorted.get((q * (sorted.len() - 1) as f64).round() as usize).copied().unwrap_or(f64::NAN)
}

/// W19-AC02: rollups over a million synthetic legs (hourly, merged into days, merged into the
/// month) equal direct computation: counts exactly, sums to rounding, percentiles within 1 %.
#[test]
fn w19_ac02_rollups() {
    const LEGS: usize = 1_000_000;
    const HOURS: usize = 30 * 24;
    let mut rng = Rng(42);
    let mut hours: Vec<Rollup> = vec![Rollup::default(); HOURS];
    let (mut durations, mut queues) = (Vec::with_capacity(LEGS), Vec::with_capacity(LEGS));
    let (mut success, mut failure, mut cancelled, mut flaky, mut retried) = (0u64, 0u64, 0u64, 0u64, 0u64);
    let (mut cpu, mut runner, mut capacity, mut rss) = (0.0f64, 0.0f64, 0.0f64, 0u64);
    let mut classes: BTreeMap<String, u64> = BTreeMap::new();
    for _ in 0..LEGS {
        let hour = (rng.next() % HOURS as u64) as usize;
        // Log-normal-ish durations from a second to hours; queues mostly short, some long.
        let duration = ((rng.unit().ln() * -1.2).exp() * 40.0).min(6.0 * 3600.0);
        let queue = if rng.next().is_multiple_of(10) { rng.unit() * 900.0 } else { rng.unit() * 5.0 };
        let roll = rng.next() % 100;
        let conclusion = match roll {
            0..=84 => "success",
            85..=94 => "failure",
            95..=97 => "cancelled",
            _ => "skipped",
        };
        let attempts = if rng.next().is_multiple_of(20) { 2 } else { 1 };
        let is_flaky = conclusion == "success" && attempts > 1;
        let class = (conclusion == "failure").then(|| if rng.next().is_multiple_of(3) { "infrastructure" } else { "failure" }.to_owned());
        let waits: BTreeMap<String, f64> = [("capacity".to_owned(), queue * 0.75), ("concurrency".to_owned(), queue * 0.25)].into();
        let fact = Fact { conclusion: conclusion.into(), duration: Some(duration), queue: Some(queue), cpu_seconds: duration * 0.6, peak_rss: rng.next() % (8 << 30), runner_seconds: duration * f64::from(attempts), attempts, flaky: is_flaky, waits, failure_class: class.clone() };
        if let Some(h) = hours.get_mut(hour) {
            h.add(&fact);
        }
        durations.push(duration);
        queues.push(queue);
        match conclusion {
            "success" => success += 1,
            "failure" => failure += 1,
            "cancelled" => cancelled += 1,
            _ => {}
        }
        flaky += u64::from(is_flaky);
        retried += u64::from(attempts > 1);
        cpu += fact.cpu_seconds;
        runner += fact.runner_seconds;
        capacity += queue * 0.75;
        rss = rss.max(fact.peak_rss);
        if let Some(c) = class {
            *classes.entry(c).or_default() += 1;
        }
    }
    // Hours into days through their stored form, as the server keeps them; days into the month.
    let stored: Vec<String> = hours.iter().map(|h| serde_json::to_string(h).unwrap_or_default()).collect();
    let mut days = vec![Rollup::default(); 30];
    for (i, s) in stored.iter().enumerate() {
        let h: Rollup = serde_json::from_str(s).unwrap_or_default();
        if let Some(d) = days.get_mut(i / 24) {
            d.merge(&h);
        }
    }
    let mut month = Rollup::default();
    for d in &days {
        month.merge(d);
    }
    assert_eq!(month.count, LEGS as u64);
    assert_eq!((month.success, month.failure, month.cancelled, month.flaky, month.retried), (success, failure, cancelled, flaky, retried));
    assert_eq!(month.failures, classes);
    assert_eq!(month.peak_rss, rss);
    let close = |a: f64, b: f64| ((a - b) / b).abs() < 1e-9;
    assert!(close(month.cpu_seconds, cpu) && close(month.runner_seconds, runner), "{} {cpu} · {} {runner}", month.cpu_seconds, month.runner_seconds);
    assert!(close(month.duration.sum, durations.iter().sum()) && close(month.queue.sum, queues.iter().sum()));
    assert!(month.waits.get("capacity").is_some_and(|w| close(*w, capacity)));
    durations.sort_by(f64::total_cmp);
    queues.sort_by(f64::total_cmp);
    for q in [0.01, 0.25, 0.5, 0.9, 0.99, 0.999] {
        for (h, all) in [(&month.duration, &durations), (&month.queue, &queues)] {
            let (got, want) = (h.quantile(q).unwrap_or(f64::NAN), exact(all, q));
            assert!(((got - want) / want).abs() <= ACCURACY * 1.01, "p{q}: {got} against {want}");
        }
    }
    // The ends are exact, but for the last digit JSON may round.
    assert!(close(month.duration.quantile(1.0).unwrap_or(0.0), durations.last().copied().unwrap_or(1.0)));
    assert!(close(month.duration.quantile(0.0).unwrap_or(0.0), durations.first().copied().unwrap_or(1.0)));
    let s = month.summary();
    assert!(s.success_rate.is_some_and(|r| close(r, success as f64 / (success + failure) as f64)));
    assert!(s.flaky_rate.is_some_and(|r| close(r, flaky as f64 / (success + failure) as f64)));
    // A day is its hours: merging in any order gives the same rollup.
    let mut backwards = Rollup::default();
    for s in stored.iter().take(24).rev() {
        backwards.merge(&serde_json::from_str(s).unwrap_or_default());
    }
    let first = days.first().cloned().unwrap_or_default();
    assert_eq!((backwards.count, backwards.duration.quantile(0.9)), (first.count, first.duration.quantile(0.9)));
}

#[test]
fn w19_r02_hist_edges() {
    let mut h = Hist::default();
    assert_eq!(h.quantile(0.5), None);
    for x in [0.0, 0.0, 5.0, f64::NAN, -1.0, f64::INFINITY] {
        h.add(x);
    }
    assert_eq!(h.count, 3, "NaN, negatives and infinities are left out");
    assert_eq!(h.quantile(0.0), Some(0.0));
    assert_eq!(h.quantile(1.0), Some(5.0));
    let back: Hist = serde_json::from_str(&serde_json::to_string(&h).unwrap_or_default()).unwrap_or_default();
    assert_eq!(back, h);
}
