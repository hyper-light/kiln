//! `workflow_dispatch` inputs (docs/ci.md W03-R11), GitHub-exact: what was given is checked against
//! what the workflow declares (`string`, `boolean`, `choice`, `number`, `environment`), required
//! inputs must be there, defaults fill the rest.
//!
//! Two forms come out. The `inputs` context keeps booleans and numbers typed; `github.event.inputs`
//! has every value as a string, as GitHub's payload does.

use serde_json::{Map, Value};

use crate::wf::ir::{Input, YVal};

/// GitHub allows 25 inputs; kiln allows up to this many (W03-R11).
pub const MAX_INPUTS: usize = 100;

/// The inputs a dispatch gives, checked: `(context, payload)`.
pub fn inputs(declared: &[Input], given: &Map<String, Value>) -> Result<(Value, Value), String> {
    if declared.len() > MAX_INPUTS {
        return Err(format!("A workflow_dispatch may declare at most {MAX_INPUTS} inputs; this one declares {}.", declared.len()));
    }
    let unknown: Vec<&String> = given.keys().filter(|k| !declared.iter().any(|i| i.name.eq_ignore_ascii_case(k))).collect();
    if !unknown.is_empty() {
        let names: Vec<String> = unknown.iter().map(|k| format!("\"{k}\"")).collect();
        return Err(format!("Unexpected inputs provided: [{}]", names.join(", ")));
    }
    let (mut typed, mut strings) = (Map::new(), Map::new());
    for i in declared {
        let value = given.iter().find(|(k, _)| k.eq_ignore_ascii_case(&i.name)).map(|(_, v)| v.clone()).or_else(|| i.default.as_ref().map(default_json));
        let Some(value) = value.filter(|v| !v.is_null()) else {
            if i.required {
                return Err(format!("Required input '{}' not provided", i.name));
            }
            // Absent and optional: booleans are false, everything else empty, as on GitHub.
            let (t, s) = if i.ty.as_deref() == Some("boolean") { (Value::Bool(false), "false".to_owned()) } else { (Value::String(String::new()), String::new()) };
            typed.insert(i.name.clone(), t);
            strings.insert(i.name.clone(), Value::String(s));
            continue;
        };
        let (t, s) = typed_value(i, &value)?;
        typed.insert(i.name.clone(), t);
        strings.insert(i.name.clone(), Value::String(s));
    }
    Ok((Value::Object(typed), Value::Object(strings)))
}

/// A default as written (GitHub doesn't evaluate expressions in them).
fn default_json(y: &YVal) -> Value {
    match y {
        YVal::Null => Value::Null,
        YVal::Bool(b) => Value::Bool(*b),
        YVal::Num(n) => serde_json::Number::from_f64(*n).map_or(Value::Null, Value::Number),
        YVal::Str(t) => Value::String(t.src.clone()),
        YVal::Seq(s) => Value::Array(s.iter().map(default_json).collect()),
        YVal::Map(m) => Value::Object(m.iter().map(|(k, v)| (k.clone(), default_json(v))).collect()),
    }
}

fn as_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn typed_value(i: &Input, v: &Value) -> Result<(Value, String), String> {
    let text = as_text(v);
    match i.ty.as_deref().unwrap_or("string") {
        "boolean" => match text.trim().to_ascii_lowercase().as_str() {
            "true" => Ok((Value::Bool(true), "true".into())),
            "false" => Ok((Value::Bool(false), "false".into())),
            _ => Err(format!("Provided value '{text}' for input '{}' is not a boolean (true or false)", i.name)),
        },
        "number" => {
            let n: f64 = text.trim().parse().map_err(|_| format!("Provided value '{text}' for input '{}' is not a number", i.name))?;
            let json = serde_json::Number::from_f64(n).map(Value::Number).ok_or_else(|| format!("Provided value '{text}' for input '{}' is not a number", i.name))?;
            Ok((json, text.trim().to_owned()))
        }
        "choice" => {
            if i.options.iter().any(|o| o == &text) {
                Ok((Value::String(text.clone()), text))
            } else {
                Err(format!("Provided value '{text}' for input '{}' not in the list of allowed values", i.name))
            }
        }
        // `string` and `environment` (an environment's name; W15 checks it exists).
        _ => Ok((Value::String(text.clone()), text)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(name: &str, ty: &str, required: bool, default: Option<YVal>, options: &[&str]) -> Input {
        Input { name: name.into(), description: None, required, default, ty: Some(ty.into()), options: options.iter().map(|s| (*s).to_owned()).collect() }
    }

    fn given(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn w03_r11_inputs_are_typed_defaulted_and_checked() -> Result<(), String> {
        let declared = vec![
            input("env", "choice", true, None, &["staging", "prod"]),
            input("dry", "boolean", false, Some(YVal::Bool(true)), &[]),
            input("count", "number", false, None, &[]),
            input("note", "string", false, None, &[]),
            input("debug", "boolean", false, None, &[]),
        ];
        let (typed, strings) = inputs(&declared, &given(serde_json::json!({ "env": "prod", "count": "3" })))?;
        assert_eq!(typed, serde_json::json!({ "env": "prod", "dry": true, "count": 3.0, "note": "", "debug": false }));
        assert_eq!(strings, serde_json::json!({ "env": "prod", "dry": "true", "count": "3", "note": "", "debug": "false" }));
        // Booleans given as strings or booleans; input names without regard to case.
        let (typed, _) = inputs(&declared, &given(serde_json::json!({ "ENV": "staging", "dry": "FALSE" })))?;
        assert_eq!(typed.get("dry"), Some(&Value::Bool(false)));
        // GitHub's refusals.
        assert_eq!(inputs(&declared, &given(serde_json::json!({}))).err().as_deref(), Some("Required input 'env' not provided"));
        assert!(inputs(&declared, &given(serde_json::json!({ "env": "dev" }))).err().is_some_and(|e| e.contains("not in the list of allowed values")));
        assert!(inputs(&declared, &given(serde_json::json!({ "env": "prod", "dry": "maybe" }))).is_err());
        assert!(inputs(&declared, &given(serde_json::json!({ "env": "prod", "count": "x" }))).is_err());
        assert_eq!(inputs(&declared, &given(serde_json::json!({ "env": "prod", "other": 1 }))).err().as_deref(), Some("Unexpected inputs provided: [\"other\"]"));
        Ok(())
    }
}
