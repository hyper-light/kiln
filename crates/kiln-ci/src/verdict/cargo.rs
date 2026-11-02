//! A Cargo workspace's units (docs/ci.md W28-R54; W24-R16's Cargo part), the pure part: the root
//! manifest's `members` and `exclude`, each member's package name and the members it depends on by
//! path (normal, dev and build dependencies, target-specific ones too, `workspace = true` resolved
//! through `[workspace.dependencies]`). Every member reads the workspace's root `Cargo.toml` and
//! `Cargo.lock`. The server lists the tree's files and reads the manifests at a commit; this says
//! what the units are, for [`crate::land::conflict::Units`].

use std::collections::{BTreeMap, BTreeSet};

use crate::land::conflict::Unit;
use crate::trigger::pattern::Patterns;

/// The tables of a manifest that name dependencies.
const KINDS: [&str; 3] = ["dependencies", "dev-dependencies", "build-dependencies"];

/// The units of the Cargo workspace whose root manifest is `root` (its text), in a tree whose
/// files are `files` (paths from the root), each manifest read with `read`. A member whose
/// manifest can't be read or parsed is left out; a root manifest that can't be parsed is an error.
pub fn units(root: &str, files: &[String], read: impl Fn(&str) -> Option<String>) -> Result<Vec<Unit>, String> {
    let doc: toml::Table = root.parse().map_err(|e: toml::de::Error| format!("Cargo.toml: {e}"))?;
    let ws = doc.get("workspace").and_then(toml::Value::as_table);
    let list = |k: &str| -> Vec<String> { ws.and_then(|w| w.get(k)).and_then(toml::Value::as_array).map(|a| a.iter().filter_map(|v| v.as_str()).map(|s| clean(s).to_owned()).collect()).unwrap_or_default() };
    let (members, exclude) = (list("members"), list("exclude"));
    let member = Patterns::new(&members).map_err(|e| format!("Cargo.toml's members: {e:?}"))?;
    let excluded = Patterns::new(&exclude).map_err(|e| format!("Cargo.toml's exclude: {e:?}"))?;
    // The folders holding a manifest that the members take and exclude leaves out; the root's too,
    // when it is a package itself.
    let mut dirs: BTreeSet<String> = files
        .iter()
        .filter_map(|f| clean(f).strip_suffix("Cargo.toml").map(|d| d.trim_end_matches('/').to_owned()))
        .filter(|d| !d.is_empty() && member.matches(d) && !(excluded.matches(d) || exclude.iter().any(|x| d == x || d.starts_with(&format!("{x}/")))))
        .collect();
    if doc.contains_key("package") {
        dirs.insert(String::new());
    }
    let shared: BTreeMap<String, String> = ws
        .and_then(|w| w.get("dependencies"))
        .and_then(toml::Value::as_table)
        .map(|t| t.iter().filter_map(|(k, v)| Some((k.clone(), clean(v.as_table()?.get("path")?.as_str()?).to_owned()))).collect())
        .unwrap_or_default();
    // Each member: its name and the folders of what it depends on by path.
    let mut found: BTreeMap<String, (String, BTreeSet<String>)> = BTreeMap::new();
    for dir in &dirs {
        let manifest = if dir.is_empty() { Some(root.to_owned()) } else { read(&format!("{dir}/Cargo.toml")) };
        let Some(m) = manifest.and_then(|t| t.parse::<toml::Table>().ok()) else { continue };
        let name = m.get("package").and_then(|p| p.get("name")).and_then(toml::Value::as_str).map_or_else(|| dir.rsplit('/').next().unwrap_or(dir).to_owned(), str::to_owned);
        let mut tables: Vec<&toml::Table> = KINDS.iter().filter_map(|k| m.get(*k).and_then(toml::Value::as_table)).collect();
        for t in m.get("target").and_then(toml::Value::as_table).into_iter().flat_map(|t| t.values()).filter_map(toml::Value::as_table) {
            tables.extend(KINDS.iter().filter_map(|k| t.get(*k).and_then(toml::Value::as_table)));
        }
        let mut deps = BTreeSet::new();
        for (key, spec) in tables.iter().flat_map(|t| t.iter()) {
            let Some(spec) = spec.as_table() else { continue };
            if let Some(p) = spec.get("path").and_then(toml::Value::as_str) {
                deps.insert(join(dir, p));
            } else if spec.get("workspace").and_then(toml::Value::as_bool) == Some(true)
                && let Some(p) = shared.get(spec.get("package").and_then(toml::Value::as_str).unwrap_or(key)).or_else(|| shared.get(key))
            {
                deps.insert(p.clone());
            }
        }
        found.insert(dir.clone(), (name, deps));
    }
    let by_dir: BTreeMap<&str, &str> = found.iter().map(|(d, (n, _))| (d.as_str(), n.as_str())).collect();
    let reads: BTreeSet<String> = ["Cargo.toml", "Cargo.lock"].map(str::to_owned).into();
    Ok(found
        .iter()
        .map(|(dir, (name, deps))| Unit {
            name: name.clone(),
            ecosystem: "cargo".into(),
            dir: dir.clone(),
            reads: reads.clone(),
            deps: deps.iter().filter_map(|d| by_dir.get(d.as_str())).filter(|n| **n != name.as_str()).map(|n| (*n).to_owned()).collect(),
        })
        .collect())
}

/// A path as units keep it: from the root, without `./` or a slash at either end.
fn clean(path: &str) -> &str {
    path.trim_start_matches("./").trim_matches('/')
}

/// `rel` from the folder `dir`, with `.` and `..` taken out.
fn join(dir: &str, rel: &str) -> String {
    let mut parts: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    for p in rel.split('/') {
        match p {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::land::conflict::Units;

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    /// W28-R54: kiln's own workspace, as its manifests stand: one unit per crate, each with the
    /// crates it depends on by path; vendored crates, excluded, aren't units.
    #[test]
    fn w28_r54_kilns_own_workspace() -> Result<(), String> {
        let manifests: [(&str, &str); 6] = [
            ("crates/kiln/Cargo.toml", include_str!("../../../kiln/Cargo.toml")),
            ("crates/kiln-cd/Cargo.toml", include_str!("../../../kiln-cd/Cargo.toml")),
            ("crates/kiln-ci/Cargo.toml", include_str!("../../Cargo.toml")),
            ("crates/kiln-http/Cargo.toml", include_str!("../../../kiln-http/Cargo.toml")),
            ("crates/kiln-seams/Cargo.toml", include_str!("../../../kiln-seams/Cargo.toml")),
            ("crates/kiln-sim/Cargo.toml", include_str!("../../../kiln-sim/Cargo.toml")),
        ];
        let mut files: Vec<String> = manifests.iter().map(|(p, _)| (*p).to_owned()).collect();
        files.extend(["Cargo.toml", "Cargo.lock", "vendor/turso/core/Cargo.toml", "docs/ci.md"].map(str::to_owned));
        let read = |p: &str| manifests.iter().find(|(q, _)| *q == p).map(|(_, t)| (*t).to_owned());
        let units = units(include_str!("../../../../Cargo.toml"), &files, read)?;
        let names: Vec<&str> = units.iter().map(|u| u.name.as_str()).collect();
        assert_eq!(names, ["kiln", "kiln-cd", "kiln-ci", "kiln-http", "kiln-seams", "kiln-sim"]);
        let unit = |n: &str| units.iter().find(|u| u.name == n).ok_or(format!("no unit {n}"));
        assert!(unit("kiln")?.deps.is_superset(&set(&["kiln-cd", "kiln-ci", "kiln-http", "kiln-seams"])), "{:?}", unit("kiln")?.deps);
        assert_eq!(unit("kiln-sim")?.deps, set(&["kiln-seams"]));
        assert_eq!(unit("kiln-ci")?.deps, set(&[]));
        assert!(units.iter().all(|u| u.dir == format!("crates/{}", u.name) && u.reads == set(&["Cargo.toml", "Cargo.lock"])));
        // A change to kiln-ci affects kiln-ci and every crate that depends on it, and no other.
        let graph = Units::new(units.clone());
        assert_eq!(graph.affected(["crates/kiln-ci/src/verdict/mod.rs"]), set(&["kiln", "kiln-ci"]));
        assert_eq!(graph.affected(["Cargo.lock"]).len(), 6);
        assert_eq!(graph.affected(["docs/ci.md"]), set(&[]));
        Ok(())
    }

    /// W28-R54: globs and exclusions, `workspace = true`, target tables, renames, and a root that
    /// is a package itself.
    #[test]
    fn w28_r54_members_paths_and_workspace_dependencies() -> Result<(), String> {
        let root = "[package]\nname = \"top\"\n[workspace]\nmembers = [\"libs/*\", \"app\"]\nexclude = [\"libs/old\"]\n[workspace.dependencies]\ncore = { path = \"libs/core\" }\n[dependencies]\napp = { path = \"app\" }\n";
        let app = "[package]\nname = \"the-app\"\n[dependencies]\nc = { workspace = true, package = \"core\" }\n[target.'cfg(unix)'.dev-dependencies]\nutil = { path = \"../libs/util\" }\n";
        let files = ["Cargo.toml", "app/Cargo.toml", "libs/core/Cargo.toml", "libs/util/Cargo.toml", "libs/old/Cargo.toml"].map(str::to_owned);
        let read = |p: &str| match p {
            "app/Cargo.toml" => Some(app.to_owned()),
            "libs/core/Cargo.toml" => Some("[package]\nname = \"core\"\n".to_owned()),
            "libs/util/Cargo.toml" => Some("[package]\nname = \"util\"\n[build-dependencies]\ncore = { path = \"../core\" }\n".to_owned()),
            _ => None,
        };
        let found = units(root, &files, read)?;
        let deps: BTreeMap<&str, (&str, &BTreeSet<String>)> = found.iter().map(|u| (u.name.as_str(), (u.dir.as_str(), &u.deps))).collect();
        assert_eq!(deps.keys().copied().collect::<Vec<_>>(), ["core", "the-app", "top", "util"]);
        assert_eq!(deps.get("top"), Some(&("", &set(&["the-app"]))));
        assert_eq!(deps.get("the-app"), Some(&("app", &set(&["core", "util"]))));
        assert_eq!(deps.get("util"), Some(&("libs/util", &set(&["core"]))));
        assert!(units("[workspace", &files, read).is_err());
        Ok(())
    }
}
