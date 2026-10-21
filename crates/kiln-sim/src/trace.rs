//! What a run did, folded into one number (two runs of a seed fold alike, or they didn't run
//! alike), and, when asked, its last steps in words, for the report of a seed that failed.

use std::collections::VecDeque;

use crate::rng::{fnv, mix};

/// Most steps in words a trace keeps: past it, the oldest go.
pub const TAIL_MAX: usize = 10_000;

/// A run's digest, and its last steps in words.
#[derive(Debug, Clone)]
pub struct Trace {
    digest: u64,
    folds: u64,
    tail: VecDeque<String>,
    keep: usize,
}

impl Trace {
    /// A trace of a run of `seed`, keeping its last `keep` steps in words (none: 0, the cost of a
    /// bulk run; a replay of one seed keeps them).
    pub fn new(seed: u64, keep: usize) -> Self {
        Self { digest: mix(seed), folds: 0, tail: VecDeque::new(), keep: keep.min(TAIL_MAX) }
    }

    /// Folds `x` into the digest.
    pub fn fold(&mut self, x: u64) {
        self.digest = mix(self.digest.rotate_left(5) ^ x);
        self.folds = self.folds.saturating_add(1);
    }

    /// Folds `s` into the digest.
    pub fn fold_str(&mut self, s: &str) {
        self.fold(fnv(s.as_bytes()));
    }

    /// Notes a step in words, when the trace keeps them (`what` isn't called otherwise).
    pub fn note(&mut self, what: impl FnOnce() -> String) {
        if self.keep == 0 {
            return;
        }
        if self.tail.len() >= self.keep {
            self.tail.pop_front();
        }
        self.tail.push_back(what());
    }

    /// Whether it keeps steps in words.
    pub fn keeps(&self) -> bool {
        self.keep > 0
    }

    pub fn digest(&self) -> u64 {
        self.digest
    }

    /// How many things were folded.
    pub fn folds(&self) -> u64 {
        self.folds
    }

    /// Its last steps, oldest first.
    pub fn tail(&self) -> impl Iterator<Item = &String> {
        self.tail.iter()
    }

    /// Its last steps, one a line.
    pub fn tail_text(&self) -> String {
        self.tail.iter().map(String::as_str).collect::<Vec<&str>>().join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CP-T6.1: the same steps fold alike, others don't; words are kept only when asked, bounded.
    #[test]
    fn the_same_steps_fold_alike_and_words_are_kept_only_when_asked() {
        let (mut a, mut b, mut c) = (Trace::new(1, 0), Trace::new(1, 0), Trace::new(1, 2));
        for x in [3, 1, 4, 1, 5] {
            a.fold(x);
            b.fold(x);
        }
        assert_eq!(a.digest(), b.digest());
        b.fold(9);
        assert_ne!(a.digest(), b.digest());
        let mut asked = false;
        a.note(|| {
            asked = true;
            "x".into()
        });
        assert!(!asked && a.tail().next().is_none());
        for s in ["one", "two", "three"] {
            c.note(|| s.into());
        }
        assert_eq!(c.tail_text(), "two\nthree");
    }
}
