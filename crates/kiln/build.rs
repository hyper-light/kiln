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

fn main() -> Result<(), String> {
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
    ui_packed();
    shards_carried()
}

/// The Shards this kiln carries (`crate::shards`, docs/shards.md): its binary and its VM and network processes for
/// the platform kiln is built for, embedded with their paths in `KILN_SHARDS_BIN`, `KILN_SHARDS_VM` and
/// `KILN_SHARDS_NET`. kiln is one binary with its microVM engine in it, so every build carries them: made here from
/// vendor/shards by ci/shards/pack.sh (into `OUT_DIR`, its cargo target dir beside kiln's, so a build where
/// vendor/shards hasn't changed takes its last), or, for a target whose Shards is built only on its own platform
/// (Windows' MSVC build, which a Linux release cross-builds kiln beside), from the folder `KILN_SHARDS_DIR` names. A
/// build that can't carry them fails, saying why: there's no kiln without Shards.
fn shards_carried() -> Result<(), String> {
    println!("cargo:rerun-if-env-changed=KILN_SHARDS_DIR");
    println!("cargo:rerun-if-env-changed=KILN_SHARDS");
    println!("cargo:rustc-check-cfg=cfg(kiln_lint)");
    let target = std::env::var("TARGET").unwrap_or_default();
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    let root = manifest.join("../..");
    let exe = if std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "windows") { ".exe" } else { "" };
    // A lint of a target whose Shards isn't built here (kiln's gate's clippy through zig for macOS and Windows, from
    // Linux): asked by name, it checks kiln's code against empty stand-ins, and the binary it makes refuses to run
    // (src/entry.rs's `main`, `cfg(kiln_lint)`), so no lint build is ever a kiln.
    if std::env::var("KILN_SHARDS").is_ok_and(|v| v == "lint") {
        let out = PathBuf::from(std::env::var("OUT_DIR").unwrap_or_default()).join("shards-lint");
        std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
        for (var, name) in [("KILN_SHARDS_BIN", "shards"), ("KILN_SHARDS_VM", "shards-vm"), ("KILN_SHARDS_NET", "shards-net")] {
            let f = out.join(name);
            std::fs::write(&f, []).map_err(|e| format!("{}: {e}", f.display()))?;
            println!("cargo:rustc-env={var}={}", f.display());
        }
        println!("cargo:rustc-cfg=kiln_lint");
        return Ok(());
    }
    let dir = match std::env::var_os("KILN_SHARDS_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => shards_packed(&root, &target)?,
    };
    let files: Vec<(&str, PathBuf)> = [("KILN_SHARDS_BIN", "shards"), ("KILN_SHARDS_VM", "shards-vm"), ("KILN_SHARDS_NET", "shards-net")]
        .into_iter()
        .map(|(var, name)| (var, dir.join(format!("{name}{exe}"))))
        .collect();
    if let Some((_, missing)) = files.iter().find(|(_, f)| !f.is_file()) {
        return Err(format!("kiln for {target} carries Shards, and {} isn't there (docs/shards.md)", missing.display()));
    }
    for (var, f) in &files {
        println!("cargo:rerun-if-changed={}", f.display());
        println!("cargo:rustc-env={var}={}", f.display());
    }
    Ok(())
}

/// Shards for `target`, made by ci/shards/pack.sh from vendor/shards into `OUT_DIR/shards-carried`: release's profile
/// for a release build, test-release's otherwise (a guest's boot in a test, as Shards tests its own). Refused for a
/// target whose Shards isn't built on this platform (Windows' is built on Windows: `KILN_SHARDS_DIR` names it). The
/// outer build's flags and tools are its own target's, and its Rust kiln's: none reaches Shards' build, which takes
/// its own toolchain file, as Shards' build.rs keeps its own nested builds apart.
fn shards_packed(root: &Path, target: &str) -> Result<PathBuf, String> {
    let host = std::env::var("HOST").unwrap_or_default();
    let builds_here = match target {
        t if t.ends_with("-apple-darwin") => host.ends_with("-apple-darwin"),
        t if t.contains("-linux-") => host.contains("-linux-") || host.ends_with("-apple-darwin"),
        t if t.ends_with("-pc-windows-msvc") => host.ends_with("-pc-windows-msvc"),
        _ => false,
    };
    if !builds_here {
        return Err(format!(
            "kiln for {target} carries Shards built on its own platform: KILN_SHARDS_DIR names the folder ci/shards/pack.sh made there (docs/shards.md)"
        ));
    }
    println!("cargo:rerun-if-changed={}", root.join("vendor/shards").display());
    println!("cargo:rerun-if-changed={}", root.join("ci/shards/pack.sh").display());
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap_or_default()).join("shards-carried");
    let profile = if std::env::var("PROFILE").is_ok_and(|p| p == "release") { "release" } else { "test-release" };
    let mut pack = std::process::Command::new(bash());
    pack.arg(root.join("ci/shards/pack.sh")).arg(target).current_dir(root).env("KILN_SHARDS_DIR", &out).env("KILN_SHARDS_PROFILE", profile);
    for name in [
        "CARGO_ENCODED_RUSTFLAGS",
        "RUSTFLAGS",
        "CARGO_BUILD_RUSTFLAGS",
        "CARGO_BUILD_TARGET",
        "TARGET_CC",
        "TARGET_CXX",
        "TARGET_AR",
        "TARGET_CFLAGS",
        "TARGET_CXXFLAGS",
        "RUSTC",
        "RUSTDOC",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "RUSTUP_TOOLCHAIN",
        "CARGO",
    ] {
        pack.env_remove(name);
    }
    // macOS's dynamic loader paths as cargo sets them for a build script are kiln's toolchain's: rust-lld under
    // Shards' toolchain must find its own libLLVM.
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("DYLD_") {
            pack.env_remove(&name);
        }
    }
    let status = pack.status().map_err(|e| format!("ci/shards/pack.sh for {target} didn't start: {e}"))?;
    if !status.success() {
        return Err(format!("ci/shards/pack.sh for {target} failed ({status}): kiln carries Shards, so it isn't built without it"));
    }
    Ok(out)
}

/// The bash that runs ci/shards/pack.sh: the one on `PATH`, or on Windows, where none may be, Git for Windows' own,
/// beside the git on `PATH` (`<git>/../bin/bash.exe`, or `<git>/../../bin/bash.exe` from Git's `cmd` folder), as the
/// release's Windows job finds it.
fn bash() -> PathBuf {
    if !cfg!(windows) || std::process::Command::new("bash").arg("--version").output().is_ok_and(|o| o.status.success()) {
        return PathBuf::from("bash");
    }
    let git = std::process::Command::new("where").arg("git").output().ok().and_then(|o| String::from_utf8(o.stdout).ok());
    for line in git.as_deref().unwrap_or("").lines() {
        let git = Path::new(line.trim());
        for up in [git.parent().and_then(Path::parent), git.parent().and_then(Path::parent).and_then(Path::parent)] {
            if let Some(bash) = up.map(|d| d.join("bin").join("bash.exe")).filter(|b| b.is_file()) {
                return bash;
            }
        }
    }
    PathBuf::from("bash")
}

/// The UI's files worth packing (`ui/dist/client`, which rust-embed embeds), packed here with brotli and with gzip
/// at their best, into `OUT_DIR/ui-packed/<the file's SHA-256>.br` and `.gz`, and the tables of them that the server
/// serves from (`OUT_DIR/ui_packed.rs`, `crate::ui`'s `PACKED` and `PACKED_GZ`): no server compresses them as it
/// starts or as they're asked for, nor keeps the copies on its heap (goal 11, plan 01a1a87b2e56 step 5; goal 11's
/// audit, row B15: a client that takes gzip alone got each file as it is the first time). A file an earlier build
/// packed is taken as it was, so a new UI build packs only what changed, and a packed file no file has now goes; the
/// packing is spread over the jobs cargo gives this script (`NUM_JOBS`). Without the UI built, the tables are empty.
fn ui_packed() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    let root = manifest.join("../../ui/dist/client");
    // Watched only where it is, as the UI server's packs are.
    let watch = if root.is_dir() { Some(root.clone()) } else { root.parent().filter(|p| p.is_dir()).map(PathBuf::from) };
    if let Some(w) = watch {
        println!("cargo:rerun-if-changed={}", w.display());
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap_or_default());
    let dir = out.join("ui-packed");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        println!("cargo:warning=could not make {}: {e}", dir.display());
    }
    let mut files = Vec::new();
    walk(&root, &root, &mut files);
    files.sort();
    let jobs = std::env::var("NUM_JOBS").ok().and_then(|n| n.parse::<usize>().ok()).unwrap_or(1).clamp(1, files.len().max(1));
    let next = std::sync::atomic::AtomicUsize::new(0);
    let entries: Vec<(Coding, (String, [u8; 32], String))> = std::thread::scope(|s| {
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
    let kept: std::collections::HashSet<&str> = entries.iter().map(|(_, (_, _, p))| p.as_str()).collect();
    for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        if !kept.contains(e.path().display().to_string().as_str()) {
            let _ = std::fs::remove_file(e.path());
        }
    }
    let of = |coding: Coding| entries.iter().filter(|(c, _)| *c == coding).map(|(_, e)| e.clone()).collect::<Vec<_>>();
    let src = ui_pack::table("PACKED", "brotli", &of(Coding::Brotli)) + &ui_pack::table("PACKED_GZ", "gzip", &of(Coding::Gzip));
    let dest = out.join("ui_packed.rs");
    if let Err(e) = std::fs::write(&dest, src) {
        println!("cargo:warning=could not write {}: {e}", dest.display());
    }
}

/// A coding the build packs the UI's files with.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Coding {
    Brotli,
    Gzip,
}

impl Coding {
    fn suffix(self) -> &'static str {
        match self {
            Self::Brotli => "br",
            Self::Gzip => "gz",
        }
    }

    fn pack(self, data: &[u8]) -> Option<Vec<u8>> {
        match self {
            Self::Brotli => ui_pack::brotli(data),
            Self::Gzip => ui_pack::gzip(data),
        }
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

/// `name`'s file at `path`, packed into `dir` each way if it's worth it and comes out smaller (as the server kept a
/// copy only then), or as an earlier build packed it: its table entries, a coding each.
fn pack(name: &str, path: &Path, dir: &Path) -> Vec<(Coding, (String, [u8; 32], String))> {
    let Ok(data) = std::fs::read(path) else { return Vec::new() };
    if !ui_pack::worth(&ui_pack::mime_of(name), data.len()) {
        return Vec::new();
    }
    let sha = ui_pack::sha256(&data);
    let mut hex = String::with_capacity(64);
    for b in sha {
        let _ = write!(hex, "{b:02x}");
    }
    let mut out = Vec::new();
    for coding in [Coding::Brotli, Coding::Gzip] {
        let file = dir.join(format!("{hex}.{}", coding.suffix()));
        if !file.is_file() {
            let Some(packed) = coding.pack(&data).filter(|p| p.len() < data.len()) else { continue };
            // Written whole or not at all: a build stopped halfway leaves no short file for the next to take.
            let part = dir.join(format!("{hex}.{}.part", coding.suffix()));
            if std::fs::write(&part, &packed).and_then(|()| std::fs::rename(&part, &file)).is_err() {
                continue;
            }
        }
        out.push((coding, (name.to_owned(), sha, file.display().to_string())));
    }
    out
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
