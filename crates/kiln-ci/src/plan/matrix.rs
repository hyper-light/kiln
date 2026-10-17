//! Matrix expansion (docs/ci.md W04-R07, W04-R08, W04-R13 to W04-R15). The values come in
//! evaluated; [`super`] evaluates them and applies `where:`.

use crate::expr::{Value, js_number_to_string, json_stringify};

/// One combination: the leg's `matrix` context, keys in order (original axes first, then what
/// `include` added).
pub type Combo = Vec<(String, Value)>;

/// The value of `key` in `c` (matrix variable names are case-insensitive, as on GitHub).
pub fn get<'a>(c: &'a Combo, key: &str) -> Option<&'a Value> {
    c.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v)
}

fn set(c: &mut Combo, key: &str, v: &Value) {
    match c.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case(key)) {
        Some(slot) => slot.1 = v.clone(),
        None => c.push((key.to_owned(), v.clone())),
    }
}

/// Whether two values are the same value: strings exactly, numbers by value, arrays and
/// objects member by member.
pub fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Number(x), Value::Number(y)) => x == y,
        (Value::String(x), Value::String(y)) => x == y,
        (Value::Array(_) | Value::Filtered(_), Value::Array(_) | Value::Filtered(_)) => {
            let (x, y) = (a.items().unwrap_or_default(), b.items().unwrap_or_default());
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| same(p, q))
        }
        (Value::Object(x), Value::Object(y)) => x.len() == y.len() && x.0.iter().all(|(k, v)| y.get(k).is_some_and(|w| same(v, w))),
        _ => false,
    }
}

fn members(v: &Value, what: &str) -> Result<Vec<(String, Value)>, String> {
    match v {
        Value::Object(m) => Ok(m.0.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()),
        _ => Err(format!("each `{what}` entry must be a mapping of matrix variables to values")),
    }
}

/// GitHub's expansion (W04-R07): the Cartesian product of `axes` (first axis varying slowest),
/// without the combinations an `exclude` entry matches (a partial match is enough), then each
/// `include` entry in order: added to every original combination whose original values it
/// doesn't contradict (overwriting what earlier entries added, never an original value), or
/// appended as a combination of its own when it fits none. With no axes, each `include` entry is
/// a leg. More than `limit` legs is an error.
pub fn expand(axes: &[(String, Vec<Value>)], include: &[Value], exclude: &[Value], limit: usize) -> Result<Vec<Combo>, String> {
    for (i, (k, vals)) in axes.iter().enumerate() {
        if vals.is_empty() {
            return Err(format!("matrix variable `{k}` has no values"));
        }
        if axes.iter().take(i).any(|(o, _)| o.eq_ignore_ascii_case(k)) {
            return Err(format!("matrix variable `{k}` is defined twice (names are case-insensitive)"));
        }
    }
    // Bound the work before doing it: excludes may bring the count back under the limit, but not
    // by more than this.
    let product = axes.iter().try_fold(1usize, |n, (_, v)| n.checked_mul(v.len())).unwrap_or(usize::MAX);
    if !axes.is_empty() && product > limit.saturating_mul(16).max(4096) {
        return Err(format!("the matrix has {product} combinations; at most {limit} legs are allowed"));
    }
    let mut combos: Vec<Combo> = if axes.is_empty() { Vec::new() } else { vec![Vec::new()] };
    for (k, vals) in axes {
        let mut next = Vec::with_capacity(combos.len().saturating_mul(vals.len()));
        for c in &combos {
            for v in vals {
                let mut c2 = c.clone();
                c2.push((k.clone(), v.clone()));
                next.push(c2);
            }
        }
        combos = next;
    }
    let excludes: Vec<Vec<(String, Value)>> = exclude.iter().map(|e| members(e, "exclude")).collect::<Result<_, _>>()?;
    combos.retain(|c| !excludes.iter().any(|e| e.iter().all(|(k, v)| get(c, k).is_some_and(|x| same(x, v)))));
    let is_axis = |k: &str| axes.iter().any(|(a, _)| a.eq_ignore_ascii_case(k));
    let originals = combos.len();
    for inc in include {
        let entry = members(inc, "include")?;
        let mut fitted = false;
        for c in combos.iter_mut().take(originals) {
            let fits = entry.iter().all(|(k, v)| if is_axis(k) { get(c, k).is_some_and(|x| same(x, v)) } else { true });
            if fits {
                for (k, v) in entry.iter().filter(|(k, _)| !is_axis(k)) {
                    set(c, k, v);
                }
                fitted = true;
            }
        }
        if !fitted {
            combos.push(entry);
        }
    }
    if combos.len() > limit {
        return Err(format!("the matrix gives {} legs; at most {limit} are allowed per job", combos.len()));
    }
    Ok(combos)
}

/// A value as a leg's name shows it.
pub fn display(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => js_number_to_string(*n),
        Value::String(s) => s.to_string(),
        Value::Array(_) | Value::Filtered(_) | Value::Object(_) => json_stringify(v, 0),
    }
}

/// A leg's name: the job's, then its values in parentheses (W04-R08).
pub fn leg_name(job: &str, c: &Combo) -> String {
    if c.is_empty() {
        return job.to_owned();
    }
    let vals: Vec<String> = c.iter().map(|(_, v)| display(v)).collect();
    format!("{job} ({})", vals.join(", "))
}

/// An OCI platform (`linux/arm64`, `linux/arm/v7`) as a leg sees it (W04-R13): `matrix.os` and
/// `matrix.arch` in OCI's spelling, and the requirement over runner facts (which use Rust's
/// spelling: `macos`, `x86_64`, `aarch64`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Platform {
    pub os: String,
    pub arch: String,
    pub variant: Option<String>,
    /// Facts' `os` and `arch`.
    pub fact_os: &'static str,
    pub fact_arch: &'static str,
}

impl Platform {
    pub fn parse(p: &str) -> Result<Platform, String> {
        let mut parts = p.split('/');
        let (os, arch, variant) = (parts.next().unwrap_or_default(), parts.next().unwrap_or_default(), parts.next());
        if parts.next().is_some() {
            return Err(format!("`{p}` is not a platform: os/arch or os/arch/variant"));
        }
        let fact_os = match os {
            "linux" => "linux",
            "darwin" => "macos",
            "windows" => "windows",
            "freebsd" => "freebsd",
            _ => return Err(format!("`{p}`: unknown OS `{os}` (linux, darwin, windows, freebsd)")),
        };
        let fact_arch = match arch {
            "amd64" => "x86_64",
            "arm64" => "aarch64",
            "arm" => "arm",
            "386" => "x86",
            "riscv64" => "riscv64",
            "ppc64le" => "powerpc64",
            "s390x" => "s390x",
            _ => return Err(format!("`{p}`: unknown architecture `{arch}` (amd64, arm64, arm, 386, riscv64, ppc64le, s390x)")),
        };
        Ok(Platform { os: os.to_owned(), arch: arch.to_owned(), variant: variant.map(str::to_owned), fact_os, fact_arch })
    }

    /// The routing requirement (W05-R22) for running natively.
    pub fn requirement(&self) -> String {
        match &self.variant {
            Some(v) => format!("os == '{}' && arch == '{}' && arch_variant == '{v}'", self.fact_os, self.fact_arch),
            None => format!("os == '{}' && arch == '{}'", self.fact_os, self.fact_arch),
        }
    }

    /// The requirement when QEMU user emulation may stand in (`emulate:`): native, or a Linux
    /// runner with the platform registered in binfmt_misc.
    pub fn emulated_requirement(&self, platform: &str) -> String {
        format!("({}) || (os == 'linux' && binfmt contains '{platform}')", self.requirement())
    }
}

/// How many legs keep each under `target` seconds for work of `total` seconds (`count: auto`).
pub fn shard_count(total: f64, target: f64) -> usize {
    if target.is_nan() || total.is_nan() || target <= 0.0 || total <= 0.0 {
        return 1;
    }
    let n = (total / target).ceil();
    if n >= 1_024.0 { 1_024 } else { (n as usize).max(1) }
}

/// Items split into `count` legs (W04-R14). By timing: longest first, each to the leg with the
/// least work so far (unknown durations count as the median of the known ones, or 1 s), ties to
/// the lower leg. By count: dealt in turn. Deterministic for the same items and history; legs
/// keep their items in the original order.
pub fn balance(items: &[String], durations: &[Option<f64>], count: usize, by_timing: bool) -> Vec<Vec<String>> {
    let count = count.max(1);
    let mut legs: Vec<Vec<usize>> = vec![Vec::new(); count];
    if by_timing {
        let mut known: Vec<f64> = durations.iter().flatten().copied().filter(|d| d.is_finite() && *d >= 0.0).collect();
        known.sort_by(f64::total_cmp);
        let median = known.get(known.len() / 2).copied().unwrap_or(1.0);
        let cost = |i: usize| durations.get(i).copied().flatten().filter(|d| d.is_finite() && *d >= 0.0).unwrap_or(median);
        let mut order: Vec<usize> = (0..items.len()).collect();
        order.sort_by(|a, b| cost(*b).total_cmp(&cost(*a)).then_with(|| items.get(*a).cmp(&items.get(*b))));
        let mut load = vec![0.0f64; count];
        for i in order {
            let mut best = 0usize;
            for (l, w) in load.iter().enumerate() {
                if *w < load.get(best).copied().unwrap_or(f64::MAX) {
                    best = l;
                }
            }
            if let (Some(leg), Some(w)) = (legs.get_mut(best), load.get_mut(best)) {
                leg.push(i);
                *w += cost(i);
            }
        }
    } else {
        for i in 0..items.len() {
            if let Some(leg) = legs.get_mut(i % count) {
                leg.push(i);
            }
        }
    }
    legs.into_iter()
        .map(|mut l| {
            l.sort_unstable();
            l.into_iter().filter_map(|i| items.get(i).cloned()).collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(x: &str) -> Value {
        Value::str(x)
    }
    fn n(x: f64) -> Value {
        Value::Number(x)
    }
    fn obj(pairs: &[(&str, Value)]) -> Value {
        Value::object(pairs.iter().map(|(k, v)| ((*k).to_owned(), v.clone())))
    }
    fn combo(pairs: &[(&str, Value)]) -> Combo {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect()
    }
    fn axes(a: &[(&str, Vec<Value>)]) -> Vec<(String, Vec<Value>)> {
        a.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect()
    }

    // W04-AC01: GitHub's documented examples ("Running variations of jobs in a workflow" and the
    // workflow syntax reference, checked 2026-10-17), with the legs in the documented order.

    #[test]
    fn w04_ac01_two_axes_in_order() -> Result<(), String> {
        let legs = expand(&axes(&[("version", vec![n(10.0), n(12.0), n(14.0)]), ("os", vec![s("ubuntu-latest"), s("windows-latest")])]), &[], &[], 256)?;
        let want: Vec<Combo> = [10.0, 12.0, 14.0].iter().flat_map(|v| ["ubuntu-latest", "windows-latest"].map(|o| combo(&[("version", n(*v)), ("os", s(o))]))).collect();
        assert_eq!(legs, want);
        Ok(())
    }

    #[test]
    fn w04_ac01_include_fruit_and_animals() -> Result<(), String> {
        let include = [
            obj(&[("color", s("green"))]),
            obj(&[("color", s("pink")), ("animal", s("cat"))]),
            obj(&[("fruit", s("apple")), ("shape", s("circle"))]),
            obj(&[("fruit", s("banana"))]),
            obj(&[("fruit", s("banana")), ("animal", s("cat"))]),
        ];
        let legs = expand(&axes(&[("fruit", vec![s("apple"), s("pear")]), ("animal", vec![s("cat"), s("dog")])]), &include, &[], 256)?;
        assert_eq!(
            legs,
            vec![
                combo(&[("fruit", s("apple")), ("animal", s("cat")), ("color", s("pink")), ("shape", s("circle"))]),
                combo(&[("fruit", s("apple")), ("animal", s("dog")), ("color", s("green")), ("shape", s("circle"))]),
                combo(&[("fruit", s("pear")), ("animal", s("cat")), ("color", s("pink"))]),
                combo(&[("fruit", s("pear")), ("animal", s("dog")), ("color", s("green"))]),
                combo(&[("fruit", s("banana"))]),
                combo(&[("fruit", s("banana")), ("animal", s("cat"))]),
            ]
        );
        Ok(())
    }

    #[test]
    fn w04_ac01_exclude_nine_of_twelve() -> Result<(), String> {
        let exclude = [
            obj(&[("os", s("macos-latest")), ("version", n(12.0)), ("environment", s("production"))]),
            obj(&[("os", s("windows-latest")), ("version", n(16.0))]),
        ];
        let legs = expand(
            &axes(&[("os", vec![s("macos-latest"), s("windows-latest")]), ("version", vec![n(12.0), n(14.0), n(16.0)]), ("environment", vec![s("staging"), s("production")])]),
            &[],
            &exclude,
            256,
        )?;
        assert_eq!(legs.len(), 9);
        assert!(!legs.iter().any(|c| get(c, "os") == Some(&s("windows-latest")) && get(c, "version") == Some(&n(16.0))));
        assert!(!legs.contains(&combo(&[("os", s("macos-latest")), ("version", n(12.0)), ("environment", s("production"))])));
        Ok(())
    }

    #[test]
    fn w04_ac01_objects_in_an_axis() -> Result<(), String> {
        let v14 = obj(&[("version", n(14.0))]);
        let v20 = obj(&[("version", n(20.0)), ("env", s("NODE_OPTIONS=--openssl-legacy-provider"))]);
        let legs = expand(&axes(&[("os", vec![s("ubuntu-latest"), s("macos-latest")]), ("node", vec![v14.clone(), v20.clone()])]), &[], &[], 256)?;
        assert_eq!(
            legs,
            vec![
                combo(&[("os", s("ubuntu-latest")), ("node", v14.clone())]),
                combo(&[("os", s("ubuntu-latest")), ("node", v20.clone())]),
                combo(&[("os", s("macos-latest")), ("node", v14)]),
                combo(&[("os", s("macos-latest")), ("node", v20)]),
            ]
        );
        Ok(())
    }

    #[test]
    fn w04_ac01_expanding_and_adding() -> Result<(), String> {
        // Expanding: four legs; windows/16 gains npm: 6.
        let legs = expand(
            &axes(&[("os", vec![s("windows-latest"), s("ubuntu-latest")]), ("node", vec![n(14.0), n(16.0)])]),
            &[obj(&[("os", s("windows-latest")), ("node", n(16.0)), ("npm", n(6.0))])],
            &[],
            256,
        )?;
        assert_eq!(legs.len(), 4);
        assert_eq!(legs.get(1), Some(&combo(&[("os", s("windows-latest")), ("node", n(16.0)), ("npm", n(6.0))])));
        assert!(legs.iter().filter(|c| get(c, "npm").is_some()).count() == 1);
        // Adding: ten legs; the tenth is the new combination.
        let legs = expand(
            &axes(&[("os", vec![s("macos-latest"), s("windows-latest"), s("ubuntu-latest")]), ("version", vec![n(12.0), n(14.0), n(16.0)])]),
            &[obj(&[("os", s("windows-latest")), ("version", n(17.0))])],
            &[],
            256,
        )?;
        assert_eq!(legs.len(), 10);
        assert_eq!(legs.last(), Some(&combo(&[("os", s("windows-latest")), ("version", n(17.0))])));
        Ok(())
    }

    #[test]
    fn w04_ac01_include_only_and_experimental() -> Result<(), String> {
        let legs = expand(&[], &[obj(&[("site", s("production")), ("datacenter", s("site-a"))]), obj(&[("site", s("staging")), ("datacenter", s("site-b"))])], &[], 256)?;
        assert_eq!(legs, vec![combo(&[("site", s("production")), ("datacenter", s("site-a"))]), combo(&[("site", s("staging")), ("datacenter", s("site-b"))])]);
        // fail-fast's example: four legs, the fourth experimental.
        let legs = expand(
            &axes(&[("version", vec![n(6.0), n(7.0), n(8.0)]), ("experimental", vec![Value::Bool(false)])]),
            &[obj(&[("version", n(9.0)), ("experimental", Value::Bool(true))])],
            &[],
            256,
        )?;
        assert_eq!(legs.len(), 4);
        assert_eq!(legs.last(), Some(&combo(&[("version", n(9.0)), ("experimental", Value::Bool(true))])));
        // continue-on-error's example: 2 × 2 plus node 15 on ubuntu, experimental.
        let legs = expand(
            &axes(&[("node", vec![n(13.0), n(14.0)]), ("os", vec![s("macos-latest"), s("ubuntu-latest")]), ("experimental", vec![Value::Bool(false)])]),
            &[obj(&[("node", n(15.0)), ("os", s("ubuntu-latest")), ("experimental", Value::Bool(true))])],
            &[],
            256,
        )?;
        assert_eq!(legs.len(), 5);
        Ok(())
    }

    #[test]
    fn rules_and_limits() {
        assert!(expand(&axes(&[("os", vec![])]), &[], &[], 256).is_err_and(|e| e.contains("no values")));
        assert!(expand(&axes(&[("os", vec![s("a")]), ("OS", vec![s("b")])]), &[], &[], 256).is_err_and(|e| e.contains("twice")));
        let big: Vec<Value> = (0..17).map(|i| n(f64::from(i))).collect();
        assert!(expand(&axes(&[("a", big.clone()), ("b", big.clone())]), &[], &[], 256).is_err_and(|e| e.contains("at most 256")));
        assert!(expand(&axes(&[("a", big.clone()), ("b", big)]), &[], &[], 1024).is_ok());
        // include matches original values case-insensitively by name.
        let legs = expand(&axes(&[("os", vec![s("linux")])]), &[obj(&[("OS", s("linux")), ("extra", n(1.0))])], &[], 256).unwrap_or_default();
        assert_eq!(legs, vec![combo(&[("os", s("linux")), ("extra", n(1.0))])]);
        assert!(expand(&axes(&[("os", vec![s("linux")])]), &[s("x")], &[], 256).is_err());
    }

    #[test]
    fn names() {
        assert_eq!(leg_name("build", &combo(&[("os", s("ubuntu-latest")), ("node", n(14.0))])), "build (ubuntu-latest, 14)");
        assert_eq!(leg_name("build", &Vec::new()), "build");
        assert_eq!(leg_name("t", &combo(&[("node", obj(&[("version", n(14.0))]))])), "t ({\"version\":14})");
    }

    #[test]
    fn platforms() -> Result<(), String> {
        let p = Platform::parse("darwin/arm64")?;
        assert_eq!((p.fact_os, p.fact_arch), ("macos", "aarch64"));
        assert_eq!(p.requirement(), "os == 'macos' && arch == 'aarch64'");
        assert_eq!(Platform::parse("linux/arm/v7")?.requirement(), "os == 'linux' && arch == 'arm' && arch_variant == 'v7'");
        assert!(Platform::parse("plan9/amd64").is_err() && Platform::parse("linux").is_err());
        Ok(())
    }

    #[test]
    fn shards_balance() {
        let items: Vec<String> = (0..10).map(|i| format!("t{i}")).collect();
        let d: Vec<Option<f64>> = vec![Some(300.0), Some(240.0), Some(180.0), Some(120.0), Some(60.0), Some(60.0), None, Some(30.0), Some(30.0), Some(30.0)];
        let legs = balance(&items, &d, 3, true);
        let load = |l: &Vec<String>| l.iter().filter_map(|x| items.iter().position(|y| y == x)).map(|i| d.get(i).copied().flatten().unwrap_or(60.0)).sum::<f64>();
        let loads: Vec<f64> = legs.iter().map(load).collect();
        assert_eq!(legs.iter().map(Vec::len).sum::<usize>(), 10);
        let (max, min) = (loads.iter().copied().fold(0.0, f64::max), loads.iter().copied().fold(f64::MAX, f64::min));
        assert!(max - min <= 60.0, "{loads:?}");
        assert_eq!(legs, balance(&items, &d, 3, true), "deterministic");
        assert_eq!(balance(&items, &d, 3, false).iter().map(Vec::len).collect::<Vec<_>>(), vec![4, 3, 3]);
        assert_eq!(shard_count(2_400.0, 300.0), 8);
        assert_eq!(shard_count(10.0, 0.0), 1);
    }
}
