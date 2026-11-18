//! What git ignores in a workspace of its own with no mount (kiln goal 01a1ec34599974448f6b7663ddb07001): an isolated
//! agent's microVM is served its Slates volume, so there is no working tree on this machine for git to read the
//! volume's `.gitignore` files from. Its review ([`super::change`]) and its landing take the volume's changes from
//! Slates' landing entries (A-287) and leave out what git would leave out of `git add -A` in the volume itself: a path
//! matched by the volume's own `.gitignore` files (read through Slates), then the repository's `info/exclude`, then the
//! user's excludes file (gitignore(5)), as git orders them, beneath a directory git ignores always ignored.
//!
//! The matching is the `ignore` crate's (ripgrep's; vendored vorpal links the same version): gitignore(5)'s patterns,
//! negations and directory-only rules, deepest file first, each file's last match deciding.

use std::collections::HashMap;

use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};

/// Git's ignore rules over a tree whose `.gitignore` files come from a reader ([`Ignores::ignored`]), beneath which lie
/// the repository's own excludes and the user's.
pub struct Ignores {
    /// Each directory's `.gitignore` read so far (`""` the top's), none where it has none.
    dirs: HashMap<String, Option<Gitignore>>,
    /// The repository's `info/exclude`, then the user's excludes file (`core.excludesFile`).
    below: Vec<Gitignore>,
    /// Whether names match whatever their case (git's `core.ignoreCase`).
    case_insensitive: bool,
}

impl Ignores {
    /// Rules beneath the volume's own: the repository's `info/exclude` and the user's excludes file, as their bytes say
    /// them (none where there is none), names matched whatever their case where `case_insensitive` (`core.ignoreCase`).
    pub fn new(info_exclude: Option<&[u8]>, excludes_file: Option<&[u8]>, case_insensitive: bool) -> anyhow::Result<Self> {
        let below = [info_exclude, excludes_file].into_iter().flatten().map(|bytes| rules(bytes, case_insensitive)).collect::<anyhow::Result<Vec<_>>>()?;
        Ok(Self { dirs: HashMap::new(), below, case_insensitive })
    }

    /// Whether git ignores `path` (relative to the top, `/`-separated), a directory where `is_dir`, each directory's
    /// `.gitignore` read through `read` (its path relative to the top; none where there is none) the first time it's
    /// needed. A path beneath a directory git ignores is ignored, whatever a deeper rule says (gitignore(5): a file
    /// can't be re-included beneath an excluded directory).
    pub fn ignored(&mut self, path: &str, is_dir: bool, read: &mut dyn FnMut(&str) -> anyhow::Result<Option<Vec<u8>>>) -> anyhow::Result<bool> {
        let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        for depth in 1..parts.len() {
            let dir = parts.get(..depth).unwrap_or_default().join("/");
            if self.decides(&dir, true, read)? {
                return Ok(true);
            }
        }
        self.decides(&parts.join("/"), is_dir, read)
    }

    /// Whether `path`'s own rules ignore it: the `.gitignore` of each directory it lies in, deepest first, the first that
    /// matches deciding; then those beneath them.
    fn decides(&mut self, path: &str, is_dir: bool, read: &mut dyn FnMut(&str) -> anyhow::Result<Option<Vec<u8>>>) -> anyhow::Result<bool> {
        let parts: Vec<&str> = path.split('/').collect();
        for depth in (0..parts.len()).rev() {
            let dir = parts.get(..depth).unwrap_or_default().join("/");
            let within = parts.get(depth..).unwrap_or_default().join("/");
            if !self.dirs.contains_key(&dir) {
                let file = if dir.is_empty() { ".gitignore".to_owned() } else { format!("{dir}/.gitignore") };
                let read_rules = read(&file)?.map(|bytes| rules(&bytes, self.case_insensitive)).transpose()?;
                self.dirs.insert(dir.clone(), read_rules);
            }
            match self.dirs.get(&dir).and_then(Option::as_ref).map(|gi| gi.matched(&within, is_dir)) {
                Some(Match::Ignore(_)) => return Ok(true),
                Some(Match::Whitelist(_)) => return Ok(false),
                Some(Match::None) | None => {}
            }
        }
        for gi in &self.below {
            match gi.matched(path, is_dir) {
                Match::Ignore(_) => return Ok(true),
                Match::Whitelist(_) => return Ok(false),
                Match::None => {}
            }
        }
        Ok(false)
    }
}

/// The rules of one ignore file's `bytes`, matched relative to the directory it's in.
fn rules(bytes: &[u8], case_insensitive: bool) -> anyhow::Result<Gitignore> {
    let mut b = GitignoreBuilder::new("");
    b.case_insensitive(case_insensitive)?;
    for line in String::from_utf8_lossy(bytes).lines() {
        b.add_line(None, line)?;
    }
    Ok(b.build()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Kiln goal 01a1ec34599974448f6b7663ddb07001: git's ignore rules over a volume's own `.gitignore` files, read
    /// through a reader, with the repository's and the user's beneath them. Do ask of a build directory the top's file
    /// ignores and a file beneath it, a log a nested file ignores, one it re-includes, a file beneath an ignored
    /// directory a deeper file re-includes, one only the repository's excludes name, one only the user's name, and a
    /// tracked-looking source file. Expect each as git says it, each `.gitignore` read once however often it's asked.
    #[test]
    fn g265_ignores_are_git_s_over_a_volume_s_own_files() -> anyhow::Result<()> {
        let files: HashMap<&str, &str> = HashMap::from([
            (".gitignore", "target/\n*.log\n"),
            ("src/.gitignore", "!keep.log\n"),
            ("target/.gitignore", "!wanted.rs\n"),
        ]);
        let mut reads: Vec<String> = Vec::new();
        let mut read = |path: &str| -> anyhow::Result<Option<Vec<u8>>> {
            reads.push(path.to_owned());
            Ok(files.get(path).map(|t| t.as_bytes().to_vec()))
        };
        let mut ignores = Ignores::new(Some(b"secret.env\n".as_slice()), Some(b"*.swp\n".as_slice()), false)?;
        for (path, is_dir, ignored) in [
            ("target", true, true),
            ("target/debug/kiln", false, true),
            ("x.log", false, true),
            ("src/deep/y.log", false, true),
            ("src/keep.log", false, false),
            ("target/wanted.rs", false, true),
            ("secret.env", false, true),
            ("src/main.rs.swp", false, true),
            ("src/main.rs", false, false),
        ] {
            assert_eq!(ignores.ignored(path, is_dir, &mut read)?, ignored, "{path}");
        }
        let mut once = reads.clone();
        once.sort();
        once.dedup();
        assert_eq!(once.len(), reads.len(), "each .gitignore read once: {reads:?}");
        Ok(())
    }
}
