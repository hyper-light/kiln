//! `hashFiles(…)` over a real workspace (docs/ci.md W01-R09): the pure algorithm is
//! `kiln_ci::expr::files::hash_files`; this is its file source on a runner. Symbolic links are
//! followed only while they stay inside the workspace (GitHub hashes only files under
//! `GITHUB_WORKSPACE`), directory cycles are walked once, and only the directories the patterns
//! can match below are listed.

use std::collections::HashSet;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use kiln_ci::expr::ExprError;
use kiln_ci::expr::files::{FileSource, hash_files};
use sha2::{Digest as _, Sha256};

/// A workspace on disk.
pub struct Workspace {
    root: PathBuf,
}

impl Workspace {
    /// `root` is canonicalized, so links are judged against the real directory.
    pub fn new(root: &Path) -> Result<Self, ExprError> {
        let root = root.canonicalize().map_err(|e| io(&format!("workspace {}", root.display()), &e))?;
        Ok(Self { root })
    }

    /// `hashFiles(patterns…)` here.
    pub fn hash(&self, patterns: &[String]) -> Result<String, ExprError> {
        hash_files(self, patterns, cfg!(any(windows, target_os = "macos")))
    }

    /// The files `patterns` match here, relative to the workspace (W16-R01's report paths).
    pub fn matching(&self, patterns: &[String]) -> Result<Vec<String>, ExprError> {
        kiln_ci::expr::files::matching(self, patterns, cfg!(any(windows, target_os = "macos")))
    }

    pub fn root_path(&self) -> &Path {
        &self.root
    }

    fn inside(&self, p: &Path) -> bool {
        p.starts_with(&self.root)
    }

    fn walk(&self, dir: &Path, rel: &str, seen: &mut HashSet<PathBuf>, out: &mut Vec<String>) {
        let Ok(real) = dir.canonicalize() else { return };
        if !self.inside(&real) || !seen.insert(real) {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            let child = if rel.is_empty() { name } else { format!("{rel}/{name}") };
            let Ok(meta) = std::fs::symlink_metadata(&path) else { continue };
            let (is_dir, is_file) = if meta.file_type().is_symlink() {
                // Follow it only to a target inside the workspace.
                match path.canonicalize() {
                    Ok(target) if self.inside(&target) => (target.is_dir(), target.is_file()),
                    _ => continue,
                }
            } else {
                (meta.is_dir(), meta.is_file())
            };
            if is_dir {
                self.walk(&path, &child, seen, out);
            } else if is_file {
                out.push(child);
            }
        }
    }
}

fn io(what: &str, e: &std::io::Error) -> ExprError {
    ExprError::new("KCI2010", format!("hashFiles: {what}: {e}"), None)
}

impl FileSource for Workspace {
    fn root(&self) -> String {
        self.root.to_string_lossy().replace('\\', "/")
    }

    fn files(&self, under: &[String]) -> Result<Vec<String>, ExprError> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for u in under {
            let dir = if u.is_empty() { self.root.clone() } else { self.root.join(u) };
            if dir.is_dir() {
                self.walk(&dir, u, &mut seen, &mut out);
            }
        }
        Ok(out)
    }

    fn digest(&self, path: &str) -> Result<[u8; 32], ExprError> {
        let full = self.root.join(path);
        let mut f = std::fs::File::open(&full).map_err(|e| io(path, &e))?;
        let mut h = Sha256::new();
        let mut buf = vec![0u8; 64 << 10];
        loop {
            let n = f.read(&mut buf).map_err(|e| io(path, &e))?;
            if n == 0 {
                break;
            }
            h.update(buf.get(..n).unwrap_or_default());
        }
        Ok(h.finalize().into())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// docs/ci.md W01-AC06 on a real tree (F-01): nested files, an exclusion, a link inside the
    /// workspace (followed), one pointing outside (ignored), and a directory cycle.
    #[test]
    fn w01_ac06_hash_files_on_disk() -> anyhow::Result<()> {
        let outside = tempfile::tempdir()?;
        std::fs::write(outside.path().join("secret.txt"), "outside")?;
        let ws = tempfile::tempdir()?;
        let root = ws.path();
        std::fs::create_dir_all(root.join("src/deep"))?;
        std::fs::write(root.join("src/a.rs"), "a")?;
        std::fs::write(root.join("src/deep/b.rs"), "b")?;
        std::fs::write(root.join("Cargo.lock"), "lock")?;
        std::os::unix::fs::symlink(root.join("src/a.rs"), root.join("link-in.rs"))?;
        std::os::unix::fs::symlink(outside.path().join("secret.txt"), root.join("link-out.txt"))?;
        std::os::unix::fs::symlink(root.join("src"), root.join("src/deep/loop"))?;
        let w = Workspace::new(root)?;
        let all_rs = w.hash(&["**/*.rs".into()])?;
        assert_ne!(all_rs, "");
        let without_deep = w.hash(&["src/**/*.rs".into(), "!src/deep".into()])?;
        assert_ne!(all_rs, without_deep);
        assert_eq!(w.hash(&["link-out.txt".into()])?, "", "a link out of the workspace is not hashed");
        assert_ne!(w.hash(&["link-in.rs".into()])?, "", "a link inside it is");
        assert_eq!(w.hash(&["nothing-*".into()])?, "");
        assert_eq!(w.hash(&["**/*.rs".into()])?, all_rs, "stable across calls");
        Ok(())
    }
}
