//! U08 selected-repository transport. Full HEAD and configured upstream ancestry, plus refs
//! merged into either, are available to log/describe/merge-base/diff. Unrelated refs, reflogs,
//! hooks, credentials, alternates and physical object caches deliberately are not exported.
//! There is no shallow truncation. Index objects (including conflict stages) travel too.

use super::{Entry, EntryKind, HashMap, Path, PathBuf};
use anyhow::Context as _;
use std::io::{Read as _, Write as _};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const META_BYTES: u64 = 32 * 1024 * 1024;
const PACK_BYTES: u64 = 2 * 1024 * 1024 * 1024;

pub(super) struct Export {
    dir: PathBuf,
    deadline: Instant,
}

impl Drop for Export {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Export {
    fn command(root: &Path) -> Command {
        let mut c = Command::new("git");
        c.arg("-C").arg(root).args(["--no-replace-objects", "-c", "core.fsmonitor=false", "-c", "core.hooksPath=/dev/null"]);
        for key in ["GIT_DIR", "GIT_COMMON_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_OBJECT_DIRECTORY", "GIT_ALTERNATE_OBJECT_DIRECTORIES", "GIT_CONFIG", "GIT_CONFIG_COUNT"] {
            c.env_remove(key);
        }
        c.env("GIT_OPTIONAL_LOCKS", "0").env("GIT_NO_LAZY_FETCH", "1").env("GIT_TERMINAL_PROMPT", "0");
        c.stdin(Stdio::null()).stderr(Stdio::null());
        c
    }

    // All Git work shares one deadline and bounded output, including enumeration and packing.
    // Error text excludes subprocess stderr: repository configuration can contain credentials.
    fn run(&self, c: Command, limit: u64) -> anyhow::Result<Option<Vec<u8>>> {
        let output = self.dir.join("output");
        if self.run_file(c, &output, limit)? { Ok(Some(Self::metadata(&output)?)) } else { Ok(None) }
    }

    fn run_file(&self, mut c: Command, output: &Path, limit: u64) -> anyhow::Result<bool> {
        c.stdout(std::fs::File::create(output)?);
        let mut child = c.spawn().context("start reach Git export")?;
        let result = (|| -> anyhow::Result<bool> { loop {
            if Instant::now() >= self.deadline || std::fs::metadata(output)?.len() > limit || self.pack_size()? > PACK_BYTES {
                break Err(anyhow::anyhow!("U08 Git export exceeds its 60s, 32MiB metadata or 2GiB pack budget"));
            }
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status.success()),
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(e) => break Err(e.into()),
            }
        } })();
        if result.is_err() {
            let _ = child.kill();
            let _ = child.wait();
        }
        result
    }

    fn pack_size(&self) -> anyhow::Result<u64> {
        let mut size = 0u64;
        if let Ok(entries) = std::fs::read_dir(self.dir.join(".git/objects/pack")) {
            for e in entries {
                size = size.saturating_add(e?.metadata()?.len());
            }
        }
        Ok(size)
    }

    fn git(&self, root: &Path, args: &[&str]) -> anyhow::Result<Option<Vec<u8>>> {
        let mut c = Self::command(root);
        c.args(args);
        if self.dir.join(".git/index").is_file() { c.env("GIT_INDEX_FILE", self.dir.join(".git/index")); }
        self.run(c, META_BYTES)
    }

    pub(super) fn files(&self, root: &Path) -> anyhow::Result<Vec<u8>> {
        self.required(root, &["ls-files", "-z", "--cached", "--others", "--exclude-standard"])
    }

    fn required(&self, root: &Path, args: &[&str]) -> anyhow::Result<Vec<u8>> {
        self.git(root, args)?.context("Git could not construct the selected reach repository")
    }

    fn text(&self, root: &Path, args: &[&str]) -> anyhow::Result<Option<String>> {
        self.git(root, args)?.map(String::from_utf8).transpose().context("Git returned non-UTF8 metadata")
    }

    fn metadata(path: &Path) -> anyhow::Result<Vec<u8>> {
        let mut data = Vec::new();
        std::fs::File::open(path)?.take(META_BYTES.saturating_add(1)).read_to_end(&mut data)?;
        anyhow::ensure!(data.len() as u64 <= META_BYTES, "reach Git metadata exceeds 32MiB");
        Ok(data)
    }

    fn write(&self, name: &str, data: &[u8]) -> anyhow::Result<()> {
        anyhow::ensure!(Instant::now() < self.deadline && data.len() as u64 <= META_BYTES, "reach Git metadata export exceeds its budget");
        let path = self.dir.join(".git").join(super::safe_rel(name)?);
        if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; }
        std::fs::write(path, data)?;
        Ok(())
    }

    fn config(&self, key: &str, value: &str) -> anyhow::Result<()> {
        let mut c = Self::command(&self.dir);
        c.args(["config", "--file"]).arg(self.dir.join(".git/config")).args([key, value]);
        self.run(c, META_BYTES)?.context("write portable reach Git configuration")?;
        Ok(())
    }

    pub(super) fn new(root: &Path, out: &mut Vec<Entry>, sources: &mut HashMap<String, PathBuf>) -> anyhow::Result<Self> {
        let cache = std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
            .context("reach Git export needs a cache directory")?.join("kiln/reach-export");
        std::fs::create_dir_all(&cache)?;
        let dir = cache.join(crate::util::new_id());
        let builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        let mut builder = builder;
        #[cfg(unix)]
        { use std::os::unix::fs::DirBuilderExt as _; builder.mode(0o700); }
        builder.create(&dir)?;
        let export = Self { dir, deadline: Instant::now() + Duration::from_secs(60) };
        std::fs::create_dir_all(export.dir.join(".git/objects/pack"))?;
        std::fs::create_dir_all(export.dir.join(".git/refs"))?;
        let resolve = |what: &str| -> anyhow::Result<PathBuf> {
            let bytes = export.required(root, &["rev-parse", what])?;
            let path = PathBuf::from(std::str::from_utf8(&bytes)?.trim_end_matches(['\r', '\n']));
            Ok(if path.is_absolute() { path } else { root.join(path) })
        };
        let own = resolve("--absolute-git-dir")?;
        let common = resolve("--git-common-dir")?;
        let head = Self::metadata(&own.join("HEAD"))?;
        export.write("HEAD", &head)?;
        // Ref replacements/grafts change ancestry itself; refuse rather than silently export
        // a different history. In-progress rebases need a separate continuation contract.
        anyhow::ensure!(export.required(root, &["for-each-ref", "--count=1", "--format=%(refname)", "refs/replace"])?.is_empty()
            && !common.join("info/grafts").exists(), "reach Git export does not support replacement refs or grafts");
        anyhow::ensure!(!["rebase-merge", "rebase-apply", "sequencer"].iter().any(|p| own.join(p).exists()),
            "reach Git export does not support an in-progress rebase or sequencer");
        let mut roots = Vec::new();
        for name in ["MERGE_HEAD", "CHERRY_PICK_HEAD", "REVERT_HEAD"] {
            if own.join(name).is_file() {
                let data = Self::metadata(&own.join(name))?;
                roots.extend(std::str::from_utf8(&data)?.lines().map(str::to_owned));
                export.write(name, &data)?;
                for extra in ["ORIG_HEAD", "MERGE_MSG", "MERGE_MODE"] {
                    if own.join(extra).is_file() { export.write(extra, &Self::metadata(&own.join(extra))?)?; }
                }
            }
        }
        if let Some(oid) = export.text(root, &["rev-parse", "--verify", "HEAD^{commit}"])? {
            roots.push(oid.trim().to_owned());
        } else {
            let branch = std::str::from_utf8(&head)?.trim().strip_prefix("ref: ").context("invalid reach Git HEAD")?;
            export.required(root, &["check-ref-format", branch])?;
        }
        let upstream = export.text(root, &["rev-parse", "--symbolic-full-name", "@{upstream}"])?;
        if let Some(refname) = upstream.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            let oid = export.required(root, &["rev-parse", "--verify", refname])?;
            roots.push(String::from_utf8(oid)?.trim().to_owned());
        }
        anyhow::ensure!(roots.iter().all(|r| !r.is_empty() && r.bytes().all(|b| b.is_ascii_hexdigit())), "invalid reach Git history root");
        roots.sort();
        roots.dedup();
        export.write("kiln-reach-view", format!(
            "U08 selected Git view\nFull HEAD and upstream history, active merge heads, staged index objects, and refs/tags reachable from those roots are available.\nOther branches, reflogs and unrelated history are intentionally unavailable; missing refs are not evidence that the source repository lacks them.\nUse git show-ref to list available refs.\nSelected history roots:\n{}\n", roots.join("\n")
        ).as_bytes())?;
        let mut refs = std::collections::BTreeMap::new();
        for oid in &roots {
            let names = export.required(root, &["for-each-ref", "--merged", oid, "--format=%(objectname) %(refname)", "refs/heads", "refs/remotes", "refs/tags"])?;
            for line in String::from_utf8(names)?.lines() {
                if let Some((oid, name)) = line.split_once(' ') {
                    super::safe_rel(name)?;
                    refs.insert(name.to_owned(), oid.to_owned());
                }
            }
        }
        let mut revisions = roots.clone();
        // Tags are objects in their own right, even when their peeled commits are already roots.
        for (name, oid) in &refs {
            export.write(name, format!("{oid}\n").as_bytes())?;
            revisions.push(oid.to_owned());
        }
        let format = export.required(root, &["rev-parse", "--show-object-format"])?;
        export.config("core.repositoryformatversion", if format.starts_with(b"sha256") { "1" } else { "0" })?;
        if format.starts_with(b"sha256") { export.config("extensions.objectformat", "sha256")?; }
        if own.join("index").is_file() {
            anyhow::ensure!(std::fs::metadata(own.join("index"))?.len() <= META_BYTES, "reach Git index exceeds 32MiB");
            std::fs::copy(own.join("index"), export.dir.join(".git/index"))?;
            let original_time = std::fs::metadata(own.join("index"))?.modified()?;
            std::fs::File::options().write(true).open(export.dir.join(".git/index"))?.set_modified(original_time)?;
            // Even reading a split index can freshen its shared file. Put every shared part
            // in the private Git directory before asking Git to expand it.
            let mut shared = Vec::new();
            let mut shared_bytes = 0u64;
            for dir in [&own, &common] {
                for (count, item) in std::fs::read_dir(dir)?.enumerate() {
                    anyhow::ensure!(count < super::MAX_ENTRIES, "too many reach Git metadata entries");
                    anyhow::ensure!(Instant::now() < export.deadline, "reach Git export exceeded 60s");
                    let item = item?;
                    if item.file_name().to_string_lossy().starts_with("sharedindex.") && item.file_type()?.is_file() {
                        shared_bytes = shared_bytes.saturating_add(item.metadata()?.len());
                        anyhow::ensure!(shared_bytes <= META_BYTES, "reach shared Git indexes exceed 32MiB");
                        let target = export.dir.join(".git").join(item.file_name());
                        std::fs::copy(item.path(), &target)?;
                        shared.push(target);
                    }
                }
            }
            // Expand split indexes into our private index, never writing to the person's index.
            let mut c = Self::command(root);
            c.env("GIT_DIR", export.dir.join(".git"))
                .env("GIT_OBJECT_DIRECTORY", common.join("objects"))
                .env("GIT_INDEX_FILE", export.dir.join(".git/index")).args(["update-index", "--no-split-index"]);
            export.run(c, META_BYTES)?.context("materialize reach Git index")?;
            for path in shared { if path.exists() { std::fs::remove_file(path)?; } }
            let index = export.required(root, &["ls-files", "--stage", "-z"])?;
            for record in index.split(|b| *b == 0).filter(|s| !s.is_empty()) {
                let prefix = record.split(|b| *b == b'\t').next().context("invalid Git index record")?;
                // Gitlinks name commits in a different repository, not this object database.
                if prefix.starts_with(b"160000 ") { continue; }
                let oid = prefix.split(|b| *b == b' ').nth(1).context("invalid Git index object")?;
                if !oid.iter().all(|b| *b == b'0') {
                    revisions.push(std::str::from_utf8(oid)?.to_owned());
                }
            }
        }
        // A shallow source stays shallow at exactly its existing boundaries, never depth one.
        if common.join("shallow").is_file() { export.write("shallow", &Self::metadata(&common.join("shallow"))?)?; }
        if common.join("info/exclude").is_file() { export.write("info/exclude", &Self::metadata(&common.join("info/exclude"))?)?; }
        if common.join("info/attributes").is_file() { export.write("info/attributes", &Self::metadata(&common.join("info/attributes"))?)?; }
        export.config("core.bare", "false")?;
        for key in ["core.filemode", "core.ignorecase", "core.symlinks", "core.quotepath", "core.autocrlf", "core.eol", "user.name", "user.email"] {
            if let Some(value) = export.text(root, &["config", "--get", key])? { export.config(key, value.trim_end_matches(['\r', '\n']))?; }
        }
        // Keep @{upstream} resolution/ahead-behind without exporting a remote URL or credential.
        if let Some(branch) = std::str::from_utf8(&head)?.trim().strip_prefix("ref: refs/heads/") {
            for suffix in ["remote", "merge"] {
                let key = format!("branch.{branch}.{suffix}");
                if let Some(value) = export.text(root, &["config", "--get", &key])? { export.config(&key, value.trim())?; }
            }
            if let Some(remote) = export.text(root, &["config", "--get", &format!("branch.{branch}.remote")])?.filter(|r| r.trim() != ".") {
                let key = format!("remote.{}.fetch", remote.trim());
                if let Some(value) = export.text(root, &["config", "--get", &key])? { export.config(&key, value.trim())?; }
            }
        }
        if !revisions.is_empty() {
            revisions.sort();
            revisions.dedup();
            let revisions = format!("{}\n", revisions.join("\n"));
            let input = export.dir.join("revisions");
            std::fs::File::create(&input)?.write_all(revisions.as_bytes())?;
            let mut c = Self::command(root);
            c.args(["pack-objects", "--stdout", "--revs", "--delta-base-offset", "--threads=1", "--window=0"])
                .stdin(std::fs::File::open(input)?);
            let packed = export.dir.join("selected.pack");
            anyhow::ensure!(export.run_file(c, &packed, PACK_BYTES)?, "pack selected reach Git history (missing objects or export budget)");
            let mut c = Self::command(root);
            c.args(["index-pack", "--threads=1"]).arg(&packed);
            let hash = export.run(c, META_BYTES)?.context("index selected reach Git pack")?;
            let hash = std::str::from_utf8(&hash)?.trim();
            anyhow::ensure!(!hash.is_empty() && hash.bytes().all(|b| b.is_ascii_hexdigit()), "invalid Git pack digest");
            std::fs::rename(&packed, export.dir.join(format!(".git/objects/pack/pack-{hash}.pack")))?;
            std::fs::rename(packed.with_extension("idx"), export.dir.join(format!(".git/objects/pack/pack-{hash}.idx")))?;
        }
        let mut entries = Vec::new();
        super::walk(&export.dir, Path::new(".git"), &|_| false, &mut entries)?;
        entries.push(super::entry(&export.dir, Path::new(".git")).context("reach Git export disappeared")?);
        for mut e in entries {
            let source = export.dir.join(&e.path);
            // Stable across scans, but same-size metadata changes must also be sent. Keep
            // synthetic times at NTFS's 100ns resolution so Windows can round-trip them.
            e.mtime_ns = if e.kind == EntryKind::File && !e.path.starts_with(".git/objects/") {
                let digest = crate::util::sha256_hex(&Self::metadata(&source)?);
                i64::from_str_radix(digest.get(..15).context("Git metadata digest")?, 16)? / 100 * 100
            } else { 0 };
            sources.insert(e.path.clone(), source);
            out.push(e);
        }
        anyhow::ensure!(out.len() <= super::MAX_ENTRIES, "too many reach export entries");
        Ok(export)
    }
}
