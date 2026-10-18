//! Masking (docs/ci.md W06-R08): secrets, `::add-mask::` values and every line of a multi-line
//! secret become `***` in logs, annotations, summaries, step names and outputs before anything
//! leaves the runner. Secrets of 8 characters or more are also masked base64-encoded (at each of
//! the three byte alignments), percent-encoded and JSON-escaped. Values shorter than 4 characters
//! are not masked (they would wreck logs); the secret's page warns.
//!
//! A stream is masked across chunk boundaries: [`Masker::push`] holds back up to the longest
//! mask's length minus one byte, so a match that could start in what it emits is always whole.

use aho_corasick::{AhoCorasick, MatchKind};
use base64::Engine;

/// The shortest value masked.
pub const MIN_MASK: usize = 4;
/// kiln's token prefixes (docs/ci.md W10-R03): a token is masked by its shape, even one no one
/// added as a secret.
pub const TOKEN_PREFIXES: &[&str] = &["kiln_pat_", "kiln_agt_", "kiln_job_"];
/// What follows a prefix, at least, to be taken for a token (an id, `_`, a secret).
const TOKEN_MIN: usize = 16;
/// The longest token a stream holds back while it may still be arriving.
const TOKEN_MAX: usize = 160;
/// The shortest secret whose encoded forms are masked too.
pub const MIN_ENCODED: usize = 8;

/// The forms of `value` to mask.
pub fn forms(value: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |s: String| {
        if s.len() >= MIN_MASK && !out.contains(&s) {
            out.push(s);
        }
    };
    push(value.to_owned());
    for line in value.lines() {
        push(line.trim_end_matches('\r').to_owned());
    }
    if value.len() >= MIN_ENCODED {
        let std = base64::engine::general_purpose::STANDARD;
        let bytes = value.as_bytes();
        // Encoded at offset 0, 1 and 2 of a 3-byte group: keep only the characters every byte of
        // which comes from the value, so the form matches inside any longer encoded text.
        for shift in 0..3usize {
            let mut b = vec![0u8; shift];
            b.extend_from_slice(bytes);
            let enc = std.encode(&b);
            let skip = [0usize, 2, 3].get(shift).copied().unwrap_or(0);
            let whole = (b.len() / 3) * 4;
            if let Some(core) = enc.get(skip..whole)
                && core.len() >= MIN_ENCODED
            {
                push(core.to_owned());
            }
            if shift == 0 {
                push(enc);
            }
        }
        push(percent(value));
        if let Ok(j) = serde_json::to_string(value)
            && let Some(inner) = j.get(1..j.len().saturating_sub(1))
        {
            push(inner.to_owned());
        }
    }
    out
}

/// `encodeURIComponent`'s encoding.
fn percent(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// Where the token starting at `i` ends, if one does.
fn token_at(s: &[u8], i: usize) -> Option<usize> {
    let rest = s.get(i..)?;
    let p = TOKEN_PREFIXES.iter().find(|p| rest.starts_with(p.as_bytes()))?;
    let body = rest.get(p.len()..)?.iter().take_while(|b| token_char(**b)).count();
    (body >= TOKEN_MIN).then_some(i + p.len() + body)
}

/// `s` with every kiln token in it masked.
fn mask_tokens(s: Vec<u8>) -> Vec<u8> {
    if !s.windows(5).any(|w| w == b"kiln_") {
        return s;
    }
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while let Some(b) = s.get(i).copied() {
        match token_at(&s, i) {
            Some(end) => {
                out.extend_from_slice(b"***");
                i = end;
            }
            None => {
                out.push(b);
                i += 1;
            }
        }
    }
    out
}

/// `safe`, moved back to the start of a token it would cut: a prefix (or, at the end of `held`,
/// the start of one) whose token characters reach `safe`.
fn token_safe(held: &[u8], safe: usize) -> usize {
    let before = held.get(..safe).unwrap_or_default();
    let start = safe - before.iter().rev().take(TOKEN_MAX + 9).take_while(|b| token_char(**b)).count();
    (start..safe)
        .find(|&p| {
            let rest = held.get(p..).unwrap_or_default();
            TOKEN_PREFIXES.iter().any(|t| rest.starts_with(t.as_bytes()) || t.as_bytes().starts_with(rest))
        })
        .unwrap_or(safe)
}

/// A masker over a stream.
#[derive(Debug, Clone, Default)]
pub struct Masker {
    masks: Vec<String>,
    ac: Option<AhoCorasick>,
    longest: usize,
    held: Vec<u8>,
}

impl Masker {
    pub fn new<S: AsRef<str>>(values: &[S]) -> Masker {
        let mut m = Masker::default();
        for v in values {
            m.add_forms(v.as_ref());
        }
        m.rebuild();
        m
    }

    fn add_forms(&mut self, value: &str) {
        for f in forms(value) {
            if !self.masks.contains(&f) {
                self.masks.push(f);
            }
        }
    }

    fn rebuild(&mut self) {
        self.longest = self.masks.iter().map(String::len).max().unwrap_or(0);
        self.ac = if self.masks.is_empty() { None } else { AhoCorasick::builder().match_kind(MatchKind::LeftmostLongest).build(&self.masks).ok() };
    }

    /// Masks a value from now on (`::add-mask::`, a secret delivered late). Returns false when it
    /// is too short to mask.
    pub fn add(&mut self, value: &str) -> bool {
        if value.len() < MIN_MASK {
            return false;
        }
        self.add_forms(value);
        self.rebuild();
        true
    }

    /// The masked part of the stream so far.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.held.extend_from_slice(chunk);
        let keep = self.longest.saturating_sub(1);
        // A token is never cut: it is held back whole while it may still be arriving.
        let safe = token_safe(&self.held, self.held.len().saturating_sub(keep));
        self.emit(safe)
    }

    /// The rest of the stream, masked.
    pub fn finish(&mut self) -> Vec<u8> {
        let all = self.held.len();
        self.emit(all)
    }

    fn emit(&mut self, safe: usize) -> Vec<u8> {
        let Some(ac) = &self.ac else {
            let cut = safe.min(self.held.len());
            return mask_tokens(self.held.drain(..cut).collect());
        };
        let mut out = Vec::with_capacity(safe);
        let mut last = 0usize;
        for m in ac.find_iter(&self.held) {
            if m.start() >= safe {
                break;
            }
            out.extend_from_slice(self.held.get(last..m.start()).unwrap_or_default());
            out.extend_from_slice(b"***");
            last = m.end();
        }
        let cut = last.max(safe).min(self.held.len());
        out.extend_from_slice(self.held.get(last..cut).unwrap_or_default());
        self.held.drain(..cut);
        mask_tokens(out)
    }

    /// A whole string, masked (annotations, outputs, step names).
    pub fn mask(&self, s: &str) -> String {
        let masked = match &self.ac {
            Some(ac) => ac.replace_all(s, &vec!["***"; self.masks.len()]),
            None => s.to_owned(),
        };
        // Tokens are ASCII: replacing them keeps the text UTF-8.
        String::from_utf8(mask_tokens(masked.into_bytes())).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forms_and_lengths() {
        let f = forms("hunter2-long-secret");
        let b64 = base64::engine::general_purpose::STANDARD.encode("hunter2-long-secret");
        assert!(f.contains(&b64));
        assert!(forms("pass word/secret").contains(&"pass%20word%2Fsecret".to_owned()), "percent-encoded");
        assert!(forms("abc").is_empty(), "too short to mask");
        assert_eq!(forms("abcd"), vec!["abcd"]);
        assert!(forms("line one\nline two").contains(&"line two".to_owned()));
        assert!(forms("quo\"te-secret").contains(&"quo\\\"te-secret".to_owned()));
    }

    /// W10-R03: kiln's tokens are masked by their prefix, whole, across any split of the stream.
    #[test]
    fn w10_r03_tokens_are_masked_by_their_prefix() {
        let pat = "kiln_pat_0192f0c1d2e3f4a5b6c7d8e9f0a1b2c3_AbC-dEf_ghIJKLmnopQRSTuvwxyz0123456789abcd";
        let text = format!("curl -H 'Authorization: Bearer {pat}' and kiln_job_short end é");
        let m = Masker::new::<&str>(&[]);
        assert_eq!(m.mask(&text), "curl -H 'Authorization: Bearer ***' and kiln_job_short end é", "a short look-alike stays");
        for split in 0..=text.len() {
            let mut m = Masker::new(&["unrelated-secret"]);
            let mut out = m.push(text.as_bytes().get(..split).unwrap_or_default());
            out.extend(m.push(text.as_bytes().get(split..).unwrap_or_default()));
            out.extend(m.finish());
            let out = String::from_utf8_lossy(&out);
            assert!(!out.contains("0192f0c1") && !out.contains("abcd'") && out.contains("Bearer ***'"), "split at {split}: {out}");
        }
        // Nothing waits that can't be a token.
        let mut m = Masker::new::<&str>(&[]);
        assert_eq!(m.push(b"plain line\n"), b"plain line\n");
        assert_eq!(m.push(b"held kil"), b"held ");
        assert_eq!(m.push(b"ns are fine"), b"kilns are fine");
    }

    /// W06-S03: a secret printed in base64 is masked.
    #[test]
    fn w06_s03_base64() {
        let m = Masker::new(&["hunter2-long-secret"]);
        let printed = base64::engine::general_purpose::STANDARD.encode("hunter2-long-secret");
        assert_eq!(m.mask(&format!("value: {printed}")), "value: ***");
        // Inside a longer encoded text, at any alignment.
        for prefix in ["", "x", "xy"] {
            let enc = base64::engine::general_purpose::STANDARD.encode(format!("{prefix}hunter2-long-secret and more"));
            assert!(!m.mask(&enc).contains(&enc), "{prefix:?}");
            assert!(m.mask(&enc).contains("***"));
        }
    }

    /// W06-AC03: split at every byte position, no secret leaves.
    #[test]
    fn w06_ac03_split_everywhere() {
        let secrets = ["s3cr3t-value", "another!secret", "abcd"];
        let text = "start s3cr3t-value mid another!secret abcd abc end s3cr3t-value";
        for split in 0..=text.len() {
            for second in split..=text.len() {
                let mut m = Masker::new(&secrets);
                let mut out = Vec::new();
                out.extend(m.push(text.as_bytes().get(..split).unwrap_or_default()));
                out.extend(m.push(text.as_bytes().get(split..second).unwrap_or_default()));
                out.extend(m.push(text.as_bytes().get(second..).unwrap_or_default()));
                out.extend(m.finish());
                let s = String::from_utf8_lossy(&out);
                assert!(!s.contains("s3cr3t") && !s.contains("another!") && !s.contains("abcd"), "split {split}/{second}: {s}");
                assert_eq!(s, "start *** mid *** *** abc end ***");
            }
        }
    }

    #[test]
    fn masks_added_later_and_none() {
        let mut m = Masker::new::<&str>(&[]);
        assert_eq!(m.push(b"plain"), b"plain");
        assert!(!m.add("ab"));
        assert!(m.add("late-mask"));
        let mut out = m.push(b"see late-mask here");
        out.extend(m.finish());
        assert_eq!(out, b"see *** here");
    }
}
