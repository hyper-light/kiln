//! Records the target triple the binary is built for (`KILN_TARGET`), so the server knows which
//! agents it can hand its own binary to (`GET /dl/kiln/<target>`) and which must fetch a release
//! build instead; the commit it is built from (`KILN_COMMIT`), so every other platform's binary
//! can be built from the same one (`crate::updates::in_step_loop`); and lists the store's migrations (`src/store/migrations/*.sql`, docs/ci.md
//! §2.4) so that adding one is adding a file, never editing a shared list; and embeds the UI server
//! kiln carries (Bun and the UI server's files, packed by `npm run build` into `ui/dist/embed/`:
//! vendor/bun/README.md). Nothing is built here: the UI is built separately (`ui/dist/client`,
//! embedded by rust-embed). On Windows it gives the binary's main thread the stack Linux and macOS
//! give it.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// What of the UI's files is worth packing, and how it's packed: the server's own rule (`src/ui_pack.rs`, the
/// crate's `ui_pack`), for the table of files packed here (goal 11, plan 01a1a87b2e56 step 5).
#[path = "src/ui_pack.rs"]
#[allow(dead_code)]
mod ui_pack;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let target = std::env::var("TARGET").unwrap_or_default();
    println!("cargo:rustc-env=KILN_TARGET={target}");

    // The main thread's stack: 8 MiB, as Linux and macOS give it (`ulimit -s`), where kiln is built
    // and tested. Windows gives 1 MiB (MSVC's linker; MinGW's 2), and a debug build's server
    // overflowed it as it started: the future it runs is made on the main thread before the
    // runtime takes it.
    if std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "windows") {
        let stack = 8 << 20;
        match std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default().as_str() {
            "msvc" => println!("cargo:rustc-link-arg-bins=/STACK:{stack}"),
            _ => println!("cargo:rustc-link-arg-bins=-Wl,--stack,{stack}"),
        }
    }

    // From the environment first: the Dockerfile passes it in (its build context has no .git).
    println!("cargo:rerun-if-env-changed=KILN_COMMIT");
    let commit = std::env::var("KILN_COMMIT").ok().filter(|c| is_commit(c)).or_else(git_commit).unwrap_or_default();
    println!("cargo:rustc-env=KILN_COMMIT={}", commit.to_ascii_lowercase());

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

    ui_server();
    ui_brotli();
}

/// The UI's files worth packing (`ui/dist/client`, which rust-embed embeds), packed here with brotli at its best,
/// into `OUT_DIR/ui-brotli/<the file's SHA-256>.br`, and the table of them that the server serves from
/// (`OUT_DIR/ui_brotli.rs`, `crate::ui`'s `PACKED`): no server compresses them as it starts, nor keeps the
/// copies on its heap (goal 11, plan 01a1a87b2e56 step 5). A file an earlier build packed is taken as it was,
/// so a new UI build packs only what changed, and a packed file no file has now goes; the packing is spread over
/// the jobs cargo gives this script (`NUM_JOBS`). Without the UI built, the table is empty and the server packs
/// as it did.
fn ui_brotli() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    let root = manifest.join("../../ui/dist/client");
    // Watched only where it is, as the UI server's packs are.
    let watch = if root.is_dir() { Some(root.clone()) } else { root.parent().filter(|p| p.is_dir()).map(PathBuf::from) };
    if let Some(w) = watch {
        println!("cargo:rerun-if-changed={}", w.display());
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap_or_default());
    let dir = out.join("ui-brotli");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        println!("cargo:warning=could not make {}: {e}", dir.display());
    }
    let mut files = Vec::new();
    walk(&root, &root, &mut files);
    files.sort();
    let jobs = std::env::var("NUM_JOBS").ok().and_then(|n| n.parse::<usize>().ok()).unwrap_or(1).clamp(1, files.len().max(1));
    let next = std::sync::atomic::AtomicUsize::new(0);
    let entries: Vec<(String, [u8; 32], String)> = std::thread::scope(|s| {
        let workers: Vec<_> = (0..jobs)
            .map(|_| {
                s.spawn(|| {
                    let mut mine = Vec::new();
                    while let Some((name, path)) = files.get(next.fetch_add(1, std::sync::atomic::Ordering::Relaxed)) {
                        mine.extend(pack(name, path, &dir));
                    }
                    mine
                })
            })
            .collect();
        workers.into_iter().flat_map(|w| w.join().unwrap_or_default()).collect()
    });
    // What no file has now: an earlier UI build's.
    let kept: std::collections::HashSet<&str> = entries.iter().map(|(_, _, p)| p.as_str()).collect();
    for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        if !kept.contains(e.path().display().to_string().as_str()) {
            let _ = std::fs::remove_file(e.path());
        }
    }
    let dest = out.join("ui_brotli.rs");
    if let Err(e) = std::fs::write(&dest, ui_pack::table(&entries)) {
        println!("cargo:warning=could not write {}: {e}", dest.display());
    }
}

/// Every file under `dir`, by its name under `root` as rust-embed names it (its parts joined with `/`).
fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = e.path();
        match e.file_type() {
            Ok(t) if t.is_dir() => walk(root, &path, out),
            Ok(t) if t.is_file() => {
                if let Ok(rel) = path.strip_prefix(root) {
                    let name: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
                    out.push((name.join("/"), path));
                }
            }
            _ => {}
        }
    }
}

/// `name`'s file at `path`, packed into `dir` if it's worth it and comes out smaller (as the server keeps a copy
/// only then), or as an earlier build packed it: its table entry.
fn pack(name: &str, path: &Path, dir: &Path) -> Option<(String, [u8; 32], String)> {
    let data = std::fs::read(path).ok()?;
    if !ui_pack::worth(&ui_pack::mime_of(name), data.len()) {
        return None;
    }
    let sha = ui_pack::sha256(&data);
    let mut hex = String::with_capacity(64);
    for b in sha {
        let _ = write!(hex, "{b:02x}");
    }
    let br = dir.join(format!("{hex}.br"));
    if !br.is_file() {
        let packed = ui_pack::brotli(&data).filter(|p| p.len() < data.len())?;
        // Written whole or not at all: a build stopped halfway leaves no short file for the next to take.
        let part = dir.join(format!("{hex}.part"));
        std::fs::write(&part, &packed).and_then(|()| std::fs::rename(&part, &br)).ok()?;
    }
    Some((name.to_owned(), sha, br.display().to_string()))
}

/// The UI server this kiln carries: the packs for the platform it's built for, from
/// `ui/dist/embed/` (or `KILN_UI_EMBED_DIR`), embedded under `cfg(kiln_ui_server)` with their paths
/// in `KILN_UI_RUNTIME` and `KILN_UI_APP` (`crate::uiserver::carried`). Without both, kiln builds
/// without one, and a release build says so; `KILN_UI_SERVER=off` builds without one quietly.
fn ui_server() {
    println!("cargo:rustc-check-cfg=cfg(kiln_ui_server)");
    println!("cargo:rerun-if-env-changed=KILN_UI_SERVER");
    println!("cargo:rerun-if-env-changed=KILN_UI_EMBED_DIR");
    if std::env::var("KILN_UI_SERVER").is_ok_and(|v| v.eq_ignore_ascii_case("off")) {
        return;
    }
    let os = match std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default().as_str() {
        "linux" => "linux",
        "macos" => "macos",
        "windows" => "windows",
        _ => return,
    };
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    let dir = std::env::var_os("KILN_UI_EMBED_DIR").map_or_else(|| manifest.join("../../ui/dist/embed"), PathBuf::from);
    let runtime = dir.join(format!("runtime-{os}-{arch}.br"));
    let app = dir.join("app.br");
    // Watched only where they are: a path that isn't there would run this script on every build.
    // The folder (or, before the first pack, ui/dist) is watched for packs that come.
    let watch = if dir.is_dir() { Some(dir.clone()) } else { dir.parent().filter(|p| p.is_dir()).map(PathBuf::from) };
    if let Some(w) = watch {
        println!("cargo:rerun-if-changed={}", w.display());
    }
    if runtime.is_file() && app.is_file() {
        println!("cargo:rerun-if-changed={}", runtime.display());
        println!("cargo:rerun-if-changed={}", app.display());
        println!("cargo:rustc-cfg=kiln_ui_server");
        println!("cargo:rustc-env=KILN_UI_RUNTIME={}", runtime.display());
        println!("cargo:rustc-env=KILN_UI_APP={}", app.display());
    } else if std::env::var("PROFILE").is_ok_and(|p| p == "release") {
        println!(
            "cargo:warning=kiln is built without its UI server: {} and app.br aren't in {} (npm run build in ui/ packs them; KILN_UI_SERVER=off builds without one quietly)",
            runtime.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            dir.display()
        );
    }
}

/// A full commit id.
fn is_commit(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// HEAD's commit, when nothing tracked is changed (a tree with changes is no commit's build); and
/// this script runs again when HEAD, or the branch it is on, moves.
fn git_commit() -> Option<String> {
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git").args(args).output().ok().filter(|o| o.status.success())?;
        Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    };
    let mut watch = vec!["HEAD".to_owned(), "packed-refs".to_owned()];
    watch.extend(git(&["symbolic-ref", "-q", "HEAD"]));
    for w in &watch {
        if let Some(path) = git(&["rev-parse", "--git-path", w]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    let commit = git(&["rev-parse", "HEAD"]).filter(|c| is_commit(c))?;
    let changed = git(&["status", "--porcelain", "--untracked-files=no"]).is_none_or(|s| !s.is_empty());
    (!changed).then_some(commit)
}
