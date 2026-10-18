//! Argo CD's health checks for custom resources, run as Argo CD runs them (`util/lua`): the
//! library of Lua scripts its resource customizations ship (`resource_customizations`, Apache-2.0,
//! built into kiln), and scripts settings add (Argo CD's
//! `resource.customizations.health.<group>_<kind>`). Each runs as in Argo CD's gopher-lua: Lua
//! 5.1 with 5.2's `goto`, which Lua 5.2 is (numbers are doubles; 5.1's `table.getn` and `unpack`
//! are there too), with the base, table, string and math libraries and Argo CD's safe `os`
//! (`time`, `date`), the object as `obj`, and a second to answer.
//!
//! The script for an object is, in Argo CD's order: a setting for its `group/Kind`, a setting
//! whose key matches it with `*`, the library's script for it, then the first of the library's
//! wildcard directories that matches (`_.crossplane.io/_` is `*.crossplane.io/*`). A script that
//! returns nothing (or an empty table) leaves the object to kiln's built-in rules, as Argo CD
//! leaves it to its own; one whose status isn't a health status reads `Unknown`.

use std::cell::RefCell;
use std::time::{Duration, Instant};

use mlua::serde::SerializeOptions;
use mlua::{Function, HookTriggers, Lua, LuaOptions, LuaSerdeExt, StdLib, Table, Value as LuaValue, VmState};
use rustc_hash::FxHashMap;
use serde_json::Value;

use crate::health::{Rules, Status};
use crate::model::Health;
use crate::obj;

mod library {
    include!(concat!(env!("OUT_DIR"), "/health_scripts.rs"));
}

/// How long a script may run (Argo CD's limit).
const LIMIT: Duration = Duration::from_secs(1);
/// How much memory the scripts may hold.
const MEMORY: usize = 256 << 20;
/// Argo CD's words for a script whose status isn't one.
const INVALID: &str = "Lua returned an invalid health status";

/// Where an object's script is.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Source {
    Setting(usize),
    Library(usize),
}

/// Health by Argo CD's Lua scripts: settings' first, then the library's.
pub struct LuaRules {
    lua: Lua,
    /// What the scripts see as their globals (each call gets a table of its own over it).
    sandbox: Table,
    /// Scripts from settings: the key they're for (`group/Kind`, or with `*`), and the text.
    settings: Vec<(String, String)>,
    /// Each `group/Kind`'s script, once looked up (none: no script).
    found: RefCell<FxHashMap<String, Option<Source>>>,
    /// Scripts compiled, by where they are.
    compiled: RefCell<FxHashMap<Source, Function>>,
}

impl LuaRules {
    /// The library, and settings' scripts by key: `group/Kind` (or `Kind` for the core group),
    /// `*` matching any part.
    pub fn new(settings: Vec<(String, String)>) -> Result<LuaRules, String> {
        let lua = Lua::new_with(StdLib::TABLE | StdLib::STRING | StdLib::MATH | StdLib::OS, LuaOptions::default()).map_err(|e| e.to_string())?;
        lua.set_memory_limit(MEMORY).map_err(|e| e.to_string())?;
        let sandbox = sandbox(&lua).map_err(|e| e.to_string())?;
        Ok(LuaRules { lua, sandbox, settings, found: RefCell::default(), compiled: RefCell::default() })
    }

    /// Argo CD's settings: `resource.customizations.health.<group>_<kind>` keys (the group and
    /// kind joined by the last `_`, the core group's kind alone), and their scripts.
    pub fn from_settings<'a>(settings: impl IntoIterator<Item = (&'a str, &'a str)>) -> Result<LuaRules, String> {
        let scripts = settings
            .into_iter()
            .filter_map(|(k, script)| {
                let gk = k.strip_prefix("resource.customizations.health.")?;
                let key = match gk.rsplit_once('_') {
                    Some((group, kind)) if !group.is_empty() => format!("{group}/{kind}"),
                    Some((_, kind)) => kind.to_owned(),
                    None => gk.to_owned(),
                };
                Some((key, script.to_owned()))
            })
            .collect();
        LuaRules::new(scripts)
    }

    /// The script for `key` (`group/Kind`), in Argo CD's order.
    fn source(&self, key: &str) -> Option<Source> {
        if let Some(s) = self.found.borrow().get(key) {
            return *s;
        }
        let setting = self.settings.iter().position(|(k, _)| k == key).or_else(|| self.settings.iter().position(|(k, _)| k.contains('*') && matches(k, key)));
        let library = || {
            library::SCRIPTS
                .binary_search_by(|(k, _)| (*k).cmp(key))
                .ok()
                .or_else(|| library::SCRIPTS.iter().position(|(k, _)| k.contains('_') && matches(&k.replace('_', "*"), key)))
        };
        let s = setting.map(Source::Setting).or_else(|| library().map(Source::Library));
        self.found.borrow_mut().insert(key.to_owned(), s);
        s
    }

    fn text(&self, s: Source) -> Option<&str> {
        match s {
            Source::Setting(i) => self.settings.get(i).map(|(_, t)| t.as_str()),
            Source::Library(i) => library::SCRIPTS.get(i).map(|(_, t)| *t),
        }
    }

    fn function(&self, s: Source) -> Result<Function, String> {
        if let Some(f) = self.compiled.borrow().get(&s) {
            return Ok(f.clone());
        }
        let text = self.text(s).ok_or("no such script")?;
        let f = self.lua.load(text).set_name("health.lua").into_function().map_err(|e| e.to_string())?;
        self.compiled.borrow_mut().insert(s, f.clone());
        Ok(f)
    }

    /// Runs a script on `o`: its status, `None` when it has none to give.
    fn run(&self, s: Source, o: &Value) -> Result<Option<Status>, String> {
        let f = self.function(s)?;
        // A table of globals of its own, over the sandbox: nothing one run sets reaches the next.
        let env = self.lua.create_table().map_err(|e| e.to_string())?;
        let meta = self.lua.create_table().map_err(|e| e.to_string())?;
        meta.set("__index", self.sandbox.clone()).map_err(|e| e.to_string())?;
        env.set_metatable(Some(meta)).map_err(|e| e.to_string())?;
        env.set("_G", env.clone()).map_err(|e| e.to_string())?;
        let opts = SerializeOptions::new().set_array_metatable(false).serialize_none_to_null(false).serialize_unit_to_null(false);
        env.set("obj", self.lua.to_value_with(o, opts).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        f.set_environment(env).map_err(|e| e.to_string())?;
        let started = Instant::now();
        self.lua
            .set_hook(HookTriggers::new().every_nth_instruction(10_000), move |_, _| {
                if started.elapsed() > LIMIT { Err(mlua::Error::runtime("the health script took longer than a second")) } else { Ok(VmState::Continue) }
            })
            .map_err(|e| e.to_string())?;
        let out = f.call::<LuaValue>(());
        self.lua.remove_hook();
        let out = out.map_err(|e| match e {
            mlua::Error::RuntimeError(m) => m,
            e => e.to_string(),
        })?;
        match out {
            LuaValue::Nil => Ok(None),
            LuaValue::Table(t) => {
                let status: Option<String> = t.get("status").map_err(|e| e.to_string())?;
                let message: Option<String> = t.get("message").map_err(|e| e.to_string())?;
                match (status, message) {
                    // An empty table: no health, as Argo CD reads one.
                    (None, None) if t.is_empty() => Ok(None),
                    (Some(s), m) => Ok(Some(match Health::parse(&s) {
                        Some(health) => Status { health, message: m.unwrap_or_default() },
                        None => Status { health: Health::Unknown, message: INVALID.into() },
                    })),
                    (None, _) => Ok(Some(Status { health: Health::Unknown, message: INVALID.into() })),
                }
            }
            other => Err(format!("expect table output from Lua script, not {}", other.type_name())),
        }
    }
}

impl Rules for LuaRules {
    fn health(&self, o: &Value) -> Option<Result<Status, String>> {
        let s = self.source(&key(o))?;
        match self.run(s, o) {
            Ok(Some(status)) => Some(Ok(status)),
            Ok(None) => None,
            Err(e) => Some(Err(e)),
        }
    }
}

/// Argo CD's key for an object's kind: `group/Kind`, or `Kind` for the core group.
pub fn key(o: &Value) -> String {
    let group = obj::api_group(obj::str_at(o, &["apiVersion"]).unwrap_or(""));
    let kind = obj::kind(o);
    if group.is_empty() { kind.to_owned() } else { format!("{group}/{kind}") }
}

/// Whether `key` matches `pattern`, `*` matching within one part (`group/Kind`'s parts).
fn matches(pattern: &str, key: &str) -> bool {
    let opts = glob::MatchOptions { case_sensitive: true, require_literal_separator: true, require_literal_leading_dot: false };
    glob::Pattern::new(pattern).is_ok_and(|p| p.matches_with(key, opts))
}

/// The globals scripts see: Lua's base functions without the ones that read files or load code,
/// the table, string and math libraries with 5.1's `table.getn` and `unpack`, and Argo CD's safe
/// `os` (`time` and `date`). The libraries are read-only, so no run changes them for the next.
fn sandbox(lua: &Lua) -> mlua::Result<Table> {
    let g = lua.globals();
    let s = lua.create_table()?;
    for name in ["assert", "error", "getmetatable", "ipairs", "next", "pairs", "pcall", "rawequal", "rawget", "rawset", "select", "setmetatable", "tonumber", "tostring", "type", "xpcall", "_VERSION"] {
        s.set(name, g.get::<LuaValue>(name)?)?;
    }
    let table: Table = g.get("table")?;
    let lib = lua.create_table()?;
    for pair in table.pairs::<LuaValue, LuaValue>() {
        let (k, v) = pair?;
        lib.set(k, v)?;
    }
    lib.set("getn", lua.create_function(|_, t: Table| Ok(t.raw_len()))?)?;
    s.set("unpack", table.get::<LuaValue>("unpack")?)?;
    s.set("table", read_only(lua, lib)?)?;
    for name in ["string", "math"] {
        s.set(name, read_only(lua, g.get(name)?)?)?;
    }
    let os = lua.create_table()?;
    let real: Table = g.get("os")?;
    for name in ["time", "date"] {
        os.set(name, real.get::<LuaValue>(name)?)?;
    }
    s.set("os", read_only(lua, os)?)?;
    Ok(s)
}

/// A table that reads as `lib` and can't be written.
fn read_only(lua: &Lua, lib: Table) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    let meta = lua.create_table()?;
    meta.set("__index", lib)?;
    meta.set("__newindex", lua.create_function(|_, ()| -> mlua::Result<()> { Err(mlua::Error::runtime("the libraries are read-only")) })?)?;
    t.set_metatable(Some(meta))?;
    Ok(t)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use serde_json::json;

    use super::*;

    fn rules() -> LuaRules {
        match LuaRules::new(Vec::new()) {
            Ok(r) => r,
            Err(e) => unreachable_rules(e),
        }
    }

    #[allow(clippy::panic)]
    fn unreachable_rules(e: String) -> LuaRules {
        panic!("no Lua: {e}")
    }

    fn status(r: &LuaRules, o: &Value) -> Option<(Health, String)> {
        match r.health(o) {
            Some(Ok(s)) => Some((s.health, s.message)),
            Some(Err(e)) => Some((Health::Unknown, format!("error: {e}"))),
            None => None,
        }
    }

    /// Argo CD's own expectations: every kind's `health_test.yaml` in the library, each object
    /// read as Argo CD reads it, its health and message exactly as Argo CD's.
    #[test]
    fn argo_cds_health_tests_pass() -> Result<(), String> {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resource_customizations");
        let r = rules();
        let mut tests = Vec::new();
        collect(&root, &mut tests);
        let (mut ran, mut failed) = (0, Vec::new());
        for (dir, test) in tests {
            let text = std::fs::read_to_string(&test).map_err(|e| e.to_string())?;
            let doc: Value = serde_yaml::from_str(&text).map_err(|e| format!("{}: {e}", test.display()))?;
            for t in doc.get("tests").and_then(Value::as_array).into_iter().flatten() {
                let Some(input) = t.get("inputPath").and_then(Value::as_str) else { continue };
                let want_status = t.pointer("/healthStatus/status").and_then(Value::as_str).unwrap_or("");
                let want_message = t.pointer("/healthStatus/message").and_then(Value::as_str).unwrap_or("");
                let path = dir.join(input);
                let yaml = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
                // Argo CD reads the first document of a file.
                let o = obj::parse_documents(&yaml).map_err(|e| format!("{}: {e}", path.display()))?.into_iter().next().unwrap_or(Value::Null);
                ran += 1;
                let got = status(&r, &o);
                let (got_status, got_message) = got.as_ref().map(|(h, m)| (h.as_str(), m.as_str())).unwrap_or(("", ""));
                if (got_status, got_message) != (want_status, want_message) {
                    failed.push(format!("{}: want {want_status} {want_message:?}, got {got_status} {got_message:?}", path.strip_prefix(&root).unwrap_or(&path).display()));
                }
            }
        }
        assert!(ran > 900, "ran {ran} of Argo CD's health tests");
        assert!(failed.is_empty(), "{} of {ran} differ from Argo CD:\n{}", failed.len(), failed.join("\n"));
        Ok(())
    }

    fn collect(dir: &Path, out: &mut Vec<(PathBuf, PathBuf)>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                collect(&p, out);
            } else if e.file_name() == "health_test.yaml" {
                out.push((dir.to_path_buf(), p));
            }
        }
    }

    #[test]
    fn settings_come_first_and_scripts_are_contained() {
        let paused = json!({"apiVersion": "argoproj.io/v1alpha1", "kind": "Rollout", "metadata": {"name": "r", "generation": 1},
            "spec": {"paused": true}, "status": {"observedGeneration": "1", "phase": "Paused", "message": "manually paused"}});
        let r = rules();
        assert_eq!(status(&r, &paused), Some((Health::Suspended, "manually paused".into())), "the library's Rollout script");
        // A setting for the kind wins; a wildcard setting covers a group.
        let own = LuaRules::from_settings([
            ("resource.customizations.health.argoproj.io_Rollout", "hs = {}\nhs.status = 'Degraded'\nhs.message = 'mine'\nreturn hs"),
            ("resource.customizations.health.ConfigMap", "return {status = 'Progressing'}"),
        ]);
        let own = own.unwrap_or_else(unreachable_rules);
        assert_eq!(status(&own, &paused), Some((Health::Degraded, "mine".into())));
        assert_eq!(status(&own, &json!({"apiVersion": "v1", "kind": "ConfigMap"})), Some((Health::Progressing, String::new())));
        // Nothing to say leaves it to the built-in rules; a bad status reads Unknown; errors, and
        // scripts that run too long, are errors.
        let odd = LuaRules::new(vec![
            ("a.example/Nothing".into(), "return nil".into()),
            ("a.example/Bad".into(), "return {status = 'Fine'}".into()),
            ("a.example/Broken".into(), "error('no status here')".into()),
            ("a.example/Forever".into(), "while true do end".into()),
            ("a.example/Leaks".into(), "leaked = (leaked or 0) + 1\nreturn {status = 'Healthy', message = tostring(leaked)}".into()),
            ("a.example/Files".into(), "return {status = 'Healthy', message = tostring(io) .. tostring(dofile) .. tostring(os.execute)}".into()),
        ])
        .unwrap_or_else(unreachable_rules);
        let o = |kind: &str| json!({"apiVersion": "a.example/v1", "kind": kind});
        assert_eq!(status(&odd, &o("Nothing")), None);
        assert_eq!(status(&odd, &o("Bad")), Some((Health::Unknown, INVALID.into())));
        assert!(status(&odd, &o("Broken")).is_some_and(|(h, m)| h == Health::Unknown && m.contains("no status here")));
        let t = Instant::now();
        assert!(status(&odd, &o("Forever")).is_some_and(|(h, m)| h == Health::Unknown && m.contains("longer than a second")));
        assert!(t.elapsed() < Duration::from_secs(3));
        assert_eq!(status(&odd, &o("Leaks")), Some((Health::Healthy, "1".into())));
        assert_eq!(status(&odd, &o("Leaks")), Some((Health::Healthy, "1".into())), "a run's globals don't reach the next");
        assert_eq!(status(&odd, &o("Files")), Some((Health::Healthy, "nilnilnil".into())), "no files, no processes");
    }

    #[test]
    fn wildcard_directories_cover_their_groups() {
        let r = rules();
        // A managed resource of a provider's group: no script of its own, Crossplane's for every group.
        let ready = json!({"apiVersion": "database.example.crossplane.io/v1", "kind": "Instance", "metadata": {"name": "db"},
            "status": {"conditions": [{"type": "Synced", "status": "True", "reason": "ReconcileSuccess"}, {"type": "Ready", "status": "True", "reason": "Available"}]}});
        assert!(r.source(&key(&ready)).is_some_and(|s| matches!(s, Source::Library(i) if library::SCRIPTS.get(i).is_some_and(|(k, _)| *k == "_.crossplane.io/_"))), "*.crossplane.io/* covers it");
        assert!(status(&r, &ready).is_some_and(|(h, _)| h == Health::Healthy), "{:?}", status(&r, &ready));
        assert_eq!(r.source("nothing.example/Thing"), None);
    }
}
