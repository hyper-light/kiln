//! Lists Argo CD's health scripts (`resource_customizations/<group>/<Kind>/health.lua`) for the
//! engine to ship: a table of their keys (the directory: `group/Kind`, or a wildcard directory
//! such as `_.crossplane.io/_`) and texts, sorted by key.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    let root = manifest.join("resource_customizations");
    println!("cargo:rerun-if-changed={}", root.display());
    let mut found = Vec::new();
    walk(&root, &root, &mut found);
    found.sort();
    let mut out = String::from("/// Argo CD's health scripts: (`group/Kind`, or a wildcard directory; its text), by key.\npub static SCRIPTS: &[(&str, &str)] = &[\n");
    for (key, path) in found {
        let _ = writeln!(out, "    ({key:?}, include_str!({:?})),", path.display().to_string());
    }
    out.push_str("];\n");
    let dest = PathBuf::from(std::env::var("OUT_DIR").unwrap_or_default()).join("health_scripts.rs");
    if let Err(e) = std::fs::write(&dest, out) {
        println!("cargo:warning=writing {}: {e}", dest.display());
    }
}

fn walk(root: &Path, dir: &Path, found: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let path = e.path();
        if path.is_dir() {
            walk(root, &path, found);
        } else if e.file_name() == "health.lua"
            && let Ok(rel) = dir.strip_prefix(root)
        {
            let key = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>().join("/");
            found.push((key, path));
        }
    }
}
