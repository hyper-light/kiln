//! The unpacker's tests (kiln plans 01a1ce705e4d7008993f5c7291ef4476 and 01a1ce705e5c7504b88732f443d99288,
//! step 2 of each): archives built here with tar and flate2 in each shape kiln pins (Cursor's, Devin's,
//! pi's), and in each shape of entry or pin it refuses.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::write::GzEncoder;
use sha2::{Digest as _, Sha256};
use tar::{Builder, EntryType, Header};

use super::{Fields, Member, Spec, Tree, Unpack, Why, tar_gz};

const URL: &str = "https://downloads.example/lab/1/linux/x64/agent-cli-package.tar.gz";

/// An archive's totals as `tar -t` lists its entries and `tar -tv` sizes its files.
#[derive(Debug, Clone, Copy, Default)]
struct Totals {
    entries: u64,
    bytes: u64,
}

/// A gzip tar being written, counting its totals as it goes.
struct Writer {
    tar: Builder<GzEncoder<fs::File>>,
    totals: Totals,
}

impl Writer {
    /// An entry of `kind` under `name`'s bytes as they are (tar's own setters refuse `/x` and `../x`),
    /// linking to `link` when it's a link.
    fn add(&mut self, name: &[u8], kind: EntryType, mode: u32, data: &[u8], link: Option<&str>) -> io::Result<()> {
        let mut h = Header::new_gnu();
        let gnu = h.as_gnu_mut().ok_or_else(|| io::Error::other("not a GNU header"))?;
        gnu.name.get_mut(..name.len()).ok_or_else(|| io::Error::other("a name past 100 bytes"))?.copy_from_slice(name);
        h.set_entry_type(kind);
        h.set_mode(mode);
        h.set_size(data.len() as u64);
        if let Some(link) = link {
            h.set_link_name_literal(link)?;
        }
        if matches!(kind, EntryType::Char) {
            h.set_device_major(1)?;
            h.set_device_minor(3)?;
        }
        h.set_cksum();
        self.tar.append(&h, data)?;
        self.count(kind, data.len());
        Ok(())
    }

    fn count(&mut self, kind: EntryType, len: usize) {
        if !matches!(kind, EntryType::XHeader | EntryType::XGlobalHeader | EntryType::GNULongName | EntryType::GNULongLink) {
            self.totals.entries += 1;
            if kind == EntryType::Regular {
                self.totals.bytes += len as u64;
            }
        }
    }

    fn file(&mut self, name: &str, mode: u32, data: &[u8]) -> io::Result<()> {
        self.add(name.as_bytes(), EntryType::Regular, mode, data, None)
    }

    fn dir(&mut self, name: &str, mode: u32) -> io::Result<()> {
        self.add(name.as_bytes(), EntryType::Directory, mode, b"", None)
    }

    /// A regular file whose name is past a header's 100 bytes, so tar writes a GNU long name before it.
    fn long(&mut self, name: &str, mode: u32, data: &[u8]) -> io::Result<()> {
        let mut h = Header::new_gnu();
        h.set_entry_type(EntryType::Regular);
        h.set_mode(mode);
        h.set_size(data.len() as u64);
        self.tar.append_data(&mut h, name, data)?;
        self.count(EntryType::Regular, data.len());
        Ok(())
    }
}

/// A gzip tar at `at`, its entries written by `write`; its totals.
fn archive(at: &Path, write: impl FnOnce(&mut Writer) -> io::Result<()>) -> io::Result<Totals> {
    let mut w = Writer { tar: Builder::new(GzEncoder::new(fs::File::create(at)?, Compression::default())), totals: Totals::default() };
    write(&mut w)?;
    let totals = w.totals;
    w.tar.into_inner()?.finish()?;
    Ok(totals)
}

/// One pax record, `key=value`, led by its own length.
fn pax(key: &str, value: &str) -> Vec<u8> {
    let body = format!(" {key}={value}\n");
    let mut len = body.len() + 1;
    while format!("{len}{body}").len() != len {
        len += 1;
    }
    format!("{len}{body}").into_bytes()
}

fn tree(strip: usize, t: Totals) -> Spec {
    Spec::Tree(Tree { strip, unpacked_bytes: t.bytes, entries: t.entries })
}

fn member(path: &str, data: &[u8]) -> Spec {
    Spec::Member(Member { member: path.to_owned(), sha256: hex::encode(Sha256::digest(data)), size: data.len() as u64 })
}

/// A context directory as the fetcher has it when an archive unpacks: the verified archive in it, the
/// per-arch directory not made yet.
struct Context {
    _dir: tempfile::TempDir,
    root: PathBuf,
    gz: PathBuf,
    arch: PathBuf,
}

fn context() -> io::Result<Context> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_path_buf();
    let (gz, arch) = (root.join(".cursor-amd64.part"), root.join("cursor-amd64"));
    Ok(Context { _dir: dir, root, gz, arch })
}

fn names(dir: &Path) -> io::Result<BTreeSet<String>> {
    fs::read_dir(dir)?.map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned())).collect()
}

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|n| (*n).to_owned()).collect()
}

/// Every path under `dir`, `/`-separated, with whether it's a directory and its mode.
fn walk(dir: &Path) -> io::Result<BTreeMap<String, (bool, u32)>> {
    let mut out = BTreeMap::new();
    let mut todo = vec![PathBuf::new()];
    while let Some(rel) = todo.pop() {
        for e in fs::read_dir(dir.join(&rel))? {
            let e = e?;
            let path = rel.join(e.file_name());
            let meta = fs::symlink_metadata(e.path())?;
            if meta.is_dir() {
                todo.push(path.clone());
            }
            let shown: Vec<String> = path.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
            out.insert(shown.join("/"), (meta.is_dir(), mode(&meta)));
        }
    }
    Ok(out)
}

#[cfg(unix)]
fn mode(meta: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    meta.permissions().mode() & 0o7777
}

#[cfg(not(unix))]
fn mode(_: &fs::Metadata) -> u32 {
    0
}

/// `want` as [`walk`] gives it: modes are Unix's, so elsewhere each is 0.
fn tree_of(want: &[(&str, bool, u32)]) -> BTreeMap<String, (bool, u32)> {
    want.iter().map(|(p, d, m)| ((*p).to_owned(), (*d, if cfg!(unix) { *m } else { 0 }))).collect()
}

/// Cursor's shape (one top directory, dist-package/, f10): with strip 1 its files and directories,
/// nested and an executable among them, appear under the per-arch directory exactly, only once all of
/// them are in. Each has 0755 where its owner may execute it, else 0644, whatever the archive's other
/// bits; directories the archive only implies get 0755, the root dist-package/'s. No staging directory
/// or archive is left, and the fetcher's harness.json then goes in beside them.
#[test]
fn a_strip_1_tree_appears_whole_with_its_files_directories_and_modes() -> anyhow::Result<()> {
    let c = context()?;
    let totals = archive(&c.gz, |w| {
        w.dir("dist-package/", 0o755)?;
        w.file("dist-package/cursor-agent", 0o755, b"#!/usr/bin/env bash\nexec \"$(dirname \"$0\")/node\" index.js \"$@\"\n")?;
        w.file("dist-package/index.js", 0o644, b"console.log('cursor')\n")?;
        w.file("dist-package/rg", 0o700, b"\x7fELF rg\n")?;
        w.file("dist-package/README.md", 0o444, b"# Cursor\n")?;
        w.dir("dist-package/node_modules/", 0o755)?;
        w.dir("dist-package/node_modules/a/", 0o750)?;
        w.file("dist-package/node_modules/a/package.json", 0o600, b"{}\n")?;
        w.file("dist-package/node_modules/a/lib/x.js", 0o664, b"x\n")?;
        w.dir("dist-package/node_modules/a/lib/", 0o555)?;
        w.file("dist-package/implied/deep/file", 0o640, b"deep\n")
    })?;
    assert_eq!((totals.entries, totals.bytes), (11, 111));
    assert!(!c.arch.exists());

    tar_gz(&c.gz, URL, &tree(1, totals), &c.arch, Path::new("."), None)?;

    assert_eq!(names(&c.root)?, set(&["cursor-amd64"]), "no archive and no staging directory");
    let want = tree_of(&[
        ("README.md", false, 0o644),
        ("cursor-agent", false, 0o755),
        ("implied", true, 0o755),
        ("implied/deep", true, 0o755),
        ("implied/deep/file", false, 0o644),
        ("index.js", false, 0o644),
        ("node_modules", true, 0o755),
        ("node_modules/a", true, 0o755),
        ("node_modules/a/lib", true, 0o755),
        ("node_modules/a/lib/x.js", false, 0o644),
        ("node_modules/a/package.json", false, 0o644),
        ("rg", false, 0o755),
    ]);
    assert_eq!(walk(&c.arch)?, want);
    #[cfg(unix)]
    {
        assert_eq!(mode(&fs::metadata(&c.arch)?), 0o755, "the root takes dist-package/'s mode");
    }
    assert_eq!(fs::read(c.arch.join("index.js"))?, b"console.log('cursor')\n");
    assert_eq!(fs::read(c.arch.join("implied/deep/file"))?, b"deep\n");
    fs::write(c.arch.join("harness.json"), b"{\"schemaVersion\": 1}")?;
    assert_eq!(names(&c.arch)?, set(&["README.md", "cursor-agent", "harness.json", "implied", "index.js", "node_modules", "rg"]));
    Ok(())
}

/// The good archive of the failure tests below: dist-package/ with one executable and one directory.
fn good(w: &mut Writer) -> io::Result<()> {
    w.dir("dist-package/", 0o755)?;
    w.file("dist-package/cursor-agent", 0o755, b"agent\n")?;
    w.dir("dist-package/lib/", 0o755)?;
    w.file("dist-package/lib/index.js", 0o644, b"index\n")
}

/// The per-arch directory appears only once the whole archive is in: the archive comes through a FIFO,
/// and with half of it written the unpacker has made its staging directory beside where the per-arch
/// directory goes, and not that; with the rest written, the per-arch directory is there whole and the
/// staging directory gone.
#[cfg(unix)]
#[test]
fn the_per_arch_directory_appears_only_once_the_whole_archive_is_in() -> anyhow::Result<()> {
    use std::io::Write as _;
    use std::sync::mpsc;
    use std::time::Duration;

    let c = context()?;
    let whole = c.root.join("whole.tar.gz");
    let t = archive(&whole, good)?;
    let bytes = fs::read(&whole)?;
    fs::remove_file(&whole)?;
    let made = std::process::Command::new("mkfifo").arg(&c.gz).status()?;
    anyhow::ensure!(made.success(), "mkfifo: {made}");
    let half = bytes.len() / 2;
    let (told, halfway) = (mpsc::channel(), mpsc::channel::<()>());
    let (told_tx, told_rx) = told;
    let (go_tx, go_rx) = halfway;
    let gz = c.gz.clone();
    let writer = std::thread::Builder::new().spawn(move || -> io::Result<()> {
        let mut fifo = fs::OpenOptions::new().write(true).open(&gz)?;
        fifo.write_all(bytes.get(..half).unwrap_or_default())?;
        fifo.flush()?;
        let _ = told_tx.send(());
        let _ = go_rx.recv_timeout(Duration::from_secs(120));
        fifo.write_all(bytes.get(half..).unwrap_or_default())
    })?;
    let (gz, arch, spec) = (c.gz.clone(), c.arch.clone(), tree(1, t));
    let unpacker = std::thread::Builder::new().spawn(move || tar_gz(&gz, URL, &spec, &arch, Path::new("."), None))?;

    told_rx.recv_timeout(Duration::from_secs(120))?;
    let seen = names(&c.root)?;
    go_tx.send(())?;
    let unpacked = unpacker.join().map_err(|_| anyhow::anyhow!("the unpacker panicked"))?;
    writer.join().map_err(|_| anyhow::anyhow!("the writer panicked"))??;
    unpacked?;

    assert!(!seen.contains("cursor-amd64"), "halfway: {seen:?}");
    assert_eq!(seen.iter().filter(|n| n.starts_with(".cursor-amd64.unpack-")).count(), 1, "halfway: {seen:?}");
    assert_eq!(names(&c.root)?, set(&["cursor-amd64"]), "the FIFO is removed as the archive is, and staging is gone");
    assert_eq!(names(&c.arch)?, set(&["cursor-agent", "lib"]));
    Ok(())
}

/// Nothing of a tree shows before all of it is in: a fault at its very last entry, or in a total only
/// the end can tell, leaves no per-arch directory, no staging directory and no archive, though every
/// entry before it was written. A total past the pin's is refused at the entry that takes it past.
#[test]
fn nothing_of_a_tree_shows_until_all_of_it_is_in() -> anyhow::Result<()> {
    let c = context()?;
    let t = archive(&c.gz, |w| {
        good(w)?;
        w.add(b"dist-package/lib/link", EntryType::Symlink, 0o777, b"", Some("index.js"))
    })?;
    let Err(Unpack { entry, why: Why::Link, .. }) = tar_gz(&c.gz, URL, &tree(1, t), &c.arch, Path::new("."), None) else {
        anyhow::bail!("a link as the last entry: unpacked, or refused for another reason");
    };
    assert_eq!(entry.as_deref(), Some("dist-package/lib/link"));
    assert_eq!(names(&c.root)?, set(&[]), "neither the tree, its staging directory nor the archive");

    type Is = fn(&Why) -> bool;
    let cases: [(i64, i64, Option<&str>, Is); 4] = [
        (-1, 0, Some("dist-package/lib/index.js"), |w| matches!(w, Why::Entries { expected: 3, got: 4 })),
        (1, 0, None, |w| matches!(w, Why::Entries { expected: 5, got: 4 })),
        (0, -1, Some("dist-package/lib/index.js"), |w| matches!(w, Why::Bytes { expected: 11, got: 12 })),
        (0, 1, None, |w| matches!(w, Why::Bytes { expected: 13, got: 12 })),
    ];
    for (more_entries, more_bytes, at, is) in cases {
        let c = context()?;
        let t = archive(&c.gz, good)?;
        assert_eq!((t.entries, t.bytes), (4, 12));
        let entries = t.entries.checked_add_signed(more_entries).ok_or_else(|| anyhow::anyhow!("entries"))?;
        let bytes = t.bytes.checked_add_signed(more_bytes).ok_or_else(|| anyhow::anyhow!("bytes"))?;
        let pin = Spec::Tree(Tree { strip: 1, unpacked_bytes: bytes, entries });

        let Err(Unpack { entry, why, .. }) = tar_gz(&c.gz, URL, &pin, &c.arch, Path::new("."), None) else {
            anyhow::bail!("{entries} entries and {bytes} bytes pinned: unpacked, not refused");
        };

        assert_eq!(entry.as_deref(), at, "{entries} entries and {bytes} bytes pinned");
        assert!(is(&why), "{entries} entries and {bytes} bytes pinned: {why:?}");
        assert_eq!(names(&c.root)?, set(&[]), "{entries} entries and {bytes} bytes pinned: nothing is left");
    }
    Ok(())
}

/// Each entry a tree may not hold refuses the archive with its typed fault naming the entry, after
/// entries that did unpack into staging: an absolute or climbing path, links, devices, a FIFO, set-ID
/// and sticky bits, the same path twice, an entry outside the leading directory the strip drops, a
/// file the strip leaves no name, harness.json at the root, and kinds tar has beside files and
/// directories. Neither the per-arch directory nor a staging directory is left, nor the archive.
#[test]
fn each_entry_a_tree_may_not_hold_is_refused_naming_it_and_leaves_nothing() -> anyhow::Result<()> {
    type Is = fn(&Why) -> bool;
    let cases: [(&[u8], EntryType, u32, Option<&str>, Is); 20] = [
        (b"/x", EntryType::Regular, 0o644, None, |w| matches!(w, Why::Escapes)),
        (b"../x", EntryType::Regular, 0o644, None, |w| matches!(w, Why::Escapes)),
        (b"dist-package/../../x", EntryType::Regular, 0o644, None, |w| matches!(w, Why::Escapes)),
        (b"dist-package/link", EntryType::Symlink, 0o777, Some("cursor-agent"), |w| matches!(w, Why::Link)),
        (b"dist-package/hard", EntryType::Link, 0o755, Some("dist-package/cursor-agent"), |w| matches!(w, Why::Link)),
        (b"dist-package/null", EntryType::Char, 0o666, None, |w| matches!(w, Why::Device)),
        (b"dist-package/disk", EntryType::Block, 0o660, None, |w| matches!(w, Why::Device)),
        (b"dist-package/fifo", EntryType::Fifo, 0o644, None, |w| matches!(w, Why::Device)),
        (b"dist-package/su", EntryType::Regular, 0o4755, None, |w| matches!(w, Why::SetId)),
        (b"dist-package/group/", EntryType::Directory, 0o2755, None, |w| matches!(w, Why::SetId)),
        (b"dist-package/tmp/", EntryType::Directory, 0o1777, None, |w| matches!(w, Why::Sticky)),
        (b"dist-package/sticky", EntryType::Regular, 0o1644, None, |w| matches!(w, Why::Sticky)),
        (b"dist-package/cursor-agent", EntryType::Regular, 0o755, None, |w| matches!(w, Why::Duplicate)),
        (b"other/x", EntryType::Regular, 0o644, None, |w| matches!(w, Why::Prefix)),
        (b"other/", EntryType::Directory, 0o755, None, |w| matches!(w, Why::Prefix)),
        (b"README", EntryType::Regular, 0o644, None, |w| matches!(w, Why::Nameless)),
        (b"dist-package/harness.json", EntryType::Regular, 0o644, None, |w| matches!(w, Why::HarnessFile)),
        (b"dist-package/Harness.JSON", EntryType::Regular, 0o644, None, |w| matches!(w, Why::HarnessFile)),
        (b"dist-package/contiguous", EntryType::Continuous, 0o644, None, |w| matches!(w, Why::Kind(b'7'))),
        (b"dist-package/odd", EntryType::new(b'Z'), 0o644, None, |w| matches!(w, Why::Kind(b'Z'))),
    ];
    for (name, kind, mode, link, is) in cases {
        let shown = String::from_utf8_lossy(name).into_owned();
        let c = context()?;
        let t = archive(&c.gz, |w| {
            w.dir("dist-package/", 0o755)?;
            w.file("dist-package/cursor-agent", 0o755, b"agent\n")?;
            w.add(name, kind, mode, b"", link)?;
            w.file("dist-package/after", 0o644, b"after\n")
        })?;

        let Err(Unpack { url, entry, why }) = tar_gz(&c.gz, URL, &tree(1, t), &c.arch, Path::new("."), None) else {
            anyhow::bail!("{shown}: unpacked, not refused");
        };

        assert_eq!((url.as_str(), entry.as_deref()), (URL, Some(shown.as_str())), "{shown}");
        assert!(is(&why), "{shown}: {why:?}");
        assert_eq!(names(&c.root)?, set(&[]), "{shown}: nothing is left");
    }
    Ok(())
}

/// A tree's target that's there already is refused before the archive is read, and left as it was:
/// the fetcher unpacks a build's trees before anything else goes in its per-arch directory.
#[test]
fn a_target_already_there_is_refused_and_left_as_it_was() -> anyhow::Result<()> {
    let c = context()?;
    fs::create_dir(&c.arch)?;
    fs::write(c.arch.join("harness.json"), b"the fetcher's\n")?;
    let t = archive(&c.gz, good)?;

    let Err(Unpack { entry: None, why: Why::Exists(at), .. }) = tar_gz(&c.gz, URL, &tree(1, t), &c.arch, Path::new("."), None) else {
        anyhow::bail!("unpacked over the per-arch directory, or refused for another reason");
    };

    assert_eq!(at, PathBuf::from("."));
    assert_eq!(names(&c.arch)?, set(&["harness.json"]));
    assert_eq!(fs::read(c.arch.join("harness.json"))?, b"the fetcher's\n");
    assert_eq!(names(&c.root)?, set(&["cursor-amd64"]), "no archive and no staging directory");
    Ok(())
}

/// A tree at a path inside the per-arch directory appears whole too: staged beside the first directory
/// on its way that isn't there, and renamed to it.
#[test]
fn a_tree_at_a_path_inside_appears_whole_beside_what_is_there() -> anyhow::Result<()> {
    let c = context()?;
    fs::create_dir(&c.arch)?;
    fs::write(c.arch.join("harness.json"), b"{}")?;
    let t = archive(&c.gz, good)?;

    tar_gz(&c.gz, URL, &tree(1, t), &c.arch, Path::new("opt/cursor"), None)?;

    assert_eq!(names(&c.arch)?, set(&["harness.json", "opt"]), "no staging directory");
    let want = tree_of(&[
        ("cursor", true, 0o755),
        ("cursor/cursor-agent", false, 0o755),
        ("cursor/lib", true, 0o755),
        ("cursor/lib/index.js", false, 0o644),
    ]);
    assert_eq!(walk(&c.arch.join("opt"))?, want);
    #[cfg(unix)]
    {
        assert_eq!(mode(&fs::metadata(c.arch.join("opt"))?), 0o755);
    }
    assert_eq!(names(&c.root)?, set(&["cursor-amd64"]));
    Ok(())
}

/// Devin's shape at strip 0 (f6): a `./` root, directories, an executable and documents, beside a pax
/// global header, a GNU long name and a pax header naming the entry after it, none of the three counted
/// as an entry; nothing else is written, no marker (f26).
#[test]
fn devins_shape_unpacks_at_strip_0_with_nothing_beside_it() -> anyhow::Result<()> {
    let c = context()?;
    let long = format!("share/{}/deep.mdx", "d".repeat(120));
    let t = archive(&c.gz, |w| {
        w.add(b"pax_global_header", EntryType::XGlobalHeader, 0o644, &pax("comment", "a test's"), None)?;
        w.dir("./", 0o755)?;
        w.dir("bin/", 0o755)?;
        w.file("bin/devin", 0o755, b"#!/bin/sh\necho 'devin 3000.12.43 (test)'\n")?;
        w.dir("share/", 0o755)?;
        w.dir("share/devin/", 0o755)?;
        w.dir("share/devin/docs/", 0o755)?;
        w.file("share/devin/docs/index.mdx", 0o644, b"# Devin\n")?;
        w.long(&long, 0o644, b"long\n")?;
        w.add(b"PaxHeaders/short", EntryType::XHeader, 0o644, &pax("path", "share/named-by-pax"), None)?;
        w.file("share/short", 0o600, b"pax\n")
    })?;
    assert_eq!(t.entries, 9);

    tar_gz(&c.gz, URL, &tree(0, t), &c.arch, Path::new("."), None)?;

    assert_eq!(names(&c.root)?, set(&["cursor-amd64"]));
    assert_eq!(names(&c.arch)?, set(&["bin", "share"]));
    assert_eq!(names(&c.arch.join("share"))?, BTreeSet::from(["d".repeat(120), "devin".into(), "named-by-pax".into()]));
    assert_eq!(fs::read(c.arch.join("bin/devin"))?, b"#!/bin/sh\necho 'devin 3000.12.43 (test)'\n");
    assert_eq!(fs::read(c.arch.join(&long))?, b"long\n");
    assert_eq!(fs::read(c.arch.join("share/named-by-pax"))?, b"pax\n");
    #[cfg(unix)]
    {
        for (path, want) in [("bin/devin", 0o755), ("share/devin/docs/index.mdx", 0o644), ("share/named-by-pax", 0o644)] {
            assert_eq!(mode(&fs::metadata(c.arch.join(path))?), want, "{path}");
        }
    }
    Ok(())
}

/// pi's build in pin order (its pin at kiln b89452884): its tree with pi/ stripped becomes the per-arch
/// directory, then ripgrep's and fd's members go to bin/rg and bin/fd with the pin's mode, from archives
/// that also hold a link and a set-ID file the picks never write, and then the fetcher's harness.json.
/// A member colliding with bin/rg after is refused as Exists and leaves bin/rg as it was.
#[test]
fn a_pi_shaped_build_unpacks_its_tree_then_picks_its_members() -> anyhow::Result<()> {
    let c = context()?;
    let (rg, fd) = ("ripgrep-15.2.0-x86_64-unknown-linux-musl", "fd-v10.5.2-x86_64-unknown-linux-musl");
    let (rg_gz, fd_gz, again_gz) = (c.root.join(".rg.part"), c.root.join(".fd.part"), c.root.join(".rg2.part"));
    let t = archive(&c.gz, |w| {
        w.dir("pi/", 0o750)?;
        w.file("pi/pi", 0o755, b"#!/bin/sh\necho pi\n")?;
        w.file("pi/package.json", 0o644, b"{\"version\": \"1.5.2\"}\n")?;
        w.dir("pi/docs/", 0o755)?;
        w.file("pi/docs/usage.md", 0o644, b"# pi\n")?;
        w.dir("pi/theme/", 0o755)
    })?;
    for (gz, content) in [(&rg_gz, b"rg\n".as_slice()), (&again_gz, b"another rg\n".as_slice())] {
        archive(gz, |w| {
            w.dir(&format!("{rg}/"), 0o755)?;
            w.add(format!("{rg}/complete/_rg").as_bytes(), EntryType::Symlink, 0o777, b"", Some("../doc/rg.1"))?;
            w.file(&format!("{rg}/suid"), 0o4755, b"never written\n")?;
            w.file(&format!("{rg}/rg"), 0o644, content)
        })?;
    }
    archive(&fd_gz, |w| {
        w.file(&format!("{fd}/LICENSE-MIT"), 0o644, b"MIT\n")?;
        w.file(&format!("{fd}/fd"), 0o755, b"fd\n")
    })?;

    tar_gz(&c.gz, URL, &tree(1, t), &c.arch, Path::new("."), None)?;
    tar_gz(&rg_gz, URL, &member(&format!("{rg}/rg"), b"rg\n"), &c.arch, Path::new("bin/rg"), Some(0o755))?;
    tar_gz(&fd_gz, URL, &member(&format!("./{fd}/fd"), b"fd\n"), &c.arch, Path::new("bin/fd"), Some(0o755))?;

    let want = tree_of(&[
        ("bin", true, 0o755),
        ("bin/fd", false, 0o755),
        ("bin/rg", false, 0o755),
        ("docs", true, 0o755),
        ("docs/usage.md", false, 0o644),
        ("package.json", false, 0o644),
        ("pi", false, 0o755),
        ("theme", true, 0o755),
    ]);
    assert_eq!(walk(&c.arch)?, want);
    assert_eq!(fs::read(c.arch.join("bin/rg"))?, b"rg\n");
    fs::write(c.arch.join("harness.json"), b"{\"schemaVersion\": 1}")?;
    assert_eq!(names(&c.root)?, set(&[".rg2.part", "cursor-amd64"]), "each archive unpacked is removed");

    let again = member(&format!("{rg}/rg"), b"another rg\n");
    let Err(Unpack { entry, why: Why::Exists(at), .. }) = tar_gz(&again_gz, URL, &again, &c.arch, Path::new("bin/rg"), Some(0o755)) else {
        anyhow::bail!("a member written over bin/rg, or refused for another reason");
    };
    assert_eq!((entry, at), (Some(format!("{rg}/rg")), PathBuf::from("bin/rg")));
    assert_eq!(fs::read(c.arch.join("bin/rg"))?, b"rg\n");
    assert_eq!(names(&c.root)?, set(&["cursor-amd64"]));
    Ok(())
}

/// A member is one regular file the archive holds, of its pinned size and SHA-256: a missing one, a
/// link, a directory, a set-ID or sticky file or pinned mode, another size or another digest refuses
/// the archive naming the member, and writes nothing: neither its file nor the directories made for it.
#[test]
fn a_member_is_one_regular_file_held_to_its_pin() -> anyhow::Result<()> {
    type Is = fn(&Why) -> bool;
    let rg = b"rg\n";
    let pinned = |path: &str| member(path, rg);
    let other_size = Spec::Member(Member { member: "top/rg".into(), sha256: hex::encode(Sha256::digest(rg)), size: 4 });
    let other_digest = Spec::Member(Member { member: "top/rg".into(), sha256: hex::encode(Sha256::digest(b"rh\n")), size: 3 });
    let cases: [(Spec, Option<u32>, &str, Is); 9] = [
        (pinned("top/missing"), None, "top/missing", |w| matches!(w, Why::Missing)),
        (pinned("top/link"), None, "top/link", |w| matches!(w, Why::Link)),
        (pinned("top/dir"), None, "top/dir", |w| matches!(w, Why::Kind(b'5'))),
        (pinned("top/suid"), None, "top/suid", |w| matches!(w, Why::SetId)),
        (pinned("top/sticky"), None, "top/sticky", |w| matches!(w, Why::Sticky)),
        (pinned("top/rg"), Some(0o4755), "top/rg", |w| matches!(w, Why::SetId)),
        (pinned("top/rg"), Some(0o1755), "top/rg", |w| matches!(w, Why::Sticky)),
        (other_size, None, "top/rg", |w| matches!(w, Why::Size { expected: 4, got: 3 })),
        (other_digest, None, "top/rg", |w| matches!(w, Why::Digest { .. })),
    ];
    for (asked, mode, shown, is) in cases {
        let c = context()?;
        archive(&c.gz, |w| {
            w.file("top/rg", 0o755, rg)?;
            w.add(b"top/link", EntryType::Symlink, 0o777, b"", Some("rg"))?;
            w.dir("top/dir/", 0o755)?;
            w.file("top/suid", 0o4755, rg)?;
            w.file("top/sticky", 0o1755, rg)
        })?;

        let Err(Unpack { entry, why, .. }) = tar_gz(&c.gz, URL, &asked, &c.arch, Path::new("bin/rg"), mode) else {
            anyhow::bail!("{shown}: written, not refused");
        };

        assert_eq!(entry.as_deref(), Some(shown));
        assert!(is(&why), "{shown}: {why:?}");
        assert_eq!(names(&c.root)?, set(&[]), "{shown}: nothing is left, the per-arch directory and bin/ neither");
    }
    Ok(())
}

/// pin.json's `unpack` is one of two forms: a tree's fields or a member's, each one its form needs, none
/// of the other's, nothing else. Both, neither, one lacking or another format is a typed fault, the same
/// one a pin's parse gives. A pin a call can't take (a mode for a tree, a path or member outside the
/// directory, a member at `.`) is refused before the archive is read.
#[test]
fn the_unpack_object_is_one_form_or_refused() -> anyhow::Result<()> {
    let cursor: Spec = serde_json::from_str(r#"{"format": "tar.gz", "strip": 1, "unpacked_bytes": 2048, "entries": 3}"#)?;
    assert_eq!(cursor, Spec::Tree(Tree { strip: 1, unpacked_bytes: 2048, entries: 3 }));
    let agy: Spec = serde_json::from_str(r#"{"format": "tar.gz", "member": "antigravity", "sha256": "00ff", "size": 7}"#)?;
    assert_eq!(agy, Spec::Member(Member { member: "antigravity".into(), sha256: "00ff".into(), size: 7 }));

    type Is = fn(&Why) -> bool;
    let refused: [(&str, Is); 7] = [
        (r#"{"format": "tar.gz", "strip": 1, "unpacked_bytes": 1, "entries": 1, "member": "rg"}"#, |w| matches!(w, Why::Both)),
        (r#"{"format": "tar.gz", "member": "rg", "sha256": "00", "size": 1, "entries": 1}"#, |w| matches!(w, Why::Both)),
        (r#"{"format": "tar.gz"}"#, |w| matches!(w, Why::Neither)),
        (r#"{"format": "tar.gz", "strip": 1}"#, |w| matches!(w, Why::Lacks("unpacked_bytes"))),
        (r#"{"format": "tar.gz", "unpacked_bytes": 1, "entries": 1}"#, |w| matches!(w, Why::Lacks("strip"))),
        (r#"{"format": "tar.gz", "member": "rg", "size": 1}"#, |w| matches!(w, Why::Lacks("sha256"))),
        (r#"{"format": "zip", "strip": 1, "unpacked_bytes": 1, "entries": 1}"#, |w| matches!(w, Why::Format(f) if f == "zip")),
    ];
    for (json, is) in refused {
        let fields: Fields = serde_json::from_str(json)?;
        let Err(why) = Spec::try_from(fields) else {
            anyhow::bail!("{json}: taken, not refused");
        };
        assert!(is(&why), "{json}: {why:?}");
        let Err(parsed) = serde_json::from_str::<Spec>(json) else {
            anyhow::bail!("{json}: parsed as a pin's");
        };
        assert!(parsed.to_string().contains(&why.to_string()), "{json}: {parsed}");
    }
    assert!(serde_json::from_str::<Spec>(r#"{"format": "tar.gz", "strp": 1, "unpacked_bytes": 1, "entries": 1}"#).is_err());

    let refused_before: [(Spec, &str, Option<u32>, Is); 4] = [
        (cursor.clone(), ".", Some(0o755), |w| matches!(w, Why::TreeMode)),
        (cursor.clone(), "../elsewhere", None, |w| matches!(w, Why::Escapes)),
        (member("/top/rg", b"rg\n"), "bin/rg", None, |w| matches!(w, Why::Escapes)),
        (member("top/rg", b"rg\n"), ".", None, |w| matches!(w, Why::Escapes)),
    ];
    for (asked, path, mode, is) in refused_before {
        let c = context()?;
        archive(&c.gz, |w| w.file("top/rg", 0o755, b"rg\n"))?;
        let Err(Unpack { why, .. }) = tar_gz(&c.gz, URL, &asked, &c.arch, Path::new(path), mode) else {
            anyhow::bail!("{asked:?} at {path}: unpacked, not refused");
        };
        assert!(is(&why), "{asked:?} at {path}: {why:?}");
        assert_eq!(names(&c.root)?, set(&[]));
    }
    Ok(())
}

/// Bytes that aren't a gzip tar refuse as the archive's fault, naming no entry, and leave nothing.
#[test]
fn what_is_not_a_gzip_tar_is_refused_whole() -> anyhow::Result<()> {
    let c = context()?;
    fs::write(&c.gz, b"not gzip at all")?;
    let pin = Spec::Tree(Tree { strip: 1, unpacked_bytes: 0, entries: 0 });
    let Err(Unpack { entry, why: Why::Io(_), .. }) = tar_gz(&c.gz, URL, &pin, &c.arch, Path::new("."), None) else {
        anyhow::bail!("unpacked, or refused for another reason");
    };
    assert_eq!(entry, None);
    assert_eq!(names(&c.root)?, set(&[]));
    Ok(())
}

/// Bytes no compressor shrinks, so cutting an archive in half cuts into them.
fn noise(len: usize) -> Vec<u8> {
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

/// An archive that stops partway through a file (as one cut short would, were its digest not checked
/// first) leaves nothing: for a strip-1 tree no per-arch or staging directory, for a member no bin/rg in
/// part and no bin/, and no archive either way.
#[test]
fn an_archive_cut_short_leaves_nothing_behind() -> anyhow::Result<()> {
    let big = noise(512 << 10);
    let tree_of_big = |w: &mut Writer| -> io::Result<()> {
        w.dir("dist-package/", 0o755)?;
        w.file("dist-package/small", 0o644, b"small\n")?;
        w.file("dist-package/big", 0o644, &big)
    };
    let member_big = |w: &mut Writer| w.file("top/rg", 0o755, &big);
    type Writes<'a> = &'a dyn Fn(&mut Writer) -> io::Result<()>;
    let cases: [(Spec, &str, Option<u32>, &str, Writes<'_>); 2] = [
        (tree(1, Totals { entries: 3, bytes: 6 + big.len() as u64 }), ".", None, "dist-package/big", &tree_of_big),
        (member("top/rg", &big), "bin/rg", Some(0o755), "top/rg", &member_big),
    ];
    for (asked, path, mode, shown, writes) in cases {
        let c = context()?;
        archive(&c.gz, writes)?;
        let cut = fs::OpenOptions::new().write(true).open(&c.gz)?;
        let len = cut.metadata()?.len();
        cut.set_len(len / 2)?;
        drop(cut);

        let Err(Unpack { entry, why: Why::Io(_), .. }) = tar_gz(&c.gz, URL, &asked, &c.arch, Path::new(path), mode) else {
            anyhow::bail!("{shown}: unpacked from part of an archive, or refused for another reason");
        };

        assert_eq!(entry.as_deref(), Some(shown));
        assert_eq!(names(&c.root)?, set(&[]), "{shown}: nothing is left");
    }
    Ok(())
}

/// A directory whose owner may not search it is kept so (0644), its files inside, and becomes the root's
/// mode when it's the leading one; a failed tree's staging that holds such a directory is still removed
/// whole.
#[cfg(unix)]
#[test]
fn a_directory_its_owner_may_not_search_is_kept_and_still_discarded_whole() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let c = context()?;
    let t = archive(&c.gz, |w| {
        w.dir("top/", 0o600)?;
        w.dir("top/closed/", 0o640)?;
        w.file("top/closed/inside", 0o644, b"inside\n")
    })?;

    tar_gz(&c.gz, URL, &tree(1, t), &c.arch, Path::new("."), None)?;

    let root = mode(&fs::metadata(&c.arch)?);
    fs::set_permissions(&c.arch, fs::Permissions::from_mode(0o755))?;
    let closed = mode(&fs::metadata(c.arch.join("closed"))?);
    fs::set_permissions(c.arch.join("closed"), fs::Permissions::from_mode(0o755))?;
    assert_eq!((root, closed), (0o644, 0o644), "the root takes top/'s mode");
    assert_eq!(fs::read(c.arch.join("closed/inside"))?, b"inside\n");

    let staging = c.root.join(".x.unpack-test");
    fs::create_dir_all(staging.join("closed/deeper"))?;
    fs::write(staging.join("closed/deeper/file"), b"file\n")?;
    fs::set_permissions(staging.join("closed/deeper"), fs::Permissions::from_mode(0o644))?;
    fs::set_permissions(staging.join("closed"), fs::Permissions::from_mode(0o644))?;
    super::discard(&staging);
    assert!(!staging.exists());
    Ok(())
}
