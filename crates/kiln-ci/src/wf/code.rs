//! Code workflows' types, erased (docs/ci.md W29-R27). A `.kiln/<name>.ts` runs as the JavaScript
//! its types leave: each character of type syntax becomes whitespace of its own UTF-8 length (a
//! line end, a tab or a line separator stays), so every offset, line and column of the JavaScript is
//! the TypeScript's own, as TypeScript's `erasableSyntaxOnly` and Node's type stripping have it.
//! Node 24's `stripTypeScriptTypes` in strip mode is the reference, byte for byte
//! (`tests/fixtures/erase`), down to its two repairs: a `;` where an erased statement would join the
//! statements around it, and an arrow function's `)` moved to its `=>`'s line. One difference is
//! kiln's: Node adds no `;` before a template holding `${`, which then tags the template with the
//! statement before; kiln does.
//!
//! Syntax with runtime meaning is refused (`KCI1033`) at its line and column: `enum`, a namespace
//! holding code, parameter properties, `import x =`, `export =` and `<T>x`. What the grammar can't
//! read is left as it is, for QuickJS to name (`KCI1030`). The grammar is vorpal's, the one Ship's
//! Files colour with.

use std::ops::Range as Span;

use vorpal_core::Node;
use vorpal_core::tree_sitter::StrDoc;
use vorpal_language::{LanguageExt as _, SupportLang};

use super::diag::{Diag, Pos, Range};

type N<'r> = Node<'r, StrDoc<SupportLang>>;

/// `text`'s JavaScript, or why it has none: each construct that isn't erasable.
pub fn erase(text: &str) -> Result<String, Vec<Diag>> {
    let grep = SupportLang::TypeScript.grep(text);
    let mut e = Eraser { src: text, blank: Vec::new(), put: Vec::new(), refused: Vec::new() };
    e.statements(&grep.root());
    if !e.refused.is_empty() {
        return Err(e.refused);
    }
    Ok(e.write())
}

/// Whether a code workflow's path is TypeScript, whose types [`erase`] takes out; a `.js` runs as
/// it is.
pub fn typed(path: &str) -> bool {
    path.ends_with(".ts")
}

/// The modules a program imports, each with where it names it: its imports, its re-exports and
/// its `import()`s (`None` for one whose name isn't a string). R27 lets a program import
/// `@kiln/sdk` alone.
pub fn imports(text: &str) -> Vec<(Option<String>, Range)> {
    let grep = SupportLang::TypeScript.grep(text);
    let mut out = Vec::new();
    let mut stack = vec![grep.root()];
    while let Some(n) = stack.pop() {
        let named = match n.kind().as_ref() {
            "import_statement" | "export_statement" => n.field("source").map(|s| Some(unquote(&s.text()))),
            "call_expression" if n.field("function").is_some_and(|f| f.kind() == "import") => {
                Some(n.field("arguments").and_then(|a| a.children().find(|c| c.is_named())).filter(|a| a.kind() == "string").map(|a| unquote(&a.text())))
            }
            _ => None,
        };
        match named {
            Some(name) => {
                let r = n.range();
                out.push((name, Range { start: pos(text, r.start), end: pos(text, r.end) }));
            }
            None => stack.extend(n.children()),
        }
    }
    out.sort_by_key(|(_, r)| r.start.offset);
    out
}

/// Where a program's `export default` is.
pub fn default_export(text: &str) -> Option<Pos> {
    let grep = SupportLang::TypeScript.grep(text);
    let root = grep.root();
    let at = root.children().find(|s| s.kind() == "export_statement" && s.children().any(|c| c.kind() == "default"))?.range().start;
    Some(pos(text, at))
}

/// What a program made, as YAML (R27's "What it makes"), in its own key order; and whether it's
/// GitHub-exact: read as a GitHub workflow, it uses none of kiln's keys (no `KCI5xxx`).
pub fn emitted(json: &str) -> Option<(String, bool)> {
    // Read as YAML 1.2, which JSON is, so each mapping keeps the order the program wrote it in.
    let doc = yaml_rust2::YamlLoader::load_from_str(json).ok()?.into_iter().next()?;
    let mut text = String::new();
    yaml_rust2::YamlEmitter::new(&mut text).dump(&doc).ok()?;
    let yaml = format!("{}\n", text.strip_prefix("---\n").unwrap_or(&text));
    let (_, diags) = super::parse_workflow(".github/workflows/emitted.yml", &yaml, crate::expr::Dialect::Gha);
    let github = !diags.iter().any(|d| d.code.starts_with("KCI5"));
    Some((yaml, github))
}

fn unquote(s: &str) -> String {
    s.get(1..s.len().saturating_sub(1)).unwrap_or_default().to_owned()
}

struct Eraser<'s> {
    src: &'s str,
    blank: Vec<Span<usize>>,
    /// Bytes written over blanked ones: a `;` kept or added, an arrow's `)` moved.
    put: Vec<(usize, u8)>,
    refused: Vec<Diag>,
}

impl Eraser<'_> {
    /// The children of a program or block, each a statement; one that is only types goes, with a
    /// `;` where the statement after it would otherwise continue the one before.
    fn statements(&mut self, parent: &N<'_>) {
        let kids: Vec<N<'_>> = parent.children().collect();
        for (i, s) in kids.iter().enumerate() {
            if s.is_error() {
                continue;
            }
            match self.types_only(s) {
                Some(true) => match self.misread(s) {
                    // A misread the grammar recovered from with a line break: the statement ends
                    // there, and what follows is a statement of its own.
                    Some(Some((end, rest))) => {
                        self.blank.push(s.range().start..end);
                        let next = rest.text().into_owned();
                        self.guard(&kids, i, &next);
                    }
                    // Any other: left as it is, for QuickJS to name, rather than erase code.
                    Some(None) => {}
                    None => {
                        self.blank.push(s.range());
                        let next = kids.iter().skip(i + 1).find(|k| k.kind() != "comment").map(|k| k.text().into_owned()).unwrap_or_default();
                        self.guard(&kids, i, &next);
                    }
                },
                Some(false) => {}
                None => self.walk(s),
            }
        }
    }

    /// `None` for a statement the grammar read cleanly. For one holding an ERROR, `Some` of where it
    /// ends and the part after, when that part starts on a new line: tree-sitter reads a template
    /// on the line after a type alias as the alias's type, where TypeScript ends the alias at the
    /// line break.
    fn misread<'r>(&self, s: &N<'r>) -> Option<Option<(usize, N<'r>)>> {
        if !s.has_error() || !errors_in(s) {
            return None;
        }
        let d = if s.kind() == "export_statement" { s.field("declaration").unwrap_or_else(|| s.clone()) } else { s.clone() };
        let kids: Vec<N<'r>> = d.children().collect();
        let split = kids.iter().position(N::is_error).and_then(|e| {
            let (err, rest) = (kids.get(e)?, kids.get(e + 1)?);
            let gap = self.src.get(err.range().end..rest.range().start)?;
            let last = kids.iter().skip(e + 2).all(|k| k.kind() == ";" || k.text().is_empty());
            (gap.contains('\n') && last && !errors_in(rest) && kids.iter().take(e).all(|k| !errors_in(k))).then(|| (err.range().end, rest.clone()))
        });
        Some(split)
    }

    /// `Some(true)` for a statement that is only types, `Some(false)` for one refused (said in
    /// `refused`), `None` for any other.
    fn types_only(&mut self, s: &N<'_>) -> Option<bool> {
        match s.kind().as_ref() {
            "interface_declaration" | "type_alias_declaration" | "ambient_declaration" | "function_signature" => Some(true),
            "enum_declaration" => Some(self.refuse(s, "an `enum` isn't erasable TypeScript: use a `const` object")),
            "import_alias" => Some(self.refuse(s, "`import … =` isn't erasable TypeScript: use `import … from`")),
            "expression_statement" | "module" | "internal_module" => {
                let m = if s.kind() == "expression_statement" { s.children().find(|c| c.kind() == "internal_module")? } else { s.clone() };
                Some(self.namespace(&m))
            }
            "import_statement" => {
                if s.children().any(|c| matches!(c.kind().as_ref(), "import_require_clause" | "import_alias")) {
                    return Some(self.refuse(s, "`import … =` isn't erasable TypeScript: use `import … from`"));
                }
                s.children().any(|c| c.kind() == "type").then_some(true)
            }
            "export_statement" => {
                if s.children().any(|c| c.kind() == "=") {
                    return Some(self.refuse(s, "`export =` isn't erasable TypeScript: use `export default`"));
                }
                if s.children().any(|c| c.kind() == "type") {
                    return Some(true);
                }
                let d = s.field("declaration")?;
                match d.kind().as_ref() {
                    "interface_declaration" | "type_alias_declaration" | "ambient_declaration" | "function_signature" | "enum_declaration" | "module" | "internal_module" | "import_alias" => {
                        self.types_only(&d)
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// A namespace (or `module M`): erased when it holds only types, refused when it holds code.
    fn namespace(&mut self, m: &N<'_>) -> bool {
        if Self::only_types(m) {
            return true;
        }
        self.refuse(m, "a namespace holding code isn't erasable TypeScript: use a `const` object")
    }

    fn only_types(m: &N<'_>) -> bool {
        let Some(body) = m.field("body") else { return true };
        body.children().filter(|c| c.is_named() && c.kind() != "comment").all(|c| {
            let c = if c.kind() == "export_statement" { c.field("declaration").unwrap_or(c) } else { c };
            match c.kind().as_ref() {
                "interface_declaration" | "type_alias_declaration" => true,
                "module" | "internal_module" => Self::only_types(&c),
                "expression_statement" => c.children().find(|x| x.kind() == "internal_module").is_some_and(|x| Self::only_types(&x)),
                _ => false,
            }
        })
    }

    /// Node's repair for an erased statement `kids[i]`: when the next statement (`next`) starts
    /// with what would continue the one before (`(`, `[`, a template, `+`, `-`, `/`), a `;` goes
    /// between, the erased statement's own when the one before ended with it, else at the erased
    /// one's start.
    fn guard(&mut self, kids: &[N<'_>], i: usize, next: &str) {
        let mut c = next.chars();
        let hazard = match (c.next(), c.next()) {
            (Some('(' | '[' | '`' | '/'), _) => true,
            (Some('+'), n) => n != Some('+'),
            (Some('-'), n) => n != Some('-'),
            _ => false,
        };
        if !hazard {
            return;
        }
        let Some(prev) = kids.get(..i).and_then(|k| k.iter().rev().find(|k| k.kind() != "comment")) else { return };
        let semi = prev.range().end.saturating_sub(1);
        if prev.text().ends_with(';') {
            if self.blanked(semi) {
                self.put.push((semi, b';'));
            }
            return;
        }
        if let Some(s) = kids.get(i) {
            self.put_ascii(s.range().start, b';');
        }
    }

    fn walk(&mut self, n: &N<'_>) {
        match n.kind().as_ref() {
            "ERROR" => {}
            "type_annotation" | "opting_type_annotation" | "omitting_type_annotation" | "adding_type_annotation" | "asserts_annotation" | "type_predicate_annotation" | "type_parameters"
            | "type_arguments" | "implements_clause" => self.blank.push(n.range()),
            "program" | "statement_block" | "switch_case" | "switch_default" => self.statements(n),
            "type_assertion" => {
                self.refuse(n, "`<T>x` isn't erasable TypeScript: write `x as T`");
            }
            "as_expression" | "satisfies_expression" => {
                let kw = if n.kind() == "as_expression" { "as" } else { "satisfies" };
                for c in n.children() {
                    if c.kind() == kw {
                        self.blank.push(c.range().start..n.range().end);
                        break;
                    }
                    self.walk(&c);
                }
            }
            "non_null_expression" => {
                for c in n.children() {
                    if c.kind() == "!" && c.range().end == n.range().end {
                        self.blank.push(c.range());
                    } else {
                        self.walk(&c);
                    }
                }
            }
            "abstract_class_declaration" | "class_declaration" | "class" => {
                for c in n.children() {
                    if c.kind() == "abstract" {
                        self.blank.push(c.range());
                    } else if c.kind() == "class_body" {
                        self.class_body(&c);
                    } else {
                        self.walk(&c);
                    }
                }
            }
            "formal_parameters" => self.parameters(n),
            "arrow_function" => self.arrow(n),
            "variable_declarator" => self.modifiers(n, &["!"]),
            "named_imports" | "export_clause" => self.specifiers(n),
            _ => n.children().for_each(|c| self.walk(&c)),
        }
    }

    /// A class's members: overloads, index signatures and `declare` or `abstract` members go whole,
    /// with their `;`; the others lose their modifiers and types.
    fn class_body(&mut self, body: &N<'_>) {
        let kids: Vec<N<'_>> = body.children().collect();
        for (i, m) in kids.iter().enumerate() {
            let whole = match m.kind().as_ref() {
                "method_signature" | "abstract_method_signature" | "index_signature" => true,
                "public_field_definition" => m.children().any(|c| matches!(c.kind().as_ref(), "declare" | "abstract")),
                _ => false,
            };
            if whole {
                let end = kids.get(i + 1).filter(|s| s.kind() == ";").map_or(m.range().end, |s| s.range().end);
                self.blank.push(m.range().start..end);
            } else if matches!(m.kind().as_ref(), "public_field_definition" | "method_definition") {
                self.modifiers(m, &["readonly", "?", "!"]);
            } else {
                self.walk(m);
            }
        }
    }

    /// Walks `n`, blanking its accessibility and `override` modifiers and the tokens named.
    fn modifiers(&mut self, n: &N<'_>, tokens: &[&str]) {
        for c in n.children() {
            if matches!(c.kind().as_ref(), "accessibility_modifier" | "override_modifier") || tokens.contains(&c.kind().as_ref()) {
                self.blank.push(c.range());
            } else {
                self.walk(&c);
            }
        }
    }

    /// Parameters: a parameter property is refused; `this` goes with its comma; `?` and types go.
    fn parameters(&mut self, ps: &N<'_>) {
        let kids: Vec<N<'_>> = ps.children().collect();
        for (i, p) in kids.iter().enumerate() {
            if !matches!(p.kind().as_ref(), "required_parameter" | "optional_parameter") {
                self.walk(p);
                continue;
            }
            if p.children().any(|c| matches!(c.kind().as_ref(), "accessibility_modifier" | "override_modifier" | "readonly")) {
                self.refuse(p, "a parameter property isn't erasable TypeScript: assign the field in the constructor");
                continue;
            }
            if p.field("pattern").is_some_and(|x| x.kind() == "this") {
                let end = kids.get(i + 1).filter(|s| s.kind() == ",").map_or(p.range().end, |s| s.range().end);
                self.blank.push(p.range().start..end);
                continue;
            }
            self.modifiers(p, &["?"]);
        }
    }

    /// An arrow function: its types go, and when its return type puts a line between `)` and `=>`,
    /// the `)` moves to the type's end, where `=>` may follow it.
    fn arrow(&mut self, n: &N<'_>) {
        if let (Some(ps), Some(ret), Some(arrow)) = (n.field("parameters"), n.field("return_type"), n.children().find(|c| c.kind() == "=>")) {
            let close = ps.range().end.saturating_sub(1);
            let between = self.src.get(close..arrow.range().start).unwrap_or_default();
            if self.src.as_bytes().get(close) == Some(&b')') && between.contains(['\n', '\r', '\u{2028}', '\u{2029}']) {
                self.blank.push(close..close + 1);
                self.put_ascii(ret.range().end.saturating_sub(1), b')');
            }
        }
        n.children().for_each(|c| self.walk(&c));
    }

    /// Import and export specifiers: a `type` one goes with the comma after it.
    fn specifiers(&mut self, list: &N<'_>) {
        let kids: Vec<N<'_>> = list.children().collect();
        for (i, s) in kids.iter().enumerate() {
            if matches!(s.kind().as_ref(), "import_specifier" | "export_specifier") && s.children().any(|c| c.kind() == "type") {
                let end = kids.get(i + 1).filter(|c| c.kind() == ",").map_or(s.range().end, |c| c.range().end);
                self.blank.push(s.range().start..end);
            } else {
                self.walk(s);
            }
        }
    }

    fn refuse(&mut self, n: &N<'_>, why: &str) -> bool {
        let r = n.range();
        self.refused.push(Diag::error("KCI1033", why, Range { start: pos(self.src, r.start), end: pos(self.src, r.end) }));
        false
    }

    fn blanked(&self, at: usize) -> bool {
        self.blank.iter().any(|b| b.contains(&at))
    }

    /// Writes `b` over the character at `at`, when that character is one byte.
    fn put_ascii(&mut self, at: usize, b: u8) {
        if self.src.as_bytes().get(at).is_some_and(u8::is_ascii) {
            self.put.push((at, b));
        }
    }

    fn write(mut self) -> String {
        self.blank.sort_by_key(|b| b.start);
        let mut out = String::with_capacity(self.src.len());
        let mut blanks = self.blank.iter().peekable();
        let mut end = 0;
        for (at, ch) in self.src.char_indices() {
            while blanks.peek().is_some_and(|b| b.start <= at) {
                end = end.max(blanks.next().map_or(0, |b| b.end));
            }
            if at < end {
                match ch {
                    '\n' | '\r' | '\t' | '\u{2028}' | '\u{2029}' => out.push(ch),
                    _ => out.push_str(match ch.len_utf8() {
                        1 => " ",
                        2 => "\u{a0}",
                        3 => "\u{2002}",
                        _ => " \u{feff}",
                    }),
                }
            } else {
                out.push(ch);
            }
        }
        let mut bytes = out.into_bytes();
        for (at, b) in self.put {
            if let Some(x) = bytes.get_mut(at) {
                *x = b;
            }
        }
        String::from_utf8(bytes).unwrap_or_default()
    }
}

/// Whether the grammar couldn't read some of `n` (an ERROR node, not just a missing `;`).
fn errors_in(n: &N<'_>) -> bool {
    n.is_error() || (n.has_error() && n.children().any(|c| errors_in(&c)))
}

/// The 1-based line and column (in characters) of byte `at` of `src`.
pub fn pos(src: &str, at: usize) -> Pos {
    let before = src.get(..at).unwrap_or(src);
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let line = u32::try_from(before.matches('\n').count() + 1).unwrap_or(u32::MAX);
    let col = u32::try_from(before.get(line_start..).map_or(0, |l| l.chars().count()) + 1).unwrap_or(u32::MAX);
    Pos { line, col, offset: u32::try_from(at).unwrap_or(u32::MAX) }
}

#[cfg(test)]
mod tests;
