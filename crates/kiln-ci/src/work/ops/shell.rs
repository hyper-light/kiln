//! Dockerfile words (docs/ci.md W24-R29), read as BuildKit's frontend reads them
//! (`frontend/dockerfile/shell`): quotes and escapes taken out, and `$name`, `${name}`,
//! `${name:-word}`, `${name-word}`, `${name:+word}`, `${name+word}`, `${name:?word}`,
//! `${name?word}`, `${name#pattern}`, `${name##pattern}`, `${name%pattern}`, `${name%%pattern}`,
//! `${name/pattern/replacement}` and `${name//pattern/replacement}` expanded.

use regex::Regex;

/// A variable's value, if it has one.
pub(crate) type Lookup<'a> = &'a dyn Fn(&str) -> Option<String>;

/// How words are read; BuildKit's `shell.Lex`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Lex {
    pub escape: char,
    /// Quotes stay in the result.
    pub raw_quotes: bool,
    /// Escapes stay in the result.
    pub raw_escapes: bool,
    /// Quotes aren't special at all.
    pub skip_quotes: bool,
    /// A variable without a value stays as written.
    pub skip_unset: bool,
}

impl Lex {
    pub(crate) fn new(escape: char) -> Self {
        Lex { escape, raw_quotes: false, raw_escapes: false, skip_quotes: false, skip_unset: false }
    }

    /// `word` as one word: quotes and escapes out, variables in, spaces kept.
    pub(crate) fn word(&self, word: &str, env: Lookup<'_>) -> Result<String, String> {
        self.process(word, env).map(|(w, _)| w)
    }

    /// `word` split into words after expansion, at spaces outside quotes.
    pub(crate) fn words(&self, word: &str, env: Lookup<'_>) -> Result<Vec<String>, String> {
        self.process(word, env).map(|(_, w)| w)
    }

    fn process(&self, word: &str, env: Lookup<'_>) -> Result<(String, Vec<String>), String> {
        let mut s = Scan { lex: *self, chars: word.chars().collect(), pos: 0, env, raw_escapes: self.raw_escapes, depth: 0 };
        s.stop_on(None, self.raw_escapes, false).map_err(|e| format!("failed to process {word:?}: {e}"))
    }
}

/// How deep `${…}` may nest: each level is read recursively, so a word can't run the stack out.
const MAX_DEPTH: usize = 64;

/// Words as they're read: a space ends one, except inside quotes or after an escape.
#[derive(Default)]
struct Words {
    buf: String,
    words: Vec<String>,
    in_word: bool,
}

impl Words {
    fn add_char(&mut self, c: char) {
        if c.is_whitespace() {
            if self.in_word && !self.buf.is_empty() {
                self.words.push(std::mem::take(&mut self.buf));
                self.in_word = false;
            }
        } else {
            self.add_raw_char(c);
        }
    }

    fn add_raw_char(&mut self, c: char) {
        self.buf.push(c);
        self.in_word = true;
    }

    fn add_str(&mut self, s: &str) {
        for c in s.chars() {
            self.add_char(c);
        }
    }

    fn add_raw_str(&mut self, s: &str) {
        self.buf.push_str(s);
        self.in_word = true;
    }

    fn finish(mut self) -> Vec<String> {
        if !self.buf.is_empty() {
            self.words.push(std::mem::take(&mut self.buf));
        }
        self.words
    }
}

struct Scan<'a> {
    lex: Lex,
    chars: Vec<char>,
    pos: usize,
    env: Lookup<'a>,
    raw_escapes: bool,
    /// How many `${…}` it's inside.
    depth: usize,
}

impl Scan<'_> {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn next(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    /// Reads up to `stop` (or the end), with escapes kept when `raw_escapes` says so;
    /// `preserve` doubles escapes inside quotes, for patterns read again as patterns.
    fn stop_on(&mut self, stop: Option<char>, raw_escapes: bool, preserve: bool) -> Result<(String, Vec<String>), String> {
        let saved = self.raw_escapes;
        self.raw_escapes = raw_escapes;
        let r = self.stop_on_inner(stop, preserve);
        self.raw_escapes = saved;
        r
    }

    fn stop_on_inner(&mut self, stop: Option<char>, preserve: bool) -> Result<(String, Vec<String>), String> {
        let mut result = String::new();
        let mut words = Words::default();
        while let Some(ch) = self.peek() {
            if stop == Some(ch) {
                self.next();
                return Ok((result, words.finish()));
            }
            let special = match ch {
                '$' => Some(self.dollar()?),
                '<' => Some(self.heredoc()),
                '\'' if !self.lex.skip_quotes => Some(self.single(preserve)?),
                '"' if !self.lex.skip_quotes => Some(self.double(preserve)?),
                _ => None,
            };
            if let Some(tmp) = special {
                result.push_str(&tmp);
                if ch == '$' {
                    words.add_str(&tmp);
                } else {
                    words.add_raw_str(&tmp);
                }
                continue;
            }
            self.next();
            let mut ch = ch;
            if ch == self.lex.escape {
                if self.raw_escapes {
                    words.add_raw_char(ch);
                    result.push(ch);
                }
                match self.next() {
                    None => break,
                    Some(c) => {
                        ch = c;
                        words.add_raw_char(c);
                    }
                }
            } else {
                words.add_char(ch);
            }
            result.push(ch);
        }
        if let Some(stop) = stop {
            return Err(format!("unexpected end of statement while looking for matching {stop}"));
        }
        Ok((result, words.finish()))
    }

    fn single(&mut self, preserve: bool) -> Result<String, String> {
        let mut out = String::new();
        if let Some(open) = self.next()
            && self.lex.raw_quotes
        {
            out.push(open);
        }
        loop {
            match self.next() {
                None => return Err("unexpected end of statement while looking for matching single-quote".into()),
                Some('\'') => {
                    if self.lex.raw_quotes {
                        out.push('\'');
                    }
                    return Ok(out);
                }
                Some(c) => {
                    if preserve && c == self.lex.escape {
                        out.push(c);
                    }
                    out.push(c);
                }
            }
        }
    }

    fn double(&mut self, preserve: bool) -> Result<String, String> {
        let mut out = String::new();
        if let Some(open) = self.next()
            && self.lex.raw_quotes
        {
            out.push(open);
        }
        loop {
            match self.peek() {
                None => return Err("unexpected end of statement while looking for matching double-quote".into()),
                Some('"') => {
                    self.next();
                    if self.lex.raw_quotes {
                        out.push('"');
                    }
                    return Ok(out);
                }
                Some('$') => {
                    let v = self.dollar()?;
                    out.push_str(&v);
                }
                Some(c) => {
                    self.next();
                    let mut c = c;
                    if c == self.lex.escape {
                        match self.peek() {
                            // An escape at the end is dropped; the quote is never closed.
                            None => continue,
                            Some(n) if n == '"' || n == '$' || n == self.lex.escape => {
                                if self.raw_escapes {
                                    out.push(c);
                                }
                                self.next();
                                c = n;
                            }
                            Some(_) => {
                                if self.raw_escapes && (!self.lex.raw_quotes || preserve) {
                                    out.push(c);
                                }
                            }
                        }
                    }
                    out.push(c);
                }
            }
        }
    }

    fn lookup(&self, name: &str) -> Option<String> {
        (self.env)(name)
    }

    fn dollar(&mut self) -> Result<String, String> {
        self.next();
        if self.peek() != Some('{') {
            let name = self.name();
            if name.is_empty() {
                return Ok("$".into());
            }
            return Ok(match self.lookup(&name) {
                Some(v) => v,
                None if self.lex.skip_unset => format!("${name}"),
                None => String::new(),
            });
        }
        self.next();
        match self.peek() {
            None => return Err("syntax error: missing '}'".into()),
            Some('{' | '}' | ':') => return Err("syntax error: bad substitution".into()),
            Some(_) => {}
        }
        if self.depth >= MAX_DEPTH {
            return Err(format!("substitutions nest more than {MAX_DEPTH} deep"));
        }
        self.depth += 1;
        let r = self.braced();
        self.depth -= 1;
        r
    }

    /// `${…}` after its `{`.
    fn braced(&mut self) -> Result<String, String> {
        let name = self.name();
        let ch = self.next();
        let mut chs: String = ch.map(String::from).unwrap_or_default();
        match ch {
            Some('}') => Ok(match self.lookup(&name) {
                Some(v) => v,
                None if self.lex.skip_unset => format!("${{{name}}}"),
                None => String::new(),
            }),
            Some(':') => {
                let m = self.next();
                if let Some(m) = m {
                    chs.push(m);
                }
                self.modifier(&name, m, &chs, true)
            }
            Some(c @ ('+' | '-' | '?' | '#' | '%')) => self.modifier(&name, Some(c), &chs, false),
            Some('/') => self.replace(&name),
            // (BuildKit calls this an unsupported modifier.)
            None => Err("syntax error: missing '}'".into()),
            _ => Err(format!("unsupported modifier ({chs}) in substitution")),
        }
    }

    fn modifier(&mut self, name: &str, ch: Option<char>, chs: &str, null_is_unset: bool) -> Result<String, String> {
        let raw = matches!(ch, Some('#' | '%'));
        if null_is_unset && raw {
            return Err(format!("unsupported modifier ({chs}) in substitution"));
        }
        let word = match self.stop_on(Some('}'), raw, raw) {
            Ok((w, _)) => w,
            Err(_) if self.peek().is_none() => return Err("syntax error: missing '}'".into()),
            Err(e) => return Err(e),
        };
        let value = self.lookup(name);
        if self.lex.skip_unset && value.is_none() {
            return Ok(format!("${{{name}{chs}{word}}}"));
        }
        let set = value.is_some();
        let value = value.unwrap_or_default();
        let empty = !set || (null_is_unset && value.is_empty());
        match ch {
            Some('-') => Ok(if empty { word } else { value }),
            Some('+') => Ok(if empty { String::new() } else { word }),
            Some('?') => {
                if !set {
                    let why = if word.is_empty() { "is not allowed to be unset" } else { word.as_str() };
                    return Err(format!("{name}: {why}"));
                }
                if null_is_unset && value.is_empty() {
                    let why = if word.is_empty() { "is not allowed to be empty" } else { word.as_str() };
                    return Err(format!("{name}: {why}"));
                }
                Ok(value)
            }
            Some(c @ ('#' | '%')) => {
                let (greedy, pattern) = match word.strip_prefix(c) {
                    Some(p) => (true, p.to_owned()),
                    None => (false, word),
                };
                if c == '%' { trim_suffix(&pattern, &value, greedy) } else { trim_prefix(&pattern, &value, greedy) }
            }
            _ => Err(format!("unsupported modifier ({chs}) in substitution")),
        }
    }

    fn replace(&mut self, name: &str) -> Result<String, String> {
        let all = self.peek() == Some('/');
        if all {
            self.next();
        }
        let pattern = match self.stop_on(Some('/'), true, true) {
            Ok((p, _)) => p,
            Err(_) if self.peek().is_none() => return Err("syntax error: missing '/' in ${}".into()),
            Err(e) => return Err(e),
        };
        let replacement = match self.stop_on(Some('}'), true, false) {
            Ok((r, _)) => r,
            Err(_) if self.peek().is_none() => return Err("syntax error: missing '}'".into()),
            Err(e) => return Err(e),
        };
        let value = self.lookup(name);
        if self.lex.skip_unset && value.is_none() {
            return Ok(format!("${{{name}/{pattern}/{replacement}}}"));
        }
        let value = value.unwrap_or_default();
        let re = pattern_regex(&pattern, true, false).map_err(|e| format!("invalid pattern ({pattern}) in substitution: {e}"))?;
        if all {
            return Ok(re.replace_all(&value, replacement.as_str()).into_owned());
        }
        Ok(match re.find(&value) {
            Some(m) => format!("{}{replacement}{}", value.get(..m.start()).unwrap_or_default(), value.get(m.end()..).unwrap_or_default()),
            None => value,
        })
    }

    /// A variable's name: letters, digits and `_`; or a run of digits; or one special parameter.
    fn name(&mut self) -> String {
        let mut name = String::new();
        while let Some(ch) = self.peek() {
            if name.is_empty() && ch.is_numeric() {
                while let Some(d) = self.peek().filter(|c| c.is_numeric()) {
                    name.push(d);
                    self.next();
                }
                return name;
            }
            if name.is_empty() && matches!(ch, '@' | '*' | '#' | '?' | '-' | '$' | '!' | '0') {
                self.next();
                return ch.to_string();
            }
            if !ch.is_alphanumeric() && ch != '_' {
                break;
            }
            name.push(ch);
            self.next();
        }
        name
    }

    /// `<<` and the spaces after it stay one word, as a heredoc's opener.
    fn heredoc(&mut self) -> String {
        self.next();
        if self.peek() != Some('<') {
            return "<".into();
        }
        self.next();
        let mut out = String::from("<<");
        while let Some(c) = self.peek().filter(|c| matches!(c, '\t' | '\r' | ' ')) {
            out.push(c);
            self.next();
        }
        out
    }
}

/// A shell pattern (`*`, `?`, `\` escaping them) as a regular expression; `*` takes as little as
/// it can unless `greedy`.
fn pattern_regex(pattern: &str, greedy: bool, anchored: bool) -> Result<Regex, String> {
    let mut out = String::new();
    if anchored {
        out.push('^');
    }
    let star = if greedy { ".*" } else { ".*?" };
    let mut it = pattern.chars().peekable();
    while let Some(tok) = it.next() {
        match tok {
            '*' => {
                out.push_str(star);
                continue;
            }
            '?' => {
                out.push('.');
                continue;
            }
            '\\' => {
                // `}` and `/` are escaped only to end `${…}`'s parts.
                if matches!(it.peek(), Some('}' | '/')) {
                    continue;
                }
                out.push('\\');
                match it.next() {
                    Some(t @ ('*' | '?' | '\\')) => {
                        out.push(t);
                        continue;
                    }
                    Some(t) => return Err(format!("invalid escape '\\{t}'")),
                    None => return Err("invalid escape at the end".into()),
                }
            }
            '[' | ']' | '{' | '}' | '.' | '+' | '(' | ')' | '|' | '^' | '$' => out.push('\\'),
            _ => {}
        }
        out.push(tok);
    }
    Regex::new(&out).map_err(|e| e.to_string())
}

fn trim_prefix(pattern: &str, value: &str, greedy: bool) -> Result<String, String> {
    let re = pattern_regex(pattern, greedy, true).map_err(|e| format!("invalid pattern ({pattern}) in substitution: {e}"))?;
    Ok(match re.find(value) {
        Some(m) => value.get(m.end()..).unwrap_or_default().to_owned(),
        None => value.to_owned(),
    })
}

/// The shortest (or longest) rightmost match, found as the leftmost one of both reversed.
fn trim_suffix(pattern: &str, value: &str, greedy: bool) -> Result<String, String> {
    let reversed: String = value.chars().rev().collect();
    let trimmed = trim_prefix(&reverse_pattern(pattern), &reversed, greedy)?;
    Ok(trimmed.chars().rev().collect())
}

/// A pattern reversed, each escape kept before what it escapes.
fn reverse_pattern(pattern: &str) -> String {
    let chars: Vec<char> = pattern.chars().collect();
    let mut units = Vec::new();
    let mut k = 0;
    while let Some(&c) = chars.get(k) {
        match chars.get(k + 1) {
            Some(&n) if c == '\\' => {
                units.push(format!("\\{n}"));
                k += 2;
            }
            _ => {
                units.push(c.to_string());
                k += 1;
            }
        }
    }
    units.into_iter().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn env() -> BTreeMap<String, String> {
        BTreeMap::from([("A".to_owned(), "alpha".to_owned()), ("EMPTY".to_owned(), String::new()), ("P".to_owned(), "/usr/local/bin:/usr/bin".to_owned()), ("F".to_owned(), "archive.tar.gz".to_owned())])
    }

    fn word(w: &str) -> Result<String, String> {
        let e = env();
        Lex::new('\\').word(w, &|k| e.get(k).cloned())
    }

    #[test]
    fn w24_r29_words_expand_as_dockerfiles_define() -> anyhow::Result<()> {
        let cases = [
            ("$A", "alpha"),
            ("${A}", "alpha"),
            ("x${A}y", "xalphay"),
            ("$UNSET", ""),
            ("${UNSET:-dflt}", "dflt"),
            ("${EMPTY:-dflt}", "dflt"),
            ("${EMPTY-dflt}", ""),
            ("${A:+set}", "set"),
            ("${UNSET:+set}", ""),
            ("${EMPTY+set}", "set"),
            ("${EMPTY:+set}", ""),
            ("${F%.gz}", "archive.tar"),
            ("${F%%.*}", "archive"),
            ("${F#*.}", "tar.gz"),
            ("${F##*.}", "gz"),
            ("${P/bin/sbin}", "/usr/local/sbin:/usr/bin"),
            ("${P//bin/sbin}", "/usr/local/sbin:/usr/sbin"),
            ("'$A'", "$A"),
            ("\"$A b\"", "alpha b"),
            ("\\$A", "$A"),
            ("a\\ b", "a b"),
            ("\"a\\\"b\"", "a\"b"),
            ("$", "$"),
            ("${A:-${UNSET:-in}}", "alpha"),
            ("${UNSET:-${A}}", "alpha"),
        ];
        for (input, want) in cases {
            anyhow::ensure!(word(input).as_deref() == Ok(want), "{input:?} gave {:?}, not {want:?}", word(input));
        }
        Ok(())
    }

    #[test]
    fn w24_r29_bad_words_are_errors_not_panics() {
        for bad in ["${A", "${}", "${:x}", "${UNSET:?needed}", "${EMPTY:?}", "'open", "\"open", "${A:#x}", "${A!}", "${A/x"] {
            assert!(word(bad).is_err(), "{bad:?} gave {:?}", word(bad));
        }
        assert_eq!(word("${UNSET?}").err().map(|e| e.contains("not allowed to be unset")), Some(true));
        // Nesting deep enough to run a stack out is refused instead.
        let deep = "${A:-".repeat(100_000) + &"}".repeat(100_000);
        assert!(word(&deep).is_err_and(|e| e.contains("nest more than")));
        let fine = "${A:-".repeat(MAX_DEPTH - 1) + "x" + &"}".repeat(MAX_DEPTH - 1);
        assert_eq!(word(&fine).as_deref(), Ok("alpha"));
    }

    #[test]
    fn w24_r29_words_split_after_expansion() -> anyhow::Result<()> {
        let e = BTreeMap::from([("PORTS".to_owned(), "80 443/udp".to_owned())]);
        let words = Lex::new('\\').words("$PORTS \"8 9\" x", &|k| e.get(k).cloned()).map_err(anyhow::Error::msg)?;
        assert_eq!(words, ["80", "443/udp", "8 9", "x"]);
        let raw = Lex { skip_unset: true, raw_quotes: true, ..Lex::new('\\') }.word("'$X' ${Y:-z}", &|_| None).map_err(anyhow::Error::msg)?;
        assert_eq!(raw, "'$X' ${Y:-z}");
        Ok(())
    }
}
