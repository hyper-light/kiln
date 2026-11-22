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
        println!("cargo:rustc-env=KILN_SHARDS_HASH=000000000000");
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
    // Compress carried Shards binaries at Brotli Q11 (window 24) in parallel, cached by SHA-256
    // under OUT_DIR/shards-packed so rebuilds reuse packed blobs when Shards hasn't changed.
    // KILN_SHARDS_HASH records the exact uncompressed hash (`shards::hash_of`) so runtime
    // startup checks `.ready` in O(1) without decompressing.
    let packed_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap_or_default()).join("shards-packed");
    std::fs::create_dir_all(&packed_dir).map_err(|e| format!("{}: {e}", packed_dir.display()))?;
    let mut raw_hasher = <sha2::Sha256 as sha2::Digest>::new();
    let mut items: Vec<(&str, PathBuf, Vec<u8>, String)> = Vec::with_capacity(files.len());
    for (var, f) in &files {
        println!("cargo:rerun-if-changed={}", f.display());
        let data = std::fs::read(f).map_err(|e| format!("{}: {e}", f.display()))?;
        sha2::Digest::update(&mut raw_hasher, (data.len() as u64).to_le_bytes());
        sha2::Digest::update(&mut raw_hasher, &data);
        let sha = ui_pack::sha256(&data);
        let mut hex = String::with_capacity(64);
        for b in sha {
            let _ = write!(hex, "{b:02x}");
        }
        items.push((*var, f.clone(), data, hex));
    }
    let raw_hash: String = sha2::Digest::finalize(raw_hasher).iter().take(6).map(|b| format!("{b:02x}")).collect();
    println!("cargo:rustc-env=KILN_SHARDS_HASH={raw_hash}");
    let results: Vec<Result<(&str, PathBuf), String>> = std::thread::scope(|s| {
        let handles: Vec<_> = items
            .into_iter()
            .map(|(var, orig, data, hex)| {
                let packed_dir = &packed_dir;
                s.spawn(move || -> Result<(&str, PathBuf), String> {
                    let dest = packed_dir.join(format!("{hex}.br"));
                    if !dest.is_file() {
                        let packed = ui_pack::brotli_with(&data, 11, 24).ok_or_else(|| format!("brotli compression failed for {}", orig.display()))?;
                        let part = packed_dir.join(format!("{hex}.br.part"));
                        std::fs::write(&part, &packed).map_err(|e| format!("{}: {e}", part.display()))?;
                        std::fs::rename(&part, &dest).map_err(|e| format!("{}: {e}", dest.display()))?;
                    }
                    Ok((var, dest))
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap_or_else(|_| Err("shards compression thread panicked".into()))).collect()
    });
    let mut kept_packed = std::collections::HashSet::new();
    for res in results {
        let (var, dest) = res?;
        kept_packed.insert(dest.clone());
        println!("cargo:rustc-env={var}={}", dest.display());
    }
    for e in std::fs::read_dir(&packed_dir).into_iter().flatten().flatten() {
        if !kept_packed.contains(&e.path()) {
            let _ = std::fs::remove_file(e.path());
        }
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

/// Every file under `ui/dist/client`, SHA-256 deduplicated and brotli-Q11 packed where smaller,
/// into ONE contiguous `OUT_DIR/ui_blob.bin` plus `OUT_DIR/ui_packed.rs` (`UI_BLOB`, `UI_FILES`).
/// Eliminates duplicate raw (`rust-embed`) and `.gz` copies from the binary while serving `br`
/// zero-copy from static pages and caching per-file `.br` results in `OUT_DIR/ui-packed/`.
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
    let mut prepared: Vec<PreparedUiFile> = std::thread::scope(|s| {
        let workers: Vec<_> = (0..jobs)
            .map(|_| {
                s.spawn(|| {
                    let mut mine = Vec::new();
                    while let Some((name, path)) = files.get(next.fetch_add(1, std::sync::atomic::Ordering::Relaxed)) {
                        if let Some(p) = prepare_ui_file(name, path, &dir) {
                            mine.push(p);
                        }
                    }
                    mine
                })
            })
            .collect();
        workers.into_iter().flat_map(|w| w.join().unwrap_or_default()).collect()
    });
    prepared.sort_by(|a, b| a.name.cmp(&b.name));
    // Clean stale cached `.br` files from earlier UI builds.
    let kept_cache: std::collections::HashSet<&Path> = prepared.iter().filter_map(|p| p.cache_file.as_deref()).collect();
    for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        if !kept_cache.contains(e.path().as_path()) {
            let _ = std::fs::remove_file(e.path());
        }
    }
    // Deduplicate identical payloads by (sha, brotli) into a single contiguous UI_BLOB.
    let mut blob = Vec::new();
    let mut offsets: std::collections::HashMap<([u8; 32], bool), (usize, usize)> = std::collections::HashMap::new();
    let mut entries = Vec::with_capacity(prepared.len());
    for item in prepared {
        let (start, len) = *offsets.entry((item.sha, item.brotli)).or_insert_with(|| {
            let start = blob.len();
            blob.extend_from_slice(&item.payload);
            (start, item.payload.len())
        });
        entries.push(ui_pack::BlobEntry {
            name: item.name,
            sha: item.sha,
            start,
            len,
            orig_len: item.orig_len,
            brotli: item.brotli,
        });
    }
    let blob_path = out.join("ui_blob.bin");
    let write_blob = std::fs::read(&blob_path).ok().is_none_or(|prev| prev != blob);
    if write_blob && let Err(e) = std::fs::write(&blob_path, &blob) {
        println!("cargo:warning=could not write {}: {e}", blob_path.display());
    }
    let src = ui_pack::blob_table(&blob_path.display().to_string(), &entries);
    let dest = out.join("ui_packed.rs");
    let write_src = std::fs::read_to_string(&dest).ok().is_none_or(|prev| prev != src);
    if write_src && let Err(e) = std::fs::write(&dest, src) {
        println!("cargo:warning=could not write {}: {e}", dest.display());
    }
}

struct PreparedUiFile {
    name: String,
    sha: [u8; 32],
    orig_len: usize,
    brotli: bool,
    payload: Vec<u8>,
    cache_file: Option<PathBuf>,
}

/// Every file under `dir`, by its name under `root` (its parts joined with `/`).
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

/// Prepares `name` at `path`: computes its SHA-256 and, when compressible and smaller, packs it
/// with Brotli Q11 (cached in `dir/<sha>.br`). Otherwise keeps the raw bytes.
fn prepare_ui_file(name: &str, path: &Path, dir: &Path) -> Option<PreparedUiFile> {
    let data = std::fs::read(path).ok()?;
    let orig_len = data.len();
    let sha = ui_pack::sha256(&data);
    if ui_pack::worth(&ui_pack::mime_of(name), orig_len) {
        let mut hex = String::with_capacity(64);
        for b in sha {
            let _ = write!(hex, "{b:02x}");
        }
        let file = dir.join(format!("{hex}.br"));
        if let Ok(packed) = std::fs::read(&file)
            && packed.len() < orig_len
        {
            return Some(PreparedUiFile { name: name.to_owned(), sha, orig_len, brotli: true, payload: packed, cache_file: Some(file) });
        }
        if let Some(packed) = ui_pack::brotli(&data).filter(|p| p.len() < orig_len) {
            let part = dir.join(format!("{hex}.br.part"));
            let _ = std::fs::write(&part, &packed).and_then(|()| std::fs::rename(&part, &file));
            return Some(PreparedUiFile { name: name.to_owned(), sha, orig_len, brotli: true, payload: packed, cache_file: Some(file) });
        }
    }
    Some(PreparedUiFile { name: name.to_owned(), sha, orig_len, brotli: false, payload: data, cache_file: None })
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
