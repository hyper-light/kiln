//! W29-R27's `@kiln/sdk`: its declarations hold every key of the kiln dialect's schema and no other,
//! its runtime exports what they declare, and the copy checked in for the UI's type check is the
//! generator's.

use std::collections::BTreeSet;

use serde_json::Value;

use super::*;

/// Every property name of the schema's objects.
fn schema_keys(v: &Value, out: &mut BTreeSet<String>) {
    match v {
        Value::Object(o) => {
            if let Some(Value::Object(props)) = o.get("properties") {
                out.extend(props.keys().cloned());
            }
            o.values().for_each(|x| schema_keys(x, out));
        }
        Value::Array(a) => a.iter().for_each(|x| schema_keys(x, out)),
        _ => {}
    }
}

/// Every property the declarations name, bare or quoted.
fn declared_keys(dts: &str) -> BTreeSet<String> {
    let re = regex::Regex::new(r#"^\s+(?:"([^"]+)"|([A-Za-z_$][\w$]*))\??: "#).ok();
    dts.lines().filter_map(|l| re.as_ref()?.captures(l)).filter_map(|c| c.get(1).or_else(|| c.get(2)).map(|m| m.as_str().to_owned())).collect()
}

#[test]
fn w29_r27_every_kiln_key_is_declared_and_no_other() {
    let mut want = BTreeSet::new();
    schema_keys(&schema(SchemaKind::Kiln), &mut want);
    let got = declared_keys(&dts());
    assert!(want.len() > 100, "{} keys", want.len());
    let missing: Vec<&String> = want.difference(&got).collect();
    let extra: Vec<&String> = got.difference(&want).collect();
    assert!(missing.is_empty() && extra.is_empty(), "not declared: {missing:?}; declared but not the schema's: {extra:?}");
}

#[test]
fn w29_r27_the_runtime_exports_what_is_declared() {
    let re = regex::Regex::new(r"(?m)^export const (\w+) =").ok();
    let exported: BTreeSet<&str> = re.iter().flat_map(|re| re.captures_iter(RUNTIME)).filter_map(|c| c.get(1).map(|m| m.as_str())).collect();
    let listed: BTreeSet<&str> = FUNCTIONS.iter().map(|f| f.0).chain(VALUES.iter().map(|v| v.0)).collect();
    assert_eq!(exported, listed);
    let d = dts();
    for (name, ..) in FUNCTIONS {
        assert!(d.contains(&format!("export function {name}(")), "{name}");
    }
    for (name, ..) in VALUES {
        assert!(d.contains(&format!("export const {name}: ")), "{name}");
    }
}

/// The copy the UI's type check reads (`ui/tests/kiln-sdk.test.ts`): the generator's.
/// `KILN_BLESS=1` writes it.
#[test]
fn w29_r27_the_checked_in_declarations_are_the_generators() -> Result<(), String> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../kiln/tests/fixtures/code/kiln.d.ts");
    let want = dts();
    if std::env::var_os("KILN_BLESS").is_some() {
        std::fs::write(path, &want).map_err(|e| e.to_string())?;
    }
    let got = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    if got != want {
        return Err(format!("{path} isn't the generator's: run this test with KILN_BLESS=1"));
    }
    Ok(())
}
