//! GitHub's filter patterns (docs/ci.md W03-R04): for `branches`, `tags`, `paths` (and their
//! `-ignore` forms), `workflow_run.workflows`, and kiln's `changed()`.
//!
//! - `*` matches any characters but `/`; `**` matches any characters. `**/` at the start or after
//!   a `/` also matches no directory at all (`**/README.md` matches `README.md`).
//! - `?` and `+` make the **preceding character** (or class) optional, or repeatable: `*.jsx?`
//!   matches `page.js` and `page.jsx`; `v[0-9]+` matches `v10`.
//! - `[...]` matches one character listed or in a range (`a-z`, `A-Z`, `0-9`).
//! - `\` makes the next character literal.
//! - A list is read in order: a pattern starting with `!` takes back what earlier patterns
//!   matched, and a later positive pattern adds it again.
//!
//! Patterns compile to anchored regular expressions (linear time, size-bounded), once per list.

use regex::Regex;

/// One compiled pattern.
#[derive(Debug, Clone)]
pub struct Pattern {
    negated: bool,
    re: Regex,
}

/// Why a pattern doesn't compile.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{pattern}` is not a valid filter pattern: {why}")]
pub struct PatternError {
    pub pattern: String,
    pub why: String,
}

impl Pattern {
    pub fn new(src: &str) -> Result<Self, PatternError> {
        let (negated, body) = match src.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, src),
        };
        let bad = |why: &str| PatternError { pattern: src.to_owned(), why: why.to_owned() };
        let re = regex::RegexBuilder::new(&format!("^{}$", translate(body).map_err(|w| bad(&w))?))
            .size_limit(1 << 20)
            .build()
            .map_err(|e| bad(&e.to_string()))?;
        Ok(Self { negated, re })
    }

    pub fn is_negated(&self) -> bool {
        self.negated
    }

    /// Whether `s` matches, ignoring negation.
    pub fn matches(&self, s: &str) -> bool {
        self.re.is_match(s)
    }
}

/// A list of patterns, read in order.
#[derive(Debug, Clone, Default)]
pub struct Patterns(Vec<Pattern>);

impl Patterns {
    pub fn new<S: AsRef<str>>(srcs: &[S]) -> Result<Self, PatternError> {
        srcs.iter().map(|s| Pattern::new(s.as_ref())).collect::<Result<Vec<_>, _>>().map(Patterns)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether the list selects `s`: the last pattern that matches decides (`!` deselects).
    pub fn matches(&self, s: &str) -> bool {
        let mut hit = false;
        for p in &self.0 {
            if p.matches(s) {
                hit = !p.negated;
            }
        }
        hit
    }

    /// Whether any of `items` is selected.
    pub fn any<'a>(&self, mut items: impl Iterator<Item = &'a str>) -> bool {
        items.any(|s| self.matches(s))
    }
}

/// The body of a pattern (without a leading `!`) as a regular expression.
fn translate(p: &str) -> Result<String, String> {
    let chars: Vec<char> = p.chars().collect();
    let mut out = String::new();
    // Where the regex of the last atom starts, for `?` and `+` to apply to it.
    let mut last_atom: Option<usize> = None;
    let mut i = 0usize;
    while let Some(&c) = chars.get(i) {
        match c {
            '*' => {
                if chars.get(i + 1) == Some(&'*') {
                    let at_segment_start = i == 0 || chars.get(i.wrapping_sub(1)) == Some(&'/');
                    if at_segment_start && chars.get(i + 2) == Some(&'/') {
                        // `**/`: any number of directories, none included.
                        out.push_str("(?:.*/)?");
                        i += 3;
                    } else {
                        out.push_str(".*");
                        i += 2;
                    }
                } else {
                    out.push_str("[^/]*");
                    i += 1;
                }
                last_atom = None;
            }
            '?' | '+' => {
                match last_atom {
                    Some(start) => {
                        let atom = out.split_off(start);
                        out.push_str(&format!("(?:{atom}){c}"));
                    }
                    // Nothing to apply to: the character itself.
                    None => out.push_str(&regex::escape(&c.to_string())),
                }
                last_atom = None;
                i += 1;
            }
            '[' => {
                let mut class = String::from("[");
                let mut j = i + 1;
                let mut closed = false;
                while let Some(&d) = chars.get(j) {
                    if d == ']' {
                        closed = true;
                        break;
                    }
                    if d == '\\' || d == '[' || d == '^' || d == '&' || d == '~' {
                        class.push('\\');
                    }
                    class.push(d);
                    j += 1;
                }
                if !closed || j == i + 1 {
                    return Err("`[` opens a class that `]` doesn't close".into());
                }
                class.push(']');
                last_atom = Some(out.len());
                out.push_str(&class);
                i = j + 1;
            }
            '\\' => {
                let Some(&d) = chars.get(i + 1) else { return Err("`\\` at the end escapes nothing".into()) };
                last_atom = Some(out.len());
                out.push_str(&regex::escape(&d.to_string()));
                i += 2;
            }
            _ => {
                last_atom = Some(out.len());
                out.push_str(&regex::escape(&c.to_string()));
                i += 1;
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sel(patterns: &[&str], s: &str) -> bool {
        Patterns::new(patterns).is_ok_and(|p| p.matches(s))
    }

    /// GitHub's filter pattern cheat sheet, every row (docs/ci.md W03-AC02; the workflow syntax
    /// reference, checked 2026-10-17): the examples it lists match; the others are ours, chosen to
    /// sit just outside each pattern.
    #[test]
    fn w03_ac02_branches_and_tags() {
        let rows: &[(&[&str], &[&str], &[&str])] = &[
            (&["feature/*"], &["feature/my-branch", "feature/your-branch"], &["feature/a/b", "feature", "xfeature/a"]),
            (&["feature/**"], &["feature/beta-a/my-branch", "feature/your-branch", "feature/mona/the/octocat"], &["feature", "features/x"]),
            (&["main", "releases/mona-the-octocat"], &["main", "releases/mona-the-octocat"], &["main2", "releases/mona"]),
            (&["*"], &["main", "releases"], &["releases/v1"]),
            (&["**"], &["all/the/branches", "every/tag"], &[]),
            (&["*feature"], &["mona-feature", "feature", "ver-10-feature"], &["feature-x", "a/feature"]),
            (&["v2*"], &["v2", "v2.0", "v2.9"], &["v1", "v3.0", "v2/x"]),
            (&["v[12].[0-9]+.[0-9]+"], &["v1.10.1", "v2.0.0"], &["v3.0.0", "v1.0", "v1.a.0", "v12.0.0"]),
        ];
        for (pats, yes, no) in rows {
            for s in *yes {
                assert!(sel(pats, s), "{pats:?} should match {s}");
            }
            for s in *no {
                assert!(!sel(pats, s), "{pats:?} should not match {s}");
            }
        }
    }

    #[test]
    fn w03_ac02_paths() {
        let rows: &[(&[&str], &[&str], &[&str])] = &[
            (&["*"], &["README.md", "server.rb"], &["docs/README.md"]),
            (&["*.jsx?"], &["page.js", "page.jsx"], &["page.jsxx", "a/page.js"]),
            (&["**"], &["all/the/files.md"], &[]),
            (&["*.js"], &["app.js", "index.js"], &["js/index.js"]),
            (&["**.js"], &["index.js", "js/index.js", "src/js/app.js"], &["index.ts"]),
            (&["docs/*"], &["docs/README.md", "docs/file.txt"], &["docs/a/b.md", "README.md"]),
            (&["docs/**"], &["docs/README.md", "docs/mona/octocat.txt"], &["doc/x", "a/docs/x"]),
            (&["docs/**/*.md"], &["docs/README.md", "docs/mona/hello-world.md", "docs/a/markdown/file.md"], &["docs/a.txt", "x/docs/a.md"]),
            (&["**/docs/**"], &["docs/hello.md", "dir/docs/my-file.txt", "space/docs/plan/space.doc"], &["docsx/a", "a/mydocs/b"]),
            (&["**/README.md"], &["README.md", "js/README.md"], &["README.mdx", "js/xREADME.md"]),
            (&["**/*src/**"], &["a/src/app.js", "my-src/code/js/app.js"], &["src", "a/srcx/b"]),
            (&["**/*-post.md"], &["my-post.md", "path/their-post.md"], &["post.md", "a/my-post.mdx"]),
            (&["**/migrate-*.sql"], &["migrate-10909.sql", "db/migrate-v1.0.sql", "db/sept/migrate-v1.sql"], &["migrate.sql", "db/migrate-1.txt"]),
            (&["*.md", "!README.md"], &["hello.md"], &["README.md", "docs/hello.md"]),
            (&["*.md", "!README.md", "README*"], &["hello.md", "README.md", "README.doc"], &["docs/hello.md"]),
        ];
        for (pats, yes, no) in rows {
            for s in *yes {
                assert!(sel(pats, s), "{pats:?} should match {s}");
            }
            for s in *no {
                assert!(!sel(pats, s), "{pats:?} should not match {s}");
            }
        }
    }

    #[test]
    fn classes_escapes_and_errors() {
        assert!(sel(&["[CB]at"], "Cat") && sel(&["[CB]at"], "Bat") && !sel(&["[CB]at"], "Hat"));
        assert!(sel(&["[1-2]00"], "100") && sel(&["[1-2]00"], "200") && !sel(&["[1-2]00"], "300"));
        assert!(sel(&["[0-9a-z]"], "q") && !sel(&["[0-9a-z]"], "Q"));
        assert!(sel(&["a\\*b"], "a*b") && !sel(&["a\\*b"], "axb"));
        // Regex characters are literal.
        assert!(sel(&["a.b"], "a.b") && !sel(&["a.b"], "axb"));
        assert!(sel(&["(x)|y"], "(x)|y"));
        // `?` and `+` apply to a class too, and alone they are themselves.
        assert!(sel(&["v[0-9]+"], "v123") && !sel(&["v[0-9]+"], "v"));
        assert!(sel(&["?"], "?"));
        assert!(Pattern::new("a[b").is_err());
        assert!(Pattern::new("a\\").is_err());
        // An empty list selects nothing.
        assert!(!sel(&[], "main"));
    }
}
