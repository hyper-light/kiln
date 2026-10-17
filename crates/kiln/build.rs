//! Records the target triple the binary is built for (`KILN_TARGET`), so the server knows which
//! agents it can hand its own binary to (`GET /dl/kiln/<target>`) and which must fetch a release
//! build instead; and lists the store's migrations (`src/store/migrations/*.sql`, docs/ci.md
//! §2.4) so that adding one is adding a file, never editing a shared list. Nothing else happens
//! here: the UI is built separately (`ui/dist`, embedded).

use std::fmt::Write as _;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let target = std::env::var("TARGET").unwrap_or_default();
    println!("cargo:rustc-env=KILN_TARGET={target}");

    let dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default()).join("src/store/migrations");
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.ends_with(".sql"))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    let mut out = String::from("/// Every migration, in the order they apply: (name, SQL).\npub const MIGRATIONS: &[(&str, &str)] = &[\n");
    for n in &names {
        let path = dir.join(n);
        println!("cargo:rerun-if-changed={}", path.display());
        let stem = n.trim_end_matches(".sql");
        let _ = writeln!(out, "    ({stem:?}, include_str!({:?})),", path.display().to_string());
    }
    out.push_str("];\n");
    let dest = PathBuf::from(std::env::var("OUT_DIR").unwrap_or_default()).join("migrations.rs");
    if let Err(e) = std::fs::write(&dest, out) {
        println!("cargo:warning=could not write {}: {e}", dest.display());
    }
}
