//! `hashFiles(…)` (docs/ci.md W01-R09): GitHub's runner hashes, in turn, the SHA-256 of each
//! matched file's contents, and the result is the hex SHA-256 of those digests (or `""` when
//! nothing matched). Patterns follow `@actions/glob`: `*`, `?`, `[…]`, `**`, `!` exclusions in
//! order, dot files matched, and a matched directory includes everything below it.
//! **Deviation:** kiln hashes the matched files sorted by path, so the value does not depend on
//! the order a file system lists them in.
//!
//! The algorithm is pure: the caller supplies the files through [`FileSource`] (the runner's real
//! file system, or a list in tests).

use sha2::{Digest as _, Sha256};

use super::ExprError;

/// The files `hashFiles` may see.
pub trait FileSource {
    /// The workspace's absolute path, without a trailing separator (patterns that start with it
    /// are made relative).
    fn root(&self) -> String;
    /// Every regular file below the given relative directories (`""` is the whole workspace), as
    /// relative paths with `/` separators, following symbolic links that stay inside the
    /// workspace (and only those). The directories are the patterns' literal prefixes, so a
    /// pattern like `crates/*/Cargo.toml` never walks `node_modules`.
    fn files(&self, under: &[String]) -> Result<Vec<String>, ExprError>;
    /// The SHA-256 of a file's contents.
    fn digest(&self, path: &str) -> Result<[u8; 32], ExprError>;
}

/// Computes `hashFiles(patterns…)` over `src`. `case_insensitive` is for Windows and macOS
/// runners, as `@actions/glob` matches there.
pub fn hash_files(src: &dyn FileSource, patterns: &[String], case_insensitive: bool) -> Result<String, ExprError> {
    let files = matching(src, patterns, case_insensitive)?;
    if files.is_empty() {
        return Ok(String::new());
    }
    let mut outer = Sha256::new();
    for f in &files {
        outer.update(src.digest(f)?);
    }
    Ok(hex(&outer.finalize()))
}

/// The files `patterns` match in `src`, sorted, by `@actions/glob`'s rules as [`hash_files`]
/// reads them (test reports use them too, W16-R01).
pub fn matching(src: &dyn FileSource, patterns: &[String], case_insensitive: bool) -> Result<Vec<String>, ExprError> {
    let root = src.root();
    let pats: Vec<(bool, Vec<String>)> = patterns
        .iter()
        .flat_map(|p| p.lines().map(str::to_owned).collect::<Vec<_>>())
        .map(|p| p.trim().to_owned())
        .filter(|p| !p.is_empty() && !p.starts_with('#'))
        .map(|p| {
            let (neg, body) = match p.strip_prefix('!') {
                Some(b) => (true, b.to_owned()),
                None => (false, p),
            };
            (neg, segments(&relative(&body, &root)))
        })
        .collect();
    if pats.iter().all(|(neg, _)| *neg) {
        return Ok(Vec::new());
    }
    let mut files = src.files(&search_roots(&pats))?;
    files.sort();
    files.dedup();
    files.retain(|f| {
        let parts: Vec<&str> = f.split('/').filter(|s| !s.is_empty()).collect();
        let mut included = false;
        for (neg, pat) in &pats {
            if matches_or_ancestor(pat, &parts, case_insensitive) {
                included = !neg;
            }
        }
        included
    });
    Ok(files)
}

/// Where to look: the literal leading segments of each inclusion pattern, without the ones
/// inside another (`""` when a pattern starts with a wildcard).
fn search_roots(pats: &[(bool, Vec<String>)]) -> Vec<String> {
    let mut roots: Vec<String> = pats
        .iter()
        .filter(|(neg, _)| !neg)
        .map(|(_, segs)| {
            let lit: Vec<&str> = segs.iter().take_while(|s| !s.contains(['*', '?', '[', '\\'])).map(String::as_str).collect();
            // A pattern without wildcards may name a file: search its parent.
            let n = if lit.len() == segs.len() { lit.len().saturating_sub(1) } else { lit.len() };
            lit.get(..n).map(|l| l.join("/")).unwrap_or_default()
        })
        .collect();
    roots.sort();
    roots.dedup();
    let all = roots.clone();
    roots.retain(|r| !all.iter().any(|o| o != r && (o.is_empty() || r.starts_with(&format!("{o}/")))));
    roots
}

fn hex(b: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        let _ = write!(s, "{x:02x}");
    }
    s
}

/// A pattern made relative to the workspace (`./x` and `<root>/x` both become `x`).
fn relative(p: &str, root: &str) -> String {
    let p = p.replace('\\', "/");
    let r = root.replace('\\', "/");
    let p = match p.strip_prefix(&format!("{}/", r.trim_end_matches('/'))) {
        Some(rest) if !r.is_empty() => rest.to_owned(),
        _ => p,
    };
    let mut p = p.as_str();
    while let Some(rest) = p.strip_prefix("./") {
        p = rest;
    }
    p.to_owned()
}

fn segments(p: &str) -> Vec<String> {
    p.split('/').filter(|s| !s.is_empty() && *s != ".").map(str::to_owned).collect()
}

/// The pattern matches the path, or one of its ancestor directories (implicit descendants).
fn matches_or_ancestor(pat: &[String], parts: &[&str], ci: bool) -> bool {
    (1..=parts.len()).any(|n| parts.get(..n).is_some_and(|prefix| match_segments(pat, prefix, ci)))
}

/// Matches pattern segments against path segments; `**` takes zero or more segments.
pub(crate) fn match_segments(pat: &[String], path: &[&str], ci: bool) -> bool {
    // dp[i][j]: pat[i..] matches path[j..]
    let (n, m) = (pat.len(), path.len());
    let mut dp = vec![vec![false; m + 1]; n + 1];
    if let Some(row) = dp.get_mut(n)
        && let Some(cell) = row.get_mut(m)
    {
        *cell = true;
    }
    for i in (0..n).rev() {
        for j in (0..=m).rev() {
            let seg = pat.get(i).map(String::as_str).unwrap_or_default();
            let next = |a: usize, b: usize| dp.get(a).and_then(|r| r.get(b)).copied().unwrap_or(false);
            let v = if seg == "**" {
                next(i + 1, j) || (j < m && next(i, j + 1))
            } else {
                j < m && path.get(j).is_some_and(|p| match_segment(seg, p, ci)) && next(i + 1, j + 1)
            };
            if let Some(row) = dp.get_mut(i)
                && let Some(cell) = row.get_mut(j)
            {
                *cell = v;
            }
        }
    }
    dp.first().and_then(|r| r.first()).copied().unwrap_or(false)
}

/// One segment: `*`, `?`, `[abc]`, `[a-z]`, `[!x]`, `\` escapes; dot files are matched.
pub(crate) fn match_segment(pat: &str, s: &str, ci: bool) -> bool {
    let p: Vec<char> = if ci { pat.to_lowercase().chars().collect() } else { pat.chars().collect() };
    let t: Vec<char> = if ci { s.to_lowercase().chars().collect() } else { s.chars().collect() };
    glob_here(&p, &t)
}

fn glob_here(p: &[char], t: &[char]) -> bool {
    // Iterative wildcard matching with backtracking on the last `*`.
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        match p.get(pi) {
            Some('*') => {
                star = Some((pi, ti));
                pi += 1;
                continue;
            }
            Some(_) => {
                if let Some((adv, ok)) = single(p, pi, t.get(ti).copied())
                    && ok
                {
                    pi += adv;
                    ti += 1;
                    continue;
                }
            }
            None => {}
        }
        match star {
            Some((sp, st)) => {
                pi = sp + 1;
                ti = st + 1;
                star = Some((sp, st + 1));
            }
            None => return false,
        }
    }
    while p.get(pi) == Some(&'*') {
        pi += 1;
    }
    pi == p.len()
}

/// Matches one pattern element at `p[pi]` against `c`: returns (pattern chars consumed, matched).
fn single(p: &[char], pi: usize, c: Option<char>) -> Option<(usize, bool)> {
    let c = c?;
    match p.get(pi)? {
        '?' => Some((1, true)),
        '\\' => Some((2, p.get(pi + 1) == Some(&c))),
        '[' => {
            let mut i = pi + 1;
            let negate = matches!(p.get(i), Some('!' | '^'));
            if negate {
                i += 1;
            }
            let mut matched = false;
            let mut first = true;
            loop {
                match p.get(i) {
                    None => return Some((1, c == '[')),
                    Some(']') if !first => break,
                    Some(&lo) => {
                        if p.get(i + 1) == Some(&'-') && p.get(i + 2).is_some_and(|x| *x != ']') {
                            let hi = p.get(i + 2).copied().unwrap_or(lo);
                            if lo <= c && c <= hi {
                                matched = true;
                            }
                            i += 3;
                        } else {
                            if lo == c {
                                matched = true;
                            }
                            i += 1;
                        }
                    }
                }
                first = false;
            }
            Some((i + 1 - pi, matched != negate))
        }
        &x => Some((1, x == c)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake(Vec<(&'static str, &'static [u8])>);

    impl FileSource for Fake {
        fn root(&self) -> String {
            "/w/repo".into()
        }
        fn files(&self, under: &[String]) -> Result<Vec<String>, ExprError> {
            Ok(self.0.iter().map(|(p, _)| (*p).to_owned()).filter(|p| under.iter().any(|u| u.is_empty() || p.starts_with(&format!("{u}/")))).collect())
        }
        fn digest(&self, path: &str) -> Result<[u8; 32], ExprError> {
            let body = self.0.iter().find(|(p, _)| *p == path).map(|(_, b)| *b).unwrap_or_default();
            Ok(Sha256::digest(body).into())
        }
    }

    fn tree() -> Fake {
        Fake(vec![
            ("Cargo.lock", b"lock"),
            ("crates/a/Cargo.toml", b"a"),
            ("crates/b/Cargo.toml", b"b"),
            ("crates/b/gen/x.rs", b"gen"),
            (".github/workflows/ci.yml", b"ci"),
            ("docs/readme.md", b"doc"),
        ])
    }

    #[test]
    fn hashes_digests_of_matched_files() -> Result<(), ExprError> {
        let t = tree();
        let one = hash_files(&t, &["Cargo.lock".into()], false)?;
        let mut outer = Sha256::new();
        outer.update(Sha256::digest(b"lock"));
        assert_eq!(one, hex(&outer.finalize()));
        assert_eq!(hash_files(&t, &["nothing/**".into()], false)?, "");
        Ok(())
    }

    #[test]
    fn globs_exclusions_descendants_and_dot_files() -> Result<(), ExprError> {
        let t = tree();
        let all_toml = hash_files(&t, &["**/Cargo.toml".into()], false)?;
        let explicit = hash_files(&t, &["crates/a/Cargo.toml".into(), "crates/b/Cargo.toml".into()], false)?;
        assert_eq!(all_toml, explicit);
        let crates = hash_files(&t, &["crates".into(), "!crates/b/gen".into()], false)?;
        assert_eq!(crates, explicit, "a directory includes its files; an exclusion removes a subtree");
        assert_ne!(hash_files(&t, &["**/*.yml".into()], false)?, "", "dot directories are matched");
        assert_eq!(hash_files(&t, &["/w/repo/Cargo.lock".into()], false)?, hash_files(&t, &["./Cargo.lock".into()], false)?);
        assert_eq!(hash_files(&t, &["!Cargo.lock".into()], false)?, "");
        Ok(())
    }

    #[test]
    fn searches_only_literal_prefixes() {
        let pats = |ps: &[&str]| ps.iter().map(|p| (p.starts_with('!'), segments(p.trim_start_matches('!')))).collect::<Vec<_>>();
        assert_eq!(search_roots(&pats(&["crates/*/Cargo.toml", "crates/a/x", "!docs/**"])), vec!["crates".to_owned()]);
        assert_eq!(search_roots(&pats(&["**/x", "crates/y"])), vec![String::new()]);
        assert_eq!(search_roots(&pats(&["Cargo.lock"])), vec![String::new()]);
    }

    #[test]
    fn segment_patterns() {
        assert!(match_segment("*.rs", "lib.rs", false));
        assert!(!match_segment("*.rs", "lib.rs.bak", false));
        assert!(match_segment("[a-c]?x", "bzx", false));
        assert!(!match_segment("[!a-c]x", "bx", false));
        assert!(match_segment("\\*", "*", false));
        assert!(match_segment("README.*", "readme.md", true));
        assert!(!match_segment("README.*", "readme.md", false));
    }
}
