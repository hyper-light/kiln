//! The TypeScript codec, generated from the schema (docs/browser-protocol.md; plan step 3): one
//! file, `ui/src/api/bp/gen.ts`, checked in, holding every type the frames carry with its encoder,
//! decoder, default and fuzz draw, and the frame kinds. The runtime it calls
//! (`ui/src/api/bp/runtime.ts`: the cursor, varints, strings) is hand-written once and held to
//! this crate's rules by the shared vectors. `cargo test -p kiln-bp --test ts` fails, naming the
//! file, when the types change without it; `KILN_WRITE_BP=1` writes it.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::frames::{KINDS, OLDEST, VERSION, registry, schema_hash};
use crate::schema::{Field, Shape, Ty};

/// What the file starts with: how to make it again.
pub const HEADER: &str = "// Made from kiln-bp's Rust types (crates/kiln-bp/src/frames.rs): change those, then run\n// `KILN_WRITE_BP=1 cargo test -p kiln-bp --test ts`.\n";

/// Where the browser's protocol code lives.
pub fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ui/src/api/bp")
}

/// The generated file's path.
pub fn path() -> PathBuf {
    dir().join("gen.ts")
}

fn ts_type(t: &Ty) -> String {
    match t {
        Ty::U8 | Ty::U16 | Ty::U32 | Ty::U53 | Ty::I53 => "number".into(),
        Ty::Bits => "bigint".into(),
        Ty::Bool => "boolean".into(),
        Ty::Str => "string".into(),
        Ty::Bytes => "Uint8Array".into(),
        Ty::Opt(t) => format!("{} | null", ts_type(t)),
        Ty::Vec(t) => format!("{}[]", paren(t)),
        Ty::Named(n) => (*n).to_owned(),
    }
}

fn paren(t: &Ty) -> String {
    match t {
        Ty::Opt(_) => format!("({})", ts_type(t)),
        _ => ts_type(t),
    }
}

fn prim(t: &Ty) -> Option<&'static str> {
    Some(match t {
        Ty::U8 => "u8",
        Ty::U16 => "u16",
        Ty::U32 => "u32",
        Ty::U53 => "u53",
        Ty::I53 => "i53",
        Ty::Bits => "bits",
        Ty::Bool => "bool",
        Ty::Str => "str",
        Ty::Bytes => "bytes",
        _ => return None,
    })
}

/// A statement writing `v` (an expression) of type `t`.
fn put(t: &Ty, v: &str, depth: usize) -> String {
    if let Some(p) = prim(t) {
        return format!("w.{p}({v})");
    }
    let x = format!("x{depth}");
    match t {
        Ty::Opt(i) => format!("w.opt({v}, ({x}) => {})", put(i, &x, depth + 1)),
        Ty::Vec(i) => format!("w.vec({v}, ({x}) => {})", put(i, &x, depth + 1)),
        Ty::Named(n) => format!("put{n}(w, {v})"),
        _ => String::new(),
    }
}

/// An expression reading a `t`.
fn take(t: &Ty) -> String {
    if let Some(p) = prim(t) {
        return format!("r.{p}()");
    }
    match t {
        Ty::Opt(i) => format!("r.opt(() => {})", take(i)),
        Ty::Vec(i) => format!("r.vec(() => {})", take(i)),
        Ty::Named(n) => format!("take{n}(r)"),
        _ => String::new(),
    }
}

/// An expression drawing a `t`.
fn arb(t: &Ty) -> String {
    if let Some(p) = prim(t) {
        return format!("g.{p}()");
    }
    match t {
        Ty::Opt(i) => format!("g.opt(() => {})", arb(i)),
        Ty::Vec(i) => format!("g.vec(() => {})", arb(i)),
        Ty::Named(n) => format!("arb{n}(g)"),
        _ => String::new(),
    }
}

/// An expression for a `t`'s default.
fn default(t: &Ty) -> String {
    match t {
        Ty::U8 | Ty::U16 | Ty::U32 | Ty::U53 | Ty::I53 => "0".into(),
        Ty::Bits => "0n".into(),
        Ty::Bool => "false".into(),
        Ty::Str => "\"\"".into(),
        Ty::Bytes => "new Uint8Array(0)".into(),
        Ty::Opt(_) => "null".into(),
        Ty::Vec(_) => "[]".into(),
        Ty::Named(n) => format!("default{n}()"),
    }
}

fn fields_type(fields: &[Field]) -> String {
    fields.iter().map(|f| format!("{}: {}", f.name, ts_type(&f.ty))).collect::<Vec<_>>().join("; ")
}

fn object(fields: &[Field]) -> String {
    fields.iter().map(|f| f.name).collect::<Vec<_>>().join(", ")
}

fn render_struct(out: &mut String, name: &str, fields: &[Field], body: bool) {
    let _ = writeln!(out, "export interface {name} {{ {} }}", fields_type(fields));
    let _ = writeln!(out, "export function put{name}(w: Writer, v: {name}): void {{");
    for f in fields {
        let _ = writeln!(out, "  {};", put(&f.ty, &format!("v.{}", f.name), 0));
    }
    let _ = writeln!(out, "}}");
    let _ = writeln!(out, "export function take{name}(r: Reader): {name} {{");
    for f in fields {
        let _ = writeln!(out, "  const {} = {};", f.name, take(&f.ty));
    }
    let _ = writeln!(out, "  return {{ {} }};", object(fields));
    let _ = writeln!(out, "}}");
    let defaults: Vec<String> = fields.iter().map(|f| format!("{}: {}", f.name, default(&f.ty))).collect();
    let _ = writeln!(out, "export function default{name}(): {name} {{\n  return {{ {} }};\n}}", defaults.join(", "));
    let _ = writeln!(out, "export function arb{name}(g: Gen): {name} {{");
    for f in fields {
        let _ = writeln!(out, "  const {} = {};", f.name, arb(&f.ty));
    }
    let _ = writeln!(out, "  return {{ {} }};", object(fields));
    let _ = writeln!(out, "}}");
    if body {
        // As a frame's body: written for the peer's version, read tolerantly.
        let _ = writeln!(out, "export function put{name}Body(w: Writer, v: {name}, version: number): void {{");
        for f in fields {
            if f.since > 1 {
                let _ = writeln!(out, "  if (version < {}) return;", f.since);
            }
            let _ = writeln!(out, "  {};", put(&f.ty, &format!("v.{}", f.name), 0));
        }
        let _ = writeln!(out, "  void version;");
        let _ = writeln!(out, "}}");
        let _ = writeln!(out, "export function take{name}Body(r: Reader): {name} {{");
        let _ = writeln!(out, "  const v = default{name}();");
        for f in fields {
            let _ = writeln!(out, "  if (r.done()) return v;");
            let _ = writeln!(out, "  v.{} = {};", f.name, take(&f.ty));
        }
        let _ = writeln!(out, "  return v;");
        let _ = writeln!(out, "}}");
    }
    out.push('\n');
}

fn render_enum(out: &mut String, name: &str, variants: &[crate::schema::Variant]) {
    let arms: Vec<String> = variants
        .iter()
        .map(|v| if v.fields.is_empty() { format!("{{ t: \"{}\" }}", v.name) } else { format!("{{ t: \"{}\"; {} }}", v.name, fields_type(&v.fields)) })
        .collect();
    let _ = writeln!(out, "export type {name} = {};", arms.join(" | "));
    let _ = writeln!(out, "export function put{name}(w: Writer, v: {name}): void {{");
    let _ = writeln!(out, "  switch (v.t) {{");
    for v in variants {
        let _ = writeln!(out, "    case \"{}\":", v.name);
        let _ = writeln!(out, "      w.u32({});", v.index);
        for f in &v.fields {
            let _ = writeln!(out, "      {};", put(&f.ty, &format!("v.{}", f.name), 0));
        }
        let _ = writeln!(out, "      return;");
    }
    let _ = writeln!(out, "  }}");
    let _ = writeln!(out, "}}");
    let _ = writeln!(out, "export function take{name}(r: Reader): {name} {{");
    let _ = writeln!(out, "  switch (r.variant()) {{");
    for v in variants {
        if v.fields.is_empty() {
            let _ = writeln!(out, "    case {}:\n      return {{ t: \"{}\" }};", v.index, v.name);
        } else {
            let _ = writeln!(out, "    case {}: {{", v.index);
            for f in &v.fields {
                let _ = writeln!(out, "      const {} = {};", f.name, take(&f.ty));
            }
            let _ = writeln!(out, "      return {{ t: \"{}\", {} }};", v.name, object(&v.fields));
            let _ = writeln!(out, "    }}");
        }
    }
    let _ = writeln!(out, "    default:\n      throw new BpError(Why.UnknownVariant);");
    let _ = writeln!(out, "  }}");
    let _ = writeln!(out, "}}");
    if let Some(first) = variants.first() {
        let fs: Vec<String> = first.fields.iter().map(|f| format!(", {}: {}", f.name, default(&f.ty))).collect();
        let _ = writeln!(out, "export function default{name}(): {name} {{\n  return {{ t: \"{}\"{} }};\n}}", first.name, fs.concat());
    }
    let _ = writeln!(out, "export function arb{name}(g: Gen): {name} {{");
    let _ = writeln!(out, "  switch (g.index({})) {{", variants.len());
    for (i, v) in variants.iter().enumerate() {
        if v.fields.is_empty() {
            let _ = writeln!(out, "    case {i}:\n      return {{ t: \"{}\" }};", v.name);
        } else {
            let _ = writeln!(out, "    case {i}: {{");
            for f in &v.fields {
                let _ = writeln!(out, "      const {} = {};", f.name, arb(&f.ty));
            }
            let _ = writeln!(out, "      return {{ t: \"{}\", {} }};", v.name, object(&v.fields));
            let _ = writeln!(out, "    }}");
        }
    }
    let _ = writeln!(out, "    default:\n      return default{name}();");
    let _ = writeln!(out, "  }}");
    let _ = writeln!(out, "}}\n");
}

/// The file's text.
pub fn render() -> String {
    let reg = registry();
    let bodies: Vec<&str> = KINDS.iter().map(|k| k.body()).collect();
    let mut out = String::from(HEADER);
    out.push_str("import { BpError, Gen, Reader, Why, Writer } from \"./runtime\";\n\n");
    let _ = writeln!(out, "/** The protocol version this build speaks, and the oldest it still serves. */");
    let _ = writeln!(out, "export const VERSION = {VERSION};");
    let _ = writeln!(out, "export const OLDEST = {OLDEST};");
    let _ = writeln!(out, "/** The schema's hash, said in `Open`: two builds that disagree on a layout find out at once. */");
    let _ = writeln!(out, "export const SCHEMA_HASH = {}n;\n", schema_hash());
    for shape in reg.shapes.values() {
        match shape {
            Shape::Struct { name, fields } => render_struct(&mut out, name, fields, bodies.contains(name)),
            Shape::Enum { name, variants } => render_enum(&mut out, name, variants),
        }
    }
    // The kinds.
    let names: Vec<String> = KINDS.iter().map(|k| format!("\"{}\"", k.name())).collect();
    let _ = writeln!(out, "/** Every kind, by its index on the wire. */");
    let _ = writeln!(out, "export const KINDS = [{}] as const;", names.join(", "));
    let _ = writeln!(out, "export type KindName = (typeof KINDS)[number];");
    let arms: Vec<String> = KINDS.iter().map(|k| format!("{{ kind: \"{}\"; body: {} }}", k.name(), k.body())).collect();
    let _ = writeln!(out, "export type Frame = {};", arms.join(" | "));
    let _ = writeln!(out, "export const KIND_INDEX: {{ readonly [K in KindName]: number }} = {{ {} }};", KINDS.iter().map(|k| format!("{}: {}", k.name(), k.index())).collect::<Vec<_>>().join(", "));
    let _ = writeln!(out, "/** Writes a frame's body for a peer speaking `version`. */");
    let _ = writeln!(out, "export function putFrameBody(w: Writer, f: Frame, version: number): void {{");
    let _ = writeln!(out, "  switch (f.kind) {{");
    for k in KINDS {
        let _ = writeln!(out, "    case \"{}\":\n      return put{}Body(w, f.body, version);", k.name(), k.body());
    }
    let _ = writeln!(out, "  }}\n}}");
    let _ = writeln!(out, "/** Reads a body of the kind at `index` (tolerantly: missing trailing fields default, newer ones are left). */");
    let _ = writeln!(out, "export function takeFrameBody(index: number, r: Reader): Frame {{");
    let _ = writeln!(out, "  switch (index) {{");
    for k in KINDS {
        let _ = writeln!(out, "    case {}:\n      return {{ kind: \"{}\", body: take{}Body(r) }};", k.index(), k.name(), k.body());
    }
    let _ = writeln!(out, "    default:\n      throw new BpError(Why.UnknownKind);");
    let _ = writeln!(out, "  }}\n}}");
    let _ = writeln!(out, "/** A frame drawn from `g`: its kind, then its body (as kiln-bp's `Frame::arb`). */");
    let _ = writeln!(out, "export function arbFrame(g: Gen): Frame {{");
    let _ = writeln!(out, "  switch (g.index(KINDS.length)) {{");
    for (i, k) in KINDS.iter().enumerate() {
        let _ = writeln!(out, "    case {i}:\n      return {{ kind: \"{}\", body: arb{}(g) }};", k.name(), k.body());
    }
    let _ = writeln!(out, "    default:\n      return {{ kind: \"Idle\", body: arbIdle(g) }};");
    let _ = writeln!(out, "  }}\n}}");
    out
}
