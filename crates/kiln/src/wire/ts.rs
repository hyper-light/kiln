//! The UI's wire types as TypeScript, made from the Rust types its sockets carry (docs/wire.md,
//! "The UI's sockets"): what [`roots`] names and everything they hold, one file each in
//! `ui/src/api/wire/`, checked in. [`bindings_are_current`] renders them and compares them with
//! the files, and `KILN_WRITE_TS=1 cargo test -p kiln wire::ts` writes them. The types derive ts-rs'
//! `TS` in test builds alone (`cfg_attr(test, ...)`; kiln-ci's and kiln-cd's under their `ts` feature,
//! which only kiln's dev-dependencies turn on): the release binary doesn't carry it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ts_rs::{Config, TS, TypeVisitor};

/// What each file starts with: how to make it again.
pub const HEADER: &str = "// Made from kiln's Rust types: change those, then run `KILN_WRITE_TS=1 cargo test -p kiln wire::ts`.\n";

/// The file that names every type, for `import type { .. } from "./wire"`.
const INDEX: &str = "index.ts";

/// Where the files go.
pub fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ui/src/api/wire")
}

/// kiln's integers are JSON numbers (none it sends passes 2^53), not BigInts.
fn config() -> Config {
    Config::new().with_large_int("number")
}

/// Every type's file, by its name in the folder: its text, header first.
struct Collect<'a> {
    cfg: &'a Config,
    files: BTreeMap<PathBuf, String>,
    /// Two types that would be one file, or a type that can't be rendered.
    wrong: Vec<String>,
}

impl TypeVisitor for Collect<'_> {
    fn visit<T: TS + 'static + ?Sized>(&mut self) {
        // A wrapper (`Vec`, `Option`, `Box` ...) has no file: what it holds comes as its own visit.
        let Some(path) = T::output_path() else { return };
        let text = match T::export_to_string(self.cfg) {
            Ok(t) => format!("{HEADER}{t}"),
            Err(e) => {
                self.wrong.push(format!("{}: {e}", std::any::type_name::<T>()));
                return;
            }
        };
        match self.files.get(&path) {
            Some(had) if *had != text => self.wrong.push(format!("two types are {}: rename one with `ts(rename = ..)` ({})", path.display(), std::any::type_name::<T>())),
            Some(_) => {}
            None => {
                self.files.insert(path, text);
                T::visit_dependencies(self);
            }
        }
    }
}

/// The types the UI's sockets carry (everything they hold comes with them).
fn roots(c: &mut Collect<'_>) {
    c.visit::<crate::harness::model::Command>();
    c.visit::<crate::harness::core::Update>();
    c.visit::<crate::hub::AgentLive>();
    c.visit::<crate::proto::NetDrop>();
    c.visit::<super::ui::SessionDown>();
    c.visit::<super::ui::SessionUp>();
    c.visit::<super::ui::TerminalDown>();
    c.visit::<super::ui::TerminalUp>();
    c.visit::<super::ui::LogDown>();
    c.visit::<super::ui::StreamDown>();
    c.visit::<super::ui::StreamUp>();
    c.visit::<super::ui::CdLogDown>();
    c.visit::<super::events::ServerEvent>();
}

/// Every file as it should be, by its name in [`dir`], the index among them.
pub fn render() -> anyhow::Result<BTreeMap<PathBuf, String>> {
    let cfg = config();
    let mut c = Collect { cfg: &cfg, files: BTreeMap::new(), wrong: Vec::new() };
    roots(&mut c);
    anyhow::ensure!(c.wrong.is_empty(), "{}", c.wrong.join("\n"));
    let mut index = String::from(HEADER);
    for path in c.files.keys() {
        let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
        // An import path, `/` between its parts on every system.
        let from: Vec<String> = path.with_extension("").components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
        index.push_str(&format!("export type {{ {name} }} from \"./{}\";\n", from.join("/")));
    }
    c.files.insert(PathBuf::from(INDEX), index);
    Ok(c.files)
}

/// Where `had` first differs from `want`: its line, and the two lines.
fn first_difference(had: &str, want: &str) -> String {
    let (mut h, mut w) = (had.lines(), want.lines());
    for n in 1.. {
        match (h.next(), w.next()) {
            (Some(a), Some(b)) if a == b => {}
            (a, b) => return format!("line {n}: the file has {:?}, the Rust says {:?}", a.unwrap_or("(its end)"), b.unwrap_or("(its end)")),
        }
    }
    String::new()
}

/// The `.ts` files under `dir`, by their names in it.
fn on_disk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let path = e.path();
            if path.is_dir() {
                todo.push(path);
            } else if path.extension().is_some_and(|x| x == "ts")
                && let Ok(rel) = path.strip_prefix(dir)
            {
                out.push(rel.to_path_buf());
            }
        }
    }
    out
}

/// What's wrong with the files in `dir` against `want`: one line each, the file and its first
/// difference; a file of the folder no type makes among them.
pub fn stale(dir: &Path, want: &BTreeMap<PathBuf, String>) -> Vec<String> {
    let mut out = Vec::new();
    for (path, text) in want {
        match std::fs::read_to_string(dir.join(path)) {
            Ok(had) if had == *text => {}
            Ok(had) => out.push(format!("ui/src/api/wire/{}: {}", path.display(), first_difference(&had, text))),
            Err(_) => out.push(format!("ui/src/api/wire/{}: missing", path.display())),
        }
    }
    for name in on_disk(dir) {
        if !want.contains_key(&name) {
            out.push(format!("ui/src/api/wire/{}: no Rust type makes it", name.display()));
        }
    }
    out
}

/// Writes `want` into `dir`, and takes out the files no type makes.
fn write(dir: &Path, want: &BTreeMap<PathBuf, String>) -> anyhow::Result<()> {
    for name in on_disk(dir) {
        if !want.contains_key(&name) {
            std::fs::remove_file(dir.join(name))?;
        }
    }
    for (path, text) in want {
        let path = dir.join(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, text)?;
    }
    Ok(())
}

#[test]
fn bindings_are_current() -> anyhow::Result<()> {
    let want = render()?;
    if std::env::var("KILN_WRITE_TS").is_ok_and(|v| v == "1") {
        return write(&dir(), &want);
    }
    let wrong = stale(&dir(), &want);
    anyhow::ensure!(wrong.is_empty(), "the UI's wire types aren't what the Rust says; `KILN_WRITE_TS=1 cargo test -p kiln wire::ts` writes them:\n{}", wrong.join("\n"));
    Ok(())
}

/// Where under `ui/src` (outside the generated folder) a type of `names` is defined by hand: `type
/// Name =`, `interface Name`, `enum Name`, exported or not, with or without type parameters. A
/// re-export (`export type { Name } from ..`) defines nothing.
pub fn hand_written(ui_src: &Path, generated: &Path, names: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let (ui_src, generated) = (ui_src.canonicalize().unwrap_or_else(|_| ui_src.to_path_buf()), generated.canonicalize().unwrap_or_else(|_| generated.to_path_buf()));
    let mut todo = vec![ui_src.clone()];
    while let Some(d) = todo.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let path = e.path();
            if path.is_dir() {
                if path != generated && path.file_name().is_none_or(|n| n != "node_modules") {
                    todo.push(path);
                }
                continue;
            }
            if !path.extension().is_some_and(|x| x == "ts" || x == "tsx") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            for (n, line) in text.lines().enumerate() {
                let l = line.trim_start();
                let l = l.strip_prefix("export ").unwrap_or(l);
                let l = l.strip_prefix("declare ").unwrap_or(l);
                let Some(rest) = ["type ", "interface ", "enum "].iter().find_map(|k| l.strip_prefix(k)) else { continue };
                let name: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$').collect();
                // A definition goes on (`=`, `<`, `{`, `extends`); `type X,` in an import list doesn't.
                let after = rest.get(name.len()..).unwrap_or_default().trim_start();
                let defines = after.starts_with('=') || after.starts_with('<') || after.starts_with('{') || after.starts_with("extends");
                if defines && names.contains(&name) {
                    let rel = path.strip_prefix(&ui_src).unwrap_or(&path);
                    out.push(format!("ui/src/{}:{}: {name}", rel.display(), n + 1));
                }
            }
        }
    }
    out.sort();
    out
}

#[test]
fn no_generated_type_is_written_by_hand() -> anyhow::Result<()> {
    let names: Vec<String> = render()?.keys().filter_map(|p| p.file_stem()?.to_str().map(str::to_owned)).filter(|n| n != "index").collect();
    let ui_src = dir().join("../..");
    let found = hand_written(&ui_src, &dir(), &names);
    anyhow::ensure!(found.is_empty(), "{} hand-written definitions of types made from Rust (import them from ui/src/api/wire instead):\n{}", found.len(), found.join("\n"));
    Ok(())
}

#[test]
fn hand_written_finds_definitions_not_re_exports() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let generated = root.path().join("api/wire");
    std::fs::create_dir_all(&generated)?;
    std::fs::write(generated.join("Command.ts"), "export type Command = { type: \"interrupt\" };\n")?;
    std::fs::write(root.path().join("a.ts"), "export type { Command } from \"./api/wire\";\nexport interface Update { t: string }\n  type Command<T> = T;\nimport {\n  type Update,\n  type Command,\n} from \"./api/wire\";\n")?;
    let names = vec!["Command".to_owned(), "Update".to_owned()];
    anyhow::ensure!(hand_written(root.path(), &generated, &names) == ["ui/src/a.ts:2: Update", "ui/src/a.ts:3: Command"], "{:?}", hand_written(root.path(), &generated, &names));
    Ok(())
}

// The type as it was when its file was written (its doc is After's, so only the field differs).
/// A fixture.
#[derive(serde::Serialize, ts_rs::TS)]
#[ts(rename = "Planted")]
struct Before {
    a: u32,
}

// The same type with a field added in Rust since.
/// A fixture.
#[derive(serde::Serialize, ts_rs::TS)]
#[ts(rename = "Planted")]
struct After {
    a: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    b: Option<String>,
}

#[test]
fn a_field_added_in_rust_without_its_binding_names_the_file() -> anyhow::Result<()> {
    let files = |visit: fn(&mut Collect<'_>)| {
        let cfg = config();
        let mut c = Collect { cfg: &cfg, files: BTreeMap::new(), wrong: Vec::new() };
        visit(&mut c);
        c.files
    };
    let dir = tempfile::tempdir()?;
    write(dir.path(), &files(|c| c.visit::<Before>()))?;
    let after = files(|c| c.visit::<After>());
    let wrong = stale(dir.path(), &after);
    let named = |w: &String| w.starts_with("ui/src/api/wire/Planted.ts: line ") && w.contains("b?: string");
    anyhow::ensure!(wrong.len() == 1 && wrong.first().is_some_and(named), "{wrong:?}");
    write(dir.path(), &after)?;
    anyhow::ensure!(stale(dir.path(), &after).is_empty());
    Ok(())
}

#[test]
fn tags_are_serdes_and_what_kiln_leaves_out_is_optional() -> anyhow::Result<()> {
    let files = render()?;
    let file = |n: &str| files.get(Path::new(&format!("{n}.ts"))).cloned().unwrap_or_default();
    let (command, update, event) = (file("Command"), file("Update"), file("Event"));
    // Its declaration alone, on one line: no comments, one space between words.
    let flat = |t: &str| {
        let mut code = String::new();
        let mut rest = t;
        while let Some((before, after)) = rest.split_once("/*") {
            code.push_str(before);
            rest = after.split_once("*/").map_or("", |(_, r)| r);
        }
        code.push_str(rest);
        code.lines().filter(|l| !l.trim_start().starts_with("//")).flat_map(str::split_whitespace).collect::<Vec<_>>().join(" ")
    };
    let (command, update, event) = (flat(&command), flat(&update), flat(&event));
    for (name, text, has) in [
        // `type` tags commands; what's left out when empty may be left out.
        ("Command", &command, r#"{ "type": "send", "#),
        ("Command", &command, "attachments?: Array<Attachment>"),
        ("Command", &command, "reply_to?: string,"),
        ("Command", &command, r#"{ "type": "interrupt" }"#),
        // `t` tags updates.
        ("Update", &update, r#"{ "t": "open", info: AgentInfo, since?: number, }"#),
        ("Update", &update, r#"{ "t": "reset", from?: number, }"#),
        // `kind` tags an event's body, flattened beside its id.
        ("Event", &event, r#"{ "kind": "user", text: string, images?: number,"#),
        ("Event", &event, "parent?: string,"),
        ("Event", &event, r#"{ "kind": "tool" } & ToolCall"#),
    ] {
        anyhow::ensure!(text.contains(has), "{name}.ts has no `{has}`:\n{text}");
    }
    // A field always sent isn't optional.
    anyhow::ensure!(event.contains("id: string, seq: number, at: number,"), "{event}");
    Ok(())
}

/// A session's viewers are sent its runner's [`Update`](crate::harness::core::Update)s as they are
/// (`harness::registry`): each of its tags is one of the session socket's.
#[test]
fn every_update_goes_down_as_a_session_frame() -> anyhow::Result<()> {
    let files = render()?;
    let file = |n: &str| files.get(Path::new(&format!("{n}.ts"))).cloned().unwrap_or_default();
    let (update, down) = (file("Update"), file("SessionDown"));
    let tags: Vec<&str> = update.split("\"t\": \"").skip(1).filter_map(|t| t.split('"').next()).collect();
    anyhow::ensure!(tags.len() >= 10, "Update's tags: {tags:?}");
    for t in tags {
        anyhow::ensure!(down.contains(&format!("\"t\": \"{t}\"")), "`{t}`, an Update's, isn't a frame of the session socket's (SessionDown)");
    }
    Ok(())
}
