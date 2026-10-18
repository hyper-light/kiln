//! Job-result reuse (docs/ci.md W08-R10): what a job's `reuse:` says, and a leg's input hash.
//! Pure: the server gathers the inputs (the git trees of the listed paths, the images' digests,
//! the variables) and keeps the hashes of legs that succeeded.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;
use sha2::Digest as _;

use crate::wf::ir::YVal;

/// How long a result may be reused by default.
pub const MAX_AGE: u64 = 14 * 86_400;

/// A job's `reuse:` (kiln): `true`, or `{ paths: [globs], env: [names], max-age: 14d }`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Spec {
    /// The paths whose contents the leg's result depends on; the whole tree when empty.
    pub paths: Vec<String>,
    /// Variables (`env` and `vars`) whose values it depends on.
    pub env: Vec<String>,
    /// Seconds a result stays reusable.
    pub max_age: u64,
}

/// `14d`, `12h`, `30m`, `90s`, or seconds.
pub fn parse_age(s: &str) -> Option<u64> {
    let s = s.trim();
    let (n, unit) = s.find(|c: char| !c.is_ascii_digit()).map_or((s, ""), |i| s.split_at(i));
    let n: u64 = n.parse().ok()?;
    let mult = match unit.trim() {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        "w" => 7 * 86_400,
        _ => return None,
    };
    n.checked_mul(mult)
}

impl Spec {
    /// The job's `reuse:`; `None` when it doesn't opt in.
    pub fn from_yval(v: &YVal) -> Result<Option<Spec>, String> {
        let list = |k: &str| -> Result<Vec<String>, String> {
            match v.get(k) {
                None | Some(YVal::Null) => Ok(Vec::new()),
                Some(YVal::Seq(items)) => items.iter().map(|i| i.as_str().map(str::to_owned).ok_or_else(|| format!("`reuse.{k}` lists strings"))).collect(),
                Some(YVal::Str(t)) => Ok(t.src.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned).collect()),
                Some(_) => Err(format!("`reuse.{k}` is a list")),
            }
        };
        match v {
            YVal::Bool(false) | YVal::Null => Ok(None),
            YVal::Bool(true) => Ok(Some(Spec { max_age: MAX_AGE, ..Spec::default() })),
            YVal::Map(_) => {
                let max_age = match v.get("max-age") {
                    None => MAX_AGE,
                    Some(YVal::Num(n)) if *n >= 0.0 => *n as u64,
                    Some(a) => a.as_str().and_then(parse_age).ok_or("`reuse.max-age` is a duration (`14d`, `12h`)")?,
                };
                Ok(Some(Spec { paths: list("paths")?, env: list("env")?, max_age }))
            }
            _ => Err("`reuse` is `true` or `{ paths, env, max-age }`".into()),
        }
    }
}

/// Everything a leg's result depends on, as the server knows it before the leg runs.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Inputs {
    /// The leg as planned: its job's definition with everything known before it runs evaluated
    /// (matrix values, `needs` and `inputs` contexts, runs-on, environment, permissions, the
    /// workflow's `env` and defaults).
    pub definition: Value,
    /// The actions its steps use, as pinned when the run started: `uses` → commit.
    pub actions: BTreeMap<String, String>,
    /// Its container's and services' images: reference → digest.
    pub images: BTreeMap<String, String>,
    /// Its requirements and platform.
    pub platform: Value,
    /// The listed variables' values (absent ones are left out).
    pub env: BTreeMap<String, String>,
    /// Trusted or not: untrusted legs never reuse a trusted result, nor give one.
    pub trusted: bool,
    /// The listed paths (or the whole tree) at the commit: pattern → its git objects' hash.
    pub trees: BTreeMap<String, String>,
}

/// The input hash: SHA-256 of the inputs' canonical JSON (keys sorted).
pub fn hash(i: &Inputs) -> String {
    let v = serde_json::to_value(i).unwrap_or(Value::Null);
    let digest = sha2::Sha256::digest(canonical(&v).as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// JSON with every object's keys sorted, whatever order the map keeps (a workspace may build
/// `serde_json` with `preserve_order`).
fn canonical(v: &Value) -> String {
    match v {
        Value::Object(m) => {
            let sorted: BTreeMap<&String, &Value> = m.iter().collect();
            let fields: Vec<String> = sorted.into_iter().map(|(k, v)| format!("{}:{}", Value::String(k.clone()), canonical(v))).collect();
            format!("{{{}}}", fields.join(","))
        }
        Value::Array(a) => format!("[{}]", a.iter().map(canonical).collect::<Vec<_>>().join(",")),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wf::ir::Tpl;
    use serde_json::json;

    fn inputs() -> Inputs {
        Inputs {
            definition: json!({ "job": "build", "steps": [{ "run": "cargo build" }], "matrix": { "os": "linux" }, "needs": { "gen": { "outputs": { "v": "1" } } } }),
            actions: [("actions/checkout@v4".to_owned(), "11bd71901bbe5b1630ceea73d27597364c9af683".to_owned())].into(),
            images: [("rust:1.83".to_owned(), "sha256:aaaa".to_owned())].into(),
            platform: json!({ "runs_on": ["linux"], "requires": ["cpu.threads >= 8"] }),
            env: [("RUSTFLAGS".to_owned(), "-Dwarnings".to_owned())].into(),
            trusted: true,
            trees: [("src/**".to_owned(), "t1".to_owned()), ("Cargo.lock".to_owned(), "t2".to_owned())].into(),
        }
    }

    fn set(v: &mut Value, at: &str, to: Value) {
        if let Some(x) = v.pointer_mut(at) {
            *x = to;
        }
    }

    /// W08-AC06: the hash changes when any input does, and only then.
    #[test]
    fn w08_ac06_input_hash() {
        let base = hash(&inputs());
        assert_eq!(base, hash(&inputs()), "the same inputs, the same hash");
        assert_eq!(base.len(), 64);
        type Edit = (&'static str, fn(&mut Inputs));
        let edits: [Edit; 8] = [
            ("a step", |i| set(&mut i.definition, "/steps/0/run", json!("cargo build --release"))),
            ("a needed output", |i| set(&mut i.definition, "/needs/gen/outputs/v", json!("2"))),
            ("an action's pin", |i| {
                i.actions.insert("actions/checkout@v4".into(), "0000000000000000000000000000000000000000".into());
            }),
            ("an image's digest", |i| {
                i.images.insert("rust:1.83".into(), "sha256:bbbb".into());
            }),
            ("the platform", |i| set(&mut i.platform, "/runs_on", json!(["linux", "arm64"]))),
            ("a listed variable", |i| {
                i.env.insert("RUSTFLAGS".into(), "".into());
            }),
            ("the trust", |i| i.trusted = false),
            ("a listed path", |i| {
                i.trees.insert("src/**".into(), "t1-changed".into());
            }),
        ];
        for (what, edit) in edits {
            let mut i = inputs();
            edit(&mut i);
            assert_ne!(hash(&i), base, "{what} changes it");
        }
        // Keys in any order are the same inputs.
        let mut reordered = inputs();
        reordered.definition = json!({ "needs": { "gen": { "outputs": { "v": "1" } } }, "matrix": { "os": "linux" }, "steps": [{ "run": "cargo build" }], "job": "build" });
        assert_eq!(hash(&reordered), base);
    }

    #[test]
    fn w08_r10_spec() -> Result<(), String> {
        let t = |s: &str| YVal::Str(Tpl::new(s, Default::default()));
        assert_eq!(Spec::from_yval(&YVal::Bool(true))?, Some(Spec { max_age: MAX_AGE, ..Spec::default() }));
        assert_eq!(Spec::from_yval(&YVal::Bool(false))?, None);
        let v = YVal::Map(vec![("paths".into(), YVal::Seq(vec![t("src/**"), t("Cargo.lock")])), ("env".into(), YVal::Seq(vec![t("RUSTFLAGS")])), ("max-age".into(), t("2d"))]);
        assert_eq!(Spec::from_yval(&v)?, Some(Spec { paths: vec!["src/**".into(), "Cargo.lock".into()], env: vec!["RUSTFLAGS".into()], max_age: 2 * 86_400 }));
        assert!(Spec::from_yval(&YVal::Map(vec![("max-age".into(), t("soon"))])).is_err() && Spec::from_yval(&t("yes")).is_err());
        assert_eq!((parse_age("12h"), parse_age("90"), parse_age("1w"), parse_age("x")), (Some(43_200), Some(90), Some(604_800), None));
        Ok(())
    }
}
