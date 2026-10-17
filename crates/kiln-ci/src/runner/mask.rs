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
        let safe = self.held.len().saturating_sub(keep);
        self.emit(safe)
    }

    /// The rest of the stream, masked.
    pub fn finish(&mut self) -> Vec<u8> {
        let all = self.held.len();
        self.emit(all)
    }

    fn emit(&mut self, safe: usize) -> Vec<u8> {
        let Some(ac) = &self.ac else { return std::mem::take(&mut self.held) };
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
        out
    }

    /// A whole string, masked (annotations, outputs, step names).
    pub fn mask(&self, s: &str) -> String {
        match &self.ac {
            Some(ac) => ac.replace_all(s, &vec!["***"; self.masks.len()]),
            None => s.to_owned(),
        }
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
