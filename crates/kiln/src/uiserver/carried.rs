//! The UI server this kiln carries (vendor/bun/README.md): Bun for its platform and the UI
//! server's files, packed at build time (`ui/scripts/pack-ui-server.mjs`, embedded by build.rs) and
//! unpacked once into `<base>/ui-server/<hash>/`. The folder is named after what's packed, so a
//! kiln carrying the same reuses it and a new one never runs another's files; it's unpacked beside
//! its place and renamed into it, so a half-unpacked folder is never run; the other folders there
//! go.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use sha2::{Digest as _, Sha256};

/// How to start the UI server.
pub struct Launch {
    /// The program: Bun, or on Linux the musl loader it came with, which runs it.
    pub program: PathBuf,
    /// Its arguments before the UI server's main file.
    pub args: Vec<OsString>,
    /// The UI server's main file (`server/main.ts`).
    pub main: PathBuf,
    /// Which Bun it is, for the log.
    pub what: String,
}

#[cfg(kiln_ui_server)]
const RUNTIME: &[u8] = include_bytes!(env!("KILN_UI_RUNTIME"));
#[cfg(kiln_ui_server)]
const APP: &[u8] = include_bytes!(env!("KILN_UI_APP"));
#[cfg(not(kiln_ui_server))]
const RUNTIME: &[u8] = &[];
#[cfg(not(kiln_ui_server))]
const APP: &[u8] = &[];

/// Written last into an unpacked folder: what's in it, so a folder without it is never run.
const READY: &str = ".ready";

/// Whether this kiln carries a UI server.
pub fn carried() -> bool {
    !RUNTIME.is_empty() && !APP.is_empty()
}

/// The carried UI server, unpacked under `base` if it isn't yet, and how to start it.
pub fn launch(base: &Path) -> anyhow::Result<Launch> {
    unpacked(base, &[RUNTIME, APP])
}

fn unpacked(base: &Path, packs: &[&[u8]]) -> anyhow::Result<Launch> {
    let hash = hash_of(packs);
    let root = base.join("ui-server");
    let dir = root.join(&hash);
    if !is_ready(&dir, &hash) {
        unpack_to(&root, &dir, &hash, packs).with_context(|| format!("unpacking the UI server into {}", dir.display()))?;
    }
    sweep(&root, &hash);
    Ok(launch_in(&dir))
}

fn hash_of(packs: &[&[u8]]) -> String {
    let mut h = Sha256::new();
    for p in packs {
        h.update((p.len() as u64).to_le_bytes());
        h.update(p);
    }
    h.finalize().iter().take(10).map(|b| format!("{b:02x}")).collect()
}

fn is_ready(dir: &Path, hash: &str) -> bool {
    std::fs::read_to_string(dir.join(READY)).is_ok_and(|h| h.trim() == hash)
}

/// Unpacks `packs` beside `dir`, then renames them into it. `root` is made private to kiln's user:
/// what's in it is run as kiln.
fn unpack_to(root: &Path, dir: &Path, hash: &str, packs: &[&[u8]]) -> anyhow::Result<()> {
    let mut mk = std::fs::DirBuilder::new();
    mk.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut mk, 0o700);
    mk.create(root)?;
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    let tmp = root.join(format!(".{hash}.{}.{nanos}", std::process::id()));
    std::fs::create_dir_all(&tmp)?;
    let filled = packs.iter().try_for_each(|p| unpack(p, &tmp)).and_then(|()| std::fs::write(tmp.join(READY), hash).map_err(anyhow::Error::from));
    if let Err(e) = filled {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(e);
    }
    if std::fs::rename(&tmp, dir).is_ok() {
        return Ok(());
    }
    // Something is in the way: another kiln's unpacking that got there first (then it's ready),
    // or a broken folder, which goes.
    if is_ready(dir, hash) {
        let _ = std::fs::remove_dir_all(&tmp);
        return Ok(());
    }
    let _ = std::fs::remove_dir_all(dir);
    let moved = std::fs::rename(&tmp, dir);
    if moved.is_err() {
        let _ = std::fs::remove_dir_all(&tmp);
    }
    moved.context("moving the unpacked UI server into place")
}

/// Unpacks one brotli-compressed tar into `into`: plain files and folders only, none outside it.
fn unpack(pack: &[u8], into: &Path) -> anyhow::Result<()> {
    let mut archive = tar::Archive::new(brotli::Decompressor::new(pack, 1 << 16));
    archive.set_preserve_permissions(true);
    archive.set_preserve_mtime(false);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let kind = entry.header().entry_type();
        let path = entry.path()?.into_owned();
        if !(kind.is_file() || kind.is_dir()) {
            bail!("{} in the UI server's pack isn't a file or a folder", path.display());
        }
        if !entry.unpack_in(into)? {
            bail!("{} in the UI server's pack would land outside its folder", path.display());
        }
    }
    Ok(())
}

/// Removes everything under `root` but the folder kept: older kilns' UI servers, and unpacking
/// that never finished. Whatever can't go yet (a program still running, on Windows) goes later.
fn sweep(root: &Path, keep: &str) {
    let Ok(entries) = std::fs::read_dir(root) else { return };
    for e in entries.filter_map(Result::ok) {
        if e.file_name().to_str() != Some(keep) {
            let path = e.path();
            let _ = if path.is_dir() { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
        }
    }
}

fn launch_in(dir: &Path) -> Launch {
    let main = dir.join("server").join("main.ts");
    let what = std::fs::read_to_string(dir.join("VERSION")).map_or_else(|_| "bun".to_owned(), |v| v.trim().to_owned());
    // Bun's musl build, run by the musl loader it came with: on any Linux, nothing installed.
    let loader = dir.join("lib").join(format!("ld-musl-{}.so.1", std::env::consts::ARCH));
    if cfg!(target_os = "linux") && loader.is_file() {
        let args = vec!["--library-path".into(), dir.join("lib").into_os_string(), dir.join("bun").into_os_string()];
        return Launch { program: loader, args, main, what };
    }
    let bun = dir.join(if cfg!(windows) { "bun.exe" } else { "bun" });
    Launch { program: bun, args: Vec::new(), main, what }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// A brotli-compressed tar of `files`: (path, mode, contents).
    fn pack(files: &[(&str, u32, &[u8])]) -> anyhow::Result<Vec<u8>> {
        let mut b = tar::Builder::new(Vec::new());
        for (path, mode, data) in files {
            let mut h = tar::Header::new_ustar();
            h.set_size(data.len() as u64);
            h.set_mode(*mode);
            h.set_entry_type(tar::EntryType::Regular);
            h.set_cksum();
            b.append_data(&mut h, path, *data)?;
        }
        compress(&b.into_inner()?)
    }

    fn compress(raw: &[u8]) -> anyhow::Result<Vec<u8>> {
        let mut out = Vec::new();
        {
            let mut w = brotli::CompressorWriter::new(&mut out, 4096, 5, 22);
            w.write_all(raw)?;
        }
        Ok(out)
    }

    /// The folder a launch runs from.
    fn folder(l: &Launch) -> PathBuf {
        l.main.parent().and_then(Path::parent).map(Path::to_path_buf).unwrap_or_default()
    }

    #[test]
    fn unpacks_once_into_a_folder_named_for_what_it_holds() -> anyhow::Result<()> {
        let base = tempfile::tempdir()?;
        let runtime = pack(&[("bun", 0o755, b"#!bun"), ("VERSION", 0o644, b"bun 9.9.9 (test)\n")])?;
        let app = pack(&[("server/main.ts", 0o644, b"console.log(1)")])?;
        let l = unpacked(base.path(), &[&runtime, &app])?;
        assert_eq!(l.what, "bun 9.9.9 (test)");
        assert!(l.main.ends_with("server/main.ts") && l.main.is_file());
        let dir = folder(&l);
        assert_eq!(dir.parent(), Some(base.path().join("ui-server").as_path()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(std::fs::metadata(dir.join("bun"))?.permissions().mode() & 0o777, 0o755);
        }
        // Again: the same folder, as it was (nothing unpacked over it).
        std::fs::write(dir.join("marker"), b"x")?;
        let again = unpacked(base.path(), &[&runtime, &app])?;
        assert_eq!(again.main, l.main);
        assert!(dir.join("marker").is_file());
        // Other packs: their own folder, and the old one goes.
        let newer = pack(&[("server/main.ts", 0o644, b"console.log(2)")])?;
        let next = unpacked(base.path(), &[&runtime, &newer])?;
        assert_ne!(next.main, l.main);
        assert!(!dir.exists());
        Ok(())
    }

    #[test]
    fn a_folder_without_its_ready_mark_is_unpacked_again() -> anyhow::Result<()> {
        let base = tempfile::tempdir()?;
        let runtime = pack(&[("bun", 0o755, b"#!bun")])?;
        let app = pack(&[("server/main.ts", 0o644, b"x")])?;
        let l = unpacked(base.path(), &[&runtime, &app])?;
        let dir = folder(&l);
        std::fs::remove_file(dir.join(READY))?;
        std::fs::remove_file(&l.main)?;
        let again = unpacked(base.path(), &[&runtime, &app])?;
        assert!(again.main.is_file() && is_ready(&dir, &hash_of(&[&runtime, &app])));
        Ok(())
    }

    #[test]
    fn nothing_lands_outside_its_folder() -> anyhow::Result<()> {
        let base = tempfile::tempdir()?;
        // A tar path that climbs out, written raw (tar::Builder refuses to make one).
        let mut h = tar::Header::new_old();
        for (d, s) in h.as_old_mut().name.iter_mut().zip(b"../escape") {
            *d = *s;
        }
        h.set_size(1);
        h.set_mode(0o644);
        h.set_entry_type(tar::EntryType::Regular);
        h.set_cksum();
        let mut raw = h.as_bytes().to_vec();
        raw.push(b'x');
        raw.resize(2048, 0);
        let bad = compress(&raw)?;
        let app = pack(&[("server/main.ts", 0o644, b"x")])?;
        assert!(unpacked(base.path(), &[&bad, &app]).is_err());
        assert!(!base.path().join("escape").exists());
        // Nothing half-unpacked is left behind.
        assert_eq!(std::fs::read_dir(base.path().join("ui-server"))?.count(), 0);
        Ok(())
    }

    #[test]
    fn on_linux_the_musl_loader_runs_bun() -> anyhow::Result<()> {
        let base = tempfile::tempdir()?;
        let loader = format!("lib/ld-musl-{}.so.1", std::env::consts::ARCH);
        let runtime = pack(&[("bun", 0o755, b"#!bun"), (&loader, 0o755, b"ld")])?;
        let app = pack(&[("server/main.ts", 0o644, b"x")])?;
        let l = unpacked(base.path(), &[&runtime, &app])?;
        if cfg!(target_os = "linux") {
            assert!(l.program.ends_with(&loader));
            assert_eq!(l.args.first().map(OsString::as_os_str), Some(std::ffi::OsStr::new("--library-path")));
            assert!(l.args.last().is_some_and(|a| Path::new(a).ends_with("bun")));
        } else {
            assert!(l.program.file_stem().is_some_and(|s| s == "bun") && l.args.is_empty());
        }
        Ok(())
    }
}
