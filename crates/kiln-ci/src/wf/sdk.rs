//! `@kiln/sdk` (docs/ci.md W29-R27): what a code workflow imports. Its types are generated from
//! W02-R09's kiln-dialect JSON Schema ([`super::schema`]), made from the tables W02 checks against,
//! so an editor's types and the validator agree. Its functions and values are [`RUNTIME`]'s, the
//! module the server's QuickJS gives a program, declared from the one list each, [`FUNCTIONS`] and
//! [`VALUES`]. `GET /api/ci/sdk/kiln.d.ts` serves [`dts`].

use serde_json::Value;

use super::schema::{SchemaKind, schema};

/// The SDK's functions: each name, its signature and what it does.
pub const FUNCTIONS: &[(&str, &str, &str)] = &[
    ("workflow", "(w: Workflow): Workflow", "The workflow, its line kept for diagnostics."),
    ("job", "(j: Job): Job", "A job, its line kept for diagnostics."),
    ("step", "(s: Step): Step", "A step, its line kept for diagnostics."),
];

/// The SDK's values: each name, its type and what it is.
pub const VALUES: &[(&str, &str, &str)] = &[
    ("event", "Event", "The run's event, as `github.event`: the payload of what started it."),
    (
        "tree",
        "{\n    /** A file's text at the commit, or null when it has none at `path`. */\n    read(path: string): string | null;\n    /** The entries of `dir`, from the tree's root, a directory's ending in `/`. */\n    list(dir: string): string[];\n  }",
        "The commit's tree, read-only.",
    ),
];

/// The module QuickJS gives a program as `@kiln/sdk`. `workflow`, `job` and `step` return what
/// they're given, keeping where their caller called them; `globalThis.__kiln` is the server's:
/// the event, the tree's two reads, and `positions`, which the server calls on what the program
/// made to learn where each part was written.
pub const RUNTIME: &str = r#"const host = globalThis.__kiln;
const at = new WeakMap();
const keep = (v) => {
  if (v !== null && typeof v === "object") {
    const frame = String(new Error().stack ?? "").split("\n").find((l) => l.includes(":") && !l.includes("@kiln/sdk"));
    const m = frame === undefined ? null : /:(\d+):(\d+)\)?\s*$/.exec(frame);
    if (m !== null) at.set(v, [Number(m[1]), Number(m[2])]);
  }
  return v;
};
host.positions = (root) => {
  const out = [];
  const seen = new Set();
  const walk = (v, path) => {
    if (v === null || typeof v !== "object" || seen.has(v)) return;
    seen.add(v);
    const p = at.get(v);
    if (p !== undefined) out.push([path, p[0], p[1]]);
    if (Array.isArray(v)) v.forEach((x, i) => walk(x, `${path}[${i}]`));
    else for (const k of Object.keys(v)) walk(v[k], path === "" ? k : `${path}.${k}`);
  };
  walk(root, "");
  return out;
};
export const workflow = (w) => keep(w);
export const job = (j) => keep(j);
export const step = (s) => keep(s);
export const event = host.event;
export const tree = Object.freeze({ read: (path) => host.read(String(path)) ?? null, list: (dir) => host.list(String(dir)) });
"#;

/// The declarations of `@kiln/sdk`: the kiln dialect's types, then the SDK's own.
pub fn dts() -> String {
    let root = schema(SchemaKind::Kiln);
    let jobs = root.pointer("/properties/jobs/patternProperties").and_then(Value::as_object).and_then(|p| p.values().next());
    let steps = jobs.and_then(|j| j.pointer("/properties/steps/items"));
    let on = root.pointer("/properties/on");
    let names = Names { job: jobs, step: steps, on };
    let mut out = String::from("// @kiln/sdk (docs/ci.md W29-R27): the types of kiln's code workflows, from the kiln dialect's\n// JSON Schema (GET /api/ci/schema/kiln). Served at GET /api/ci/sdk/kiln.d.ts.\ndeclare module \"@kiln/sdk\" {\n");
    let title = root.get("title").and_then(Value::as_str).unwrap_or("kiln workflow");
    out.push_str(&format!("  /** {} */\n  export type Workflow = {};\n", doc(title), names.object(&root, 1)));
    if let Some(j) = jobs {
        out.push_str(&format!("  /** A job. */\n  export type Job = {};\n", names.object(j, 1)));
    }
    if let Some(s) = steps {
        out.push_str(&format!("  /** A step. */\n  export type Step = {};\n", names.object(s, 1)));
    }
    if let Some(o) = on {
        out.push_str(&format!("  /** What starts the workflow. */\n  export type On = {};\n", names.ty(o, 1, true)));
    }
    out.push_str("  /** A W01 expression: text holding `${{ … }}`, evaluated when the job runs. */\n  export type Expr = `${string}\\${{${string}`;\n");
    out.push_str("  /** An event's payload, as GitHub's webhooks send it. */\n  export interface Event {\n    readonly [key: string]: any;\n  }\n");
    for (name, ty, what) in VALUES {
        out.push_str(&format!("  /** {what} */\n  export const {name}: {ty};\n"));
    }
    for (name, sig, what) in FUNCTIONS {
        out.push_str(&format!("  /** {what} */\n  export function {name}{sig};\n"));
    }
    out.push_str("  /** A code workflow's default export: a workflow, or a function of the event that returns one. */\n  export type Program = Workflow | ((event: Event) => Workflow);\n}\n");
    out
}

/// The parts of the schema declared by name.
struct Names<'a> {
    job: Option<&'a Value>,
    step: Option<&'a Value>,
    on: Option<&'a Value>,
}

impl Names<'_> {
    /// The TypeScript of schema `s`, at nesting `depth`; `top` for a declaration's own type, never
    /// its name.
    fn ty(&self, s: &Value, depth: usize, top: bool) -> String {
        if !top {
            for (named, name) in [(self.job, "Job"), (self.step, "Step"), (self.on, "On")] {
                if named.is_some_and(|n| std::ptr::eq(n, s)) {
                    return name.to_owned();
                }
            }
        }
        if let Some(any) = s.get("anyOf").and_then(Value::as_array) {
            let mut alts: Vec<String> = Vec::new();
            for t in any.iter().map(|a| self.ty(a, depth, false)) {
                if !alts.contains(&t) {
                    alts.push(t);
                }
            }
            return alts.join(" | ");
        }
        if let Some(e) = s.get("enum").and_then(Value::as_array) {
            return e.iter().map(|v| serde_json::to_string(v).unwrap_or_default()).collect::<Vec<_>>().join(" | ");
        }
        match s.get("type") {
            Some(Value::String(t)) => self.of_type(t, s, depth),
            Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).map(|t| self.of_type(t, s, depth)).collect::<Vec<_>>().join(" | "),
            _ => "unknown".to_owned(),
        }
    }

    fn of_type(&self, t: &str, s: &Value, depth: usize) -> String {
        match t {
            // W01's expressions, which the schema matches by their `${{`.
            "string" if s.get("pattern").and_then(Value::as_str) == Some("\\$\\{\\{") => "Expr".to_owned(),
            "string" | "boolean" | "number" | "null" => t.to_owned(),
            "array" => match s.get("items") {
                Some(items) => {
                    let item = self.ty(items, depth, false);
                    if combined(&item) { format!("({item})[]") } else { format!("{item}[]") }
                }
                None => "unknown[]".to_owned(),
            },
            "object" => self.object(s, depth),
            _ => "unknown".to_owned(),
        }
    }

    /// An object schema's TypeScript: its properties (optional unless required), and what other
    /// keys may hold.
    fn object(&self, s: &Value, depth: usize) -> String {
        let pad = "  ".repeat(depth + 1);
        let required: Vec<&str> = s.get("required").and_then(Value::as_array).map(|r| r.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        let mut body = String::new();
        if let Some(props) = s.get("properties").and_then(Value::as_object) {
            for (k, p) in props {
                if let Some(d) = p.get("description").and_then(Value::as_str) {
                    body.push_str(&format!("{pad}/** {} */\n", doc(d)));
                }
                let opt = if required.contains(&k.as_str()) { "" } else { "?" };
                body.push_str(&format!("{pad}{}{opt}: {};\n", key(k), self.ty(p, depth + 1, false)));
            }
        }
        let others = s.get("patternProperties").and_then(Value::as_object).and_then(|p| p.values().next()).map(|p| ("id", self.ty(p, depth + 1, false))).or_else(|| match s.get("additionalProperties") {
            Some(Value::Bool(false)) => None,
            Some(extra @ Value::Object(_)) => Some(("key", self.ty(extra, depth + 1, false))),
            _ => Some(("key", "unknown".to_owned())),
        });
        let close = "  ".repeat(depth);
        match others {
            // Named keys beside typed others: an intersection, so a named key's type needn't fit
            // the others'.
            Some((k, t)) if !body.is_empty() && t != "unknown" => format!("{{\n{body}{close}}} & {{\n{pad}[{k}: string]: {t};\n{close}}}"),
            Some((k, t)) => format!("{{\n{body}{pad}[{k}: string]: {t};\n{close}}}"),
            None if body.is_empty() => "{}".to_owned(),
            None => format!("{{\n{body}{close}}}"),
        }
    }
}

/// A property's name as TypeScript writes it: bare when it's an identifier, quoted otherwise.
fn key(k: &str) -> String {
    let mut c = k.chars();
    let ident = c.next().is_some_and(|f| f.is_ascii_alphabetic() || f == '_' || f == '$') && c.all(|x| x.is_ascii_alphanumeric() || x == '_' || x == '$');
    if ident { k.to_owned() } else { serde_json::to_string(k).unwrap_or_default() }
}

/// Whether a type is a union or an intersection at its top, which `[]` would bind tighter than.
fn combined(t: &str) -> bool {
    let mut depth = 0i32;
    for c in t.chars() {
        match c {
            '{' | '(' | '[' | '<' => depth += 1,
            '}' | ')' | ']' | '>' => depth -= 1,
            '|' | '&' if depth == 0 => return true,
            _ => {}
        }
    }
    false
}

/// Text for a doc comment, which it can't end.
fn doc(d: &str) -> String {
    d.replace("*/", "*\\/")
}

#[cfg(test)]
mod tests;
