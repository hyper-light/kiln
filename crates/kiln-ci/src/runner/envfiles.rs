//! Environment files (docs/ci.md W06-R07), GitHub-exact: `$GITHUB_ENV`, `$GITHUB_OUTPUT` and
//! `$GITHUB_STATE` hold `NAME=value` lines and heredocs (`NAME<<DELIMITER`, the value's lines, then
//! `DELIMITER` alone); `$GITHUB_PATH` holds one directory per line; `$GITHUB_STEP_SUMMARY` holds
//! Markdown. Errors carry the runner's own messages.

/// The pairs a file holds, in order (later pairs of the same name win where they are applied).
pub fn pairs(text: &str) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    let mut lines = Lines { text, at: 0 };
    while let Some((line, _)) = lines.next_line() {
        if line.is_empty() {
            continue;
        }
        let eq = line.find('=');
        let heredoc = line.find("<<");
        match (eq, heredoc) {
            (Some(e), h) if h.is_none_or(|h| e < h) => {
                let (name, value) = (line.get(..e).unwrap_or_default(), line.get(e + 1..).unwrap_or_default());
                if name.is_empty() {
                    return Err(format!("Invalid format '{line}'. Name must not be empty"));
                }
                out.push((name.to_owned(), value.to_owned()));
            }
            (_, Some(h)) => {
                let (name, delimiter) = (line.get(..h).unwrap_or_default(), line.get(h + 2..).unwrap_or_default());
                if name.is_empty() || delimiter.is_empty() {
                    return Err(format!("Invalid format '{line}'. Name must not be empty and delimiter must not be empty"));
                }
                let start = lines.at;
                let mut end: Option<usize> = None;
                loop {
                    let before = lines.at;
                    match lines.next_line() {
                        None => return Err(format!("Invalid value. Matching delimiter not found '{delimiter}'")),
                        Some((l, nl)) if l == delimiter => {
                            let _ = nl;
                            break;
                        }
                        Some((_, false)) => return Err("Invalid value. EOF marker missing new line.".into()),
                        Some((l, true)) => {
                            // The value runs to the end of this line, without its newline.
                            end = Some(before + l.len());
                        }
                    }
                }
                let value = end.and_then(|e| text.get(start..e)).unwrap_or_default();
                out.push((name.to_owned(), value.to_owned()));
            }
            _ => return Err(format!("Invalid format '{line}'")),
        }
    }
    Ok(out)
}

struct Lines<'a> {
    text: &'a str,
    at: usize,
}

impl<'a> Lines<'a> {
    /// The next line without its end (`\n` or `\r\n`), and whether it had one.
    fn next_line(&mut self) -> Option<(&'a str, bool)> {
        let rest = self.text.get(self.at..)?;
        if rest.is_empty() {
            return None;
        }
        match rest.find('\n') {
            Some(i) => {
                let raw = rest.get(..i).unwrap_or_default();
                self.at += i + 1;
                Some((raw.strip_suffix('\r').unwrap_or(raw), true))
            }
            None => {
                self.at = self.text.len();
                Some((rest, false))
            }
        }
    }
}

/// Variables `$GITHUB_ENV` may not set (GitHub refuses them: they would let a step take over
/// later actions).
pub const BLOCKED_ENV: &[&str] = &["NODE_OPTIONS"];

/// `$GITHUB_ENV`'s pairs, refusing the blocked names.
pub fn env(text: &str) -> Result<Vec<(String, String)>, String> {
    let p = pairs(text)?;
    if let Some((n, _)) = p.iter().find(|(n, _)| BLOCKED_ENV.iter().any(|b| b.eq_ignore_ascii_case(n))) {
        return Err(format!("Can't store {n} output parameter using '$GITHUB_ENV' command."));
    }
    Ok(p)
}

/// `$GITHUB_PATH`'s directories, in the order written.
pub fn path_lines(text: &str) -> Vec<String> {
    text.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned).collect()
}

/// The directories prepended to `PATH`, accumulated over a job's steps: a directory added again
/// moves to the front; the last added comes first (GitHub-exact).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrependPath(Vec<String>);

impl PrependPath {
    /// The directories, the last added first.
    pub fn entries(&self) -> &[String] {
        &self.0
    }

    pub fn add(&mut self, dir: &str) {
        self.0.retain(|d| d != dir);
        self.0.push(dir.to_owned());
    }

    /// `PATH` with the additions in front.
    pub fn apply(&self, base: &str, sep: char) -> String {
        let mut parts: Vec<&str> = self.0.iter().rev().map(String::as_str).collect();
        if !base.is_empty() {
            parts.push(base);
        }
        parts.join(&sep.to_string())
    }
}

/// `$GITHUB_STEP_SUMMARY` at most 1 MiB per step (W06-R07).
pub const SUMMARY_LIMIT: usize = 1 << 20;

pub fn summary(text: &str) -> Result<&str, String> {
    if text.len() > SUMMARY_LIMIT {
        return Err(format!("$GITHUB_STEP_SUMMARY upload aborted, supports content up to a size of {}k, got {}k.", SUMMARY_LIMIT / 1024, text.len() / 1024));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kv(p: &[(&str, &str)]) -> Vec<(String, String)> {
        p.iter().map(|(a, b)| ((*a).to_owned(), (*b).to_owned())).collect()
    }

    /// W06-AC02: each file's syntax and errors.
    #[test]
    fn w06_ac02_files() {
        assert_eq!(pairs("A=1\nB=two=2\n\nC="), Ok(kv(&[("A", "1"), ("B", "two=2"), ("C", "")])));
        assert_eq!(pairs("JSON<<EOF\n{\n  \"a\": 1\n}\nEOF\nNEXT=x\n"), Ok(kv(&[("JSON", "{\n  \"a\": 1\n}"), ("NEXT", "x")])));
        // CRLF, an empty heredoc, `=` after `<<` in the name line.
        assert_eq!(pairs("A=1\r\nB<<END\r\nv\r\nEND\r\n"), Ok(kv(&[("A", "1"), ("B", "v")])));
        assert_eq!(pairs("E<<X\nX\n"), Ok(kv(&[("E", "")])));
        assert_eq!(pairs("H<<a=b\nv\na=b\n"), Ok(kv(&[("H", "v")])));
        // Errors.
        assert_eq!(pairs("JSON<<EOF\nnever closed\n"), Err("Invalid value. Matching delimiter not found 'EOF'".into()));
        assert_eq!(pairs("=x"), Err("Invalid format '=x'. Name must not be empty".into()));
        assert_eq!(pairs("<<EOF\nx\nEOF\n"), Err("Invalid format '<<EOF'. Name must not be empty and delimiter must not be empty".into()));
        assert_eq!(pairs("no equals"), Err("Invalid format 'no equals'".into()));
        assert_eq!(pairs("A<<EOF\nlast line without newline"), Err("Invalid value. EOF marker missing new line.".into()));
        assert!(env("NODE_OPTIONS=--require x").is_err_and(|e| e.contains("Can't store NODE_OPTIONS")));
        assert!(summary(&"x".repeat(SUMMARY_LIMIT + 1)).is_err());
    }

    #[test]
    fn path_order() {
        let mut p = PrependPath::default();
        for d in path_lines("/a\n\n /b \n") {
            p.add(&d);
        }
        p.add("/c");
        p.add("/a");
        assert_eq!(p.apply("/usr/bin", ':'), "/a:/c:/b:/usr/bin");
    }
}
