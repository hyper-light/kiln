//! MCP elicitation: an MCP server asking the person for input in the middle of a task (an API
//! token, a choice, a confirmation, a page to open). Claude Code and Codex both pass these on, in
//! the shape of MCP 2025-11-25: a `form` with a `requested_schema`, or a `url` to open. kiln asks
//! them as it asks any question: the form's fields become the console's questions, and the answers
//! become the typed content the server asked for.

use serde_json::{Map, Value, json};

use super::model::{ChoiceAnswer, Question, QuestionOption};

const YES: &str = "Yes";
const NO: &str = "No";

/// An elicitation as kiln asks it.
#[derive(Debug, Clone, PartialEq)]
pub struct Ask {
    pub title: String,
    /// What it shows beyond its questions: the page to open, for a `url` elicitation.
    pub detail: Option<String>,
    pub questions: Vec<Question>,
}

/// Read one: `server` the MCP server's name, `message` what it asks, `mode` "form" (with
/// `schema`) or "url" (with `url`).
pub fn ask(server: &str, message: &str, mode: Option<&str>, schema: Option<&Value>, url: Option<&str>) -> Ask {
    let server = if server.trim().is_empty() { "An MCP server" } else { server.trim() };
    let message = message.trim();
    let title: String = if message.is_empty() { format!("{server} asks for your input") } else { format!("{server} asks: {message}") }.chars().take(240).collect();
    if mode == Some("url") {
        let detail = url.map(|u| format!("Open {u} to continue there, then accept here (or decline)."));
        return Ask { title, detail, questions: Vec::new() };
    }
    let questions = schema.map(fields).unwrap_or_default().iter().map(Field::question).collect();
    Ask { title, detail: None, questions }
}

/// The answer to send: `accept` with the form's content (none for a page), or `decline`; and why
/// it was declined when the answers did not fit the form.
pub fn answer(schema: Option<&Value>, accept: bool, choices: &[ChoiceAnswer]) -> (Value, Option<String>) {
    if !accept {
        return (json!({"action": "decline"}), None);
    }
    let Some(schema) = schema.filter(|s| s.is_object()) else { return (json!({"action": "accept"}), None) };
    match content(schema, choices) {
        Ok(c) => (json!({"action": "accept", "content": c}), None),
        Err(why) => (json!({"action": "decline"}), Some(why)),
    }
}

/// One field of a form.
#[derive(Debug, Clone, PartialEq)]
struct Field {
    name: String,
    label: String,
    about: Option<String>,
    required: bool,
    default: Option<Value>,
    kind: Kind,
}

#[derive(Debug, Clone, PartialEq)]
enum Kind {
    Text { format: Option<String> },
    Number { integer: bool, min: Option<f64>, max: Option<f64> },
    Bool,
    /// (value, label)
    One(Vec<(String, String)>),
    Many(Vec<(String, String)>),
}

/// A form's fields: the required ones first, each group in the order the server wrote them.
fn fields(schema: &Value) -> Vec<Field> {
    let required: Vec<&str> = schema.get("required").and_then(Value::as_array).map(|r| r.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    let Some(props) = schema.get("properties").and_then(Value::as_object) else { return Vec::new() };
    let mut out: Vec<Field> = props.iter().map(|(name, p)| field(name, p, required.contains(&name.as_str()))).collect();
    out.sort_by_key(|f| !f.required);
    out
}

fn field(name: &str, p: &Value, required: bool) -> Field {
    let s = |k: &str| p.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
    let f = |k: &str| p.get(k).and_then(Value::as_f64);
    let kind = match s("type") {
        Some("boolean") => Kind::Bool,
        Some(t @ ("number" | "integer")) => Kind::Number { integer: t == "integer", min: f("minimum"), max: f("maximum") },
        Some("array") => Kind::Many(options(p.get("items"))),
        _ => match options(Some(p)) {
            o if o.is_empty() => Kind::Text { format: s("format").map(str::to_owned) },
            o => Kind::One(o),
        },
    };
    Field {
        name: name.to_owned(),
        label: s("title").unwrap_or(name).to_owned(),
        about: s("description").map(str::to_owned),
        required,
        default: p.get("default").filter(|d| !d.is_null()).cloned(),
        kind,
    }
}

/// An enum's (value, label) pairs: `oneOf`/`anyOf` of `{const, title}`, or `enum` with the older
/// `enumNames`.
fn options(p: Option<&Value>) -> Vec<(String, String)> {
    let Some(p) = p else { return Vec::new() };
    if let Some(list) = p.get("oneOf").or_else(|| p.get("anyOf")).and_then(Value::as_array) {
        return list
            .iter()
            .filter_map(|o| {
                let v = o.get("const")?.as_str()?;
                Some((v.to_owned(), o.get("title").and_then(Value::as_str).unwrap_or(v).to_owned()))
            })
            .collect();
    }
    let names: Vec<&str> = p.get("enumNames").and_then(Value::as_array).map(|n| n.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    p.get("enum")
        .and_then(Value::as_array)
        .map(|e| e.iter().filter_map(Value::as_str).enumerate().map(|(i, v)| (v.to_owned(), names.get(i).copied().unwrap_or(v).to_owned())).collect())
        .unwrap_or_default()
}

impl Field {
    fn question(&self) -> Question {
        let hint = match &self.kind {
            Kind::Text { format: Some(f) } => match f.as_str() {
                "email" => " (an email address)",
                "uri" => " (a URL)",
                "date" => " (a date, YYYY-MM-DD)",
                "date-time" => " (a date and time)",
                _ => "",
            },
            Kind::Number { integer: true, .. } => " (a whole number)",
            Kind::Number { .. } => " (a number)",
            _ => "",
        };
        let range = match &self.kind {
            Kind::Number { min: Some(a), max: Some(b), .. } => format!(" From {a} to {b}."),
            Kind::Number { min: Some(a), .. } => format!(" At least {a}."),
            Kind::Number { max: Some(b), .. } => format!(" At most {b}."),
            _ => String::new(),
        };
        let default = self.default.as_ref().map(|d| format!(" Default: {}.", d.as_str().map_or_else(|| d.to_string(), str::to_owned))).unwrap_or_default();
        let base = self.about.clone().unwrap_or_else(|| self.label.clone());
        let opts = |o: &[(String, String)]| o.iter().map(|(_, l)| QuestionOption { label: l.clone(), ..QuestionOption::default() }).collect();
        let (options, multi, other) = match &self.kind {
            Kind::Bool => (vec![QuestionOption { label: YES.into(), ..QuestionOption::default() }, QuestionOption { label: NO.into(), ..QuestionOption::default() }], false, false),
            Kind::One(o) => (opts(o), false, false),
            Kind::Many(o) => (opts(o), true, false),
            Kind::Text { .. } | Kind::Number { .. } => (Vec::new(), false, true),
        };
        Question {
            id: self.name.clone(),
            question: format!("{base}{hint}{range}{default}").trim().to_owned(),
            header: if self.required { self.label.clone() } else { format!("{} (optional)", self.label) },
            multi,
            other,
            secret: false,
            options,
        }
    }

    /// Its value from an answer: `None` when it was left empty.
    fn value(&self, a: Option<&ChoiceAnswer>) -> Result<Option<Value>, String> {
        let picked: Vec<&str> = a.map(|c| c.selected.iter().map(String::as_str).collect()).unwrap_or_default();
        let typed = a.and_then(|c| c.other.as_deref()).map(str::trim).filter(|t| !t.is_empty());
        let one = picked.first().copied().or(typed);
        let named = |o: &[(String, String)], x: &str| o.iter().find(|(v, l)| l == x || v == x).map(|(v, _)| v.clone());
        let v = match &self.kind {
            Kind::Text { .. } => one.map(|t| json!(t)),
            Kind::Bool => match one {
                None => None,
                Some(t) if t.eq_ignore_ascii_case(YES) || t.eq_ignore_ascii_case("true") || t.eq_ignore_ascii_case("y") => Some(json!(true)),
                Some(t) if t.eq_ignore_ascii_case(NO) || t.eq_ignore_ascii_case("false") || t.eq_ignore_ascii_case("n") => Some(json!(false)),
                Some(t) => return Err(format!("{}: answer yes or no, not {t:?}", self.label)),
            },
            Kind::Number { integer, min, max } => match one {
                None => None,
                Some(t) => {
                    let n: f64 = t.parse().map_err(|_| format!("{}: {t:?} is not a number", self.label))?;
                    if min.is_some_and(|m| n < m) || max.is_some_and(|m| n > m) {
                        return Err(format!("{}: {t} is out of range", self.label));
                    }
                    if *integer {
                        Some(json!(t.parse::<i64>().map_err(|_| format!("{}: {t:?} is not a whole number", self.label))?))
                    } else {
                        Some(json!(n))
                    }
                }
            },
            Kind::One(o) => match one {
                None => None,
                Some(x) => Some(json!(named(o, x).ok_or_else(|| format!("{}: {x:?} is not one of its choices", self.label))?)),
            },
            Kind::Many(o) => {
                let mut values = Vec::new();
                for x in picked.iter().copied().chain(typed) {
                    values.push(named(o, x).ok_or_else(|| format!("{}: {x:?} is not one of its choices", self.label))?);
                }
                (!values.is_empty()).then(|| json!(values))
            }
        };
        Ok(v)
    }
}

/// The form's content from the console's answers (matched by field name, then by question).
fn content(schema: &Value, choices: &[ChoiceAnswer]) -> Result<Value, String> {
    let mut out = Map::new();
    for f in fields(schema) {
        let q = f.question();
        let a = choices.iter().find(|c| c.id.as_deref() == Some(f.name.as_str())).or_else(|| choices.iter().find(|c| c.question == q.question));
        match f.value(a)?.or_else(|| f.default.clone()) {
            Some(v) => {
                out.insert(f.name.clone(), v);
            }
            None if f.required => return Err(format!("{} is required", f.label)),
            None => {}
        }
    }
    Ok(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "token": {"type": "string", "title": "API token", "description": "Your Linear API token"},
                "email": {"type": "string", "format": "email"},
                "port": {"type": "integer", "minimum": 1, "maximum": 65535, "default": 8080},
                "ratio": {"type": "number"},
                "confirm": {"type": "boolean", "title": "Proceed?"},
                "env": {"type": "string", "enum": ["prod", "dev"], "enumNames": ["Production", "Development"]},
                "region": {"type": "string", "oneOf": [{"const": "eu", "title": "Europe"}, {"const": "us", "title": "United States"}]},
                "tags": {"type": "array", "items": {"anyOf": [{"const": "a", "title": "Alpha"}, {"const": "b", "title": "Beta"}]}}
            },
            "required": ["token", "confirm", "env"]
        })
    }

    fn said(id: &str, selected: &[&str], other: Option<&str>) -> ChoiceAnswer {
        ChoiceAnswer { id: Some(id.into()), selected: selected.iter().map(|s| (*s).to_owned()).collect(), other: other.map(str::to_owned), ..ChoiceAnswer::default() }
    }

    #[test]
    fn a_form_becomes_questions() {
        let a = ask("linear", "Connect your Linear account", Some("form"), Some(&schema()), None);
        assert_eq!(a.title, "linear asks: Connect your Linear account");
        let ids: Vec<&str> = a.questions.iter().map(|q| q.id.as_str()).collect();
        assert_eq!(ids, ["token", "confirm", "env", "email", "port", "ratio", "region", "tags"], "required first, each in the order the server gave");
        let q = |id: &str| a.questions.iter().find(|q| q.id == id).cloned().unwrap_or_default();
        assert_eq!((q("token").header.as_str(), q("token").question.as_str(), q("token").other), ("API token", "Your Linear API token", true));
        assert_eq!(q("confirm").options.iter().map(|o| o.label.as_str()).collect::<Vec<_>>(), [YES, NO]);
        assert_eq!(q("env").options.iter().map(|o| o.label.as_str()).collect::<Vec<_>>(), ["Production", "Development"]);
        assert_eq!(q("region").options.iter().map(|o| o.label.as_str()).collect::<Vec<_>>(), ["Europe", "United States"]);
        assert!(q("tags").multi && q("port").header.ends_with("(optional)"));
        assert_eq!(q("port").question, "port (a whole number) From 1 to 65535. Default: 8080.");
        assert_eq!(q("email").question, "email (an email address)");
    }

    #[test]
    fn answers_become_typed_content() {
        let choices = [
            said("token", &[], Some("lin_123")),
            said("confirm", &[YES], None),
            said("env", &["Development"], None),
            said("region", &["Europe"], None),
            said("tags", &["Alpha", "Beta"], None),
            said("ratio", &[], Some("0.5")),
        ];
        let (r, why) = answer(Some(&schema()), true, &choices);
        assert_eq!(why, None);
        assert_eq!(r, json!({"action": "accept", "content": {"token": "lin_123", "confirm": true, "env": "dev", "region": "eu", "tags": ["a", "b"], "ratio": 0.5, "port": 8080}}));
        // Declined, a page accepted, and answers that don't fit.
        assert_eq!(answer(Some(&schema()), false, &choices).0, json!({"action": "decline"}));
        assert_eq!(answer(None, true, &[]).0, json!({"action": "accept"}));
        let (r, why) = answer(Some(&schema()), true, &[said("token", &[], Some("x")), said("confirm", &[NO], None), said("env", &["prod"], None), said("port", &[], Some("http"))]);
        assert_eq!((r, why.as_deref()), (json!({"action": "decline"}), Some("port: \"http\" is not a number")));
        assert_eq!(answer(Some(&schema()), true, &[said("token", &[], Some("x"))]).1.as_deref(), Some("Proceed? is required"));
    }

    #[test]
    fn a_page_to_open_is_shown() {
        let a = ask("", "Sign in to GitHub", Some("url"), None, Some("https://github.com/login/device"));
        assert_eq!(a.title, "An MCP server asks: Sign in to GitHub");
        assert!(a.questions.is_empty() && a.detail.as_deref().is_some_and(|d| d.contains("https://github.com/login/device")));
    }
}
