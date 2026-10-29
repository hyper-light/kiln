//! Keeping the hosts' pull requests mirrored within their rate limits (docs/ci.md W26-R20), the
//! pure part: when to read a repository's list of open pull requests and which pull requests to
//! read again. A webhook marks its pull request dirty, read once it has settled (a burst of
//! events is one read); the list is read on a cadence as the backstop for a missed webhook (each
//! minute, every five while webhooks come), with its ETag so an unchanged list costs nothing
//! (GitHub doesn't count a 304); a pull request the list shows changed (its `updated_at`) is read
//! again, one it no longer shows is gone. The caller does the reading, with the time passed in.

use std::collections::BTreeMap;

/// How often things are read, in ms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cadence {
    /// The list of open pull requests, at most this often.
    pub list_every: i64,
    /// The same while webhooks come (one within this long): they say what changed, and the list
    /// only catches one missed.
    pub list_every_hooked: i64,
    /// A pull request a webhook named waits this long for the rest of its burst.
    pub settle: i64,
}

impl Default for Cadence {
    fn default() -> Self {
        Self { list_every: 60_000, list_every_hooked: 300_000, settle: 10_000 }
    }
}

/// What to read now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Want {
    /// The repository's open pull requests.
    List,
    /// One pull request and what hangs off it (its commits, reviews and comments).
    Pull(u64),
}

/// One repository's mirror: when its list was last read, the pull requests to read again (each
/// since when), and each known one's `updated_at` as last read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Mirror {
    pub cadence: Cadence,
    listed_at: Option<i64>,
    hooked_at: Option<i64>,
    dirty: BTreeMap<u64, i64>,
    known: BTreeMap<u64, String>,
}

impl Mirror {
    pub fn new(cadence: Cadence) -> Self {
        Self { cadence, ..Self::default() }
    }

    /// A webhook about pull request `number` came at `now`.
    pub fn hook(&mut self, number: u64, now: i64) {
        self.dirty.entry(number).or_insert(now);
        self.hooked_at = Some(self.hooked_at.map_or(now, |at| at.max(now)));
    }

    /// What to read at `now`: the list when it's due (first), then each dirty pull request that
    /// has settled, oldest number first.
    pub fn due(&self, now: i64) -> Vec<Want> {
        let hooked = self.hooked_at.is_some_and(|at| now.saturating_sub(at) < self.cadence.list_every_hooked);
        let every = if hooked { self.cadence.list_every_hooked } else { self.cadence.list_every };
        let list = self.listed_at.is_none_or(|at| now.saturating_sub(at) >= every);
        let pulls = self.dirty.iter().filter(|(_, since)| now.saturating_sub(**since) >= self.cadence.settle).map(|(n, _)| Want::Pull(*n));
        list.then_some(Want::List).into_iter().chain(pulls).collect()
    }

    /// The list was read at `now`: `None` when it hadn't changed (a 304), else each open pull
    /// request and its `updated_at`. Those changed since last read are due at once; what it
    /// returns are the ones gone (merged or closed), forgotten here.
    pub fn listed(&mut self, now: i64, open: Option<&[(u64, String)]>) -> Vec<u64> {
        self.listed_at = Some(now);
        let Some(open) = open else { return Vec::new() };
        let ready = now.saturating_sub(self.cadence.settle);
        for (n, updated) in open {
            if self.known.get(n) != Some(updated) {
                let since = self.dirty.entry(*n).or_insert(ready);
                *since = (*since).min(ready);
            }
        }
        let gone: Vec<u64> = self.known.keys().filter(|n| !open.iter().any(|(m, _)| m == *n)).copied().collect();
        for n in &gone {
            self.known.remove(n);
            self.dirty.remove(n);
        }
        gone
    }

    /// Pull request `number` was read: its `updated_at`, or `None` when it's gone.
    pub fn fetched(&mut self, number: u64, updated: Option<String>) {
        self.dirty.remove(&number);
        match updated {
            Some(u) => {
                self.known.insert(number, u);
            }
            None => {
                self.known.remove(&number);
            }
        }
    }

    /// How many pull requests are known.
    pub fn known(&self) -> usize {
        self.known.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: i64 = 1000;

    fn open(xs: &[(u64, &str)]) -> Vec<(u64, String)> {
        xs.iter().map(|(n, u)| (*n, u.to_string())).collect()
    }

    #[test]
    fn w26_r20_the_list_first_then_each_pull_request_it_shows() {
        let mut m = Mirror::default();
        assert_eq!(m.due(0), vec![Want::List], "nothing known: the list");
        assert!(m.listed(0, Some(&open(&[(7, "a"), (9, "a")]))).is_empty());
        assert_eq!(m.due(0), vec![Want::Pull(7), Want::Pull(9)], "every one it shows, at once");
        m.fetched(7, Some("a".into()));
        m.fetched(9, Some("a".into()));
        assert!(m.due(30 * S).is_empty(), "nothing until the list is due again");
        assert_eq!(m.due(60 * S), vec![Want::List]);
    }

    #[test]
    fn w26_r20_an_unchanged_list_reads_nothing_more() {
        let mut m = Mirror::default();
        m.listed(0, Some(&open(&[(7, "a")])));
        m.fetched(7, Some("a".into()));
        assert!(m.listed(60 * S, None).is_empty());
        assert!(m.due(60 * S).is_empty(), "a 304: nothing to read");
        m.listed(120 * S, Some(&open(&[(7, "a")])));
        assert!(m.due(120 * S).is_empty(), "the same updated_at: nothing to read");
    }

    #[test]
    fn w26_r20_a_burst_of_webhooks_is_one_read_once_settled() {
        let mut m = Mirror::default();
        m.listed(0, Some(&[]));
        for t in [1, 2, 3, 5, 8] {
            m.hook(7, t * S);
        }
        assert!(m.due(10 * S).is_empty(), "still settling");
        assert_eq!(m.due(11 * S), vec![Want::Pull(7)]);
        m.fetched(7, Some("b".into()));
        assert!(m.due(12 * S).is_empty());
    }

    #[test]
    fn w26_r20_the_list_catches_a_missed_webhook_and_forgets_the_gone() {
        let mut m = Mirror::default();
        m.listed(0, Some(&open(&[(7, "a"), (9, "a")])));
        m.fetched(7, Some("a".into()));
        m.fetched(9, Some("a".into()));
        let gone = m.listed(60 * S, Some(&open(&[(7, "b")])));
        assert_eq!(gone, vec![9], "9 merged or closed");
        assert_eq!(m.due(60 * S), vec![Want::Pull(7)], "7 changed with no webhook");
        assert_eq!(m.known(), 1);
    }

    #[test]
    fn w26_r20_while_webhooks_come_the_list_is_read_every_five_minutes() {
        let mut m = Mirror::default();
        m.listed(0, Some(&[]));
        m.hook(7, 30 * S);
        m.fetched(7, Some("a".into()));
        assert!(m.due(60 * S).is_empty(), "a webhook a moment ago: the list waits");
        assert_eq!(m.due(300 * S), vec![Want::List]);
        m.listed(300 * S, None);
        assert_eq!(m.due(360 * S), vec![Want::List], "no webhook for five minutes: each minute again");
    }

    #[test]
    fn w26_r20_a_pull_request_read_after_its_webhook_isnt_read_again_for_the_list() {
        let mut m = Mirror::default();
        m.listed(0, Some(&open(&[(7, "a")])));
        m.fetched(7, Some("a".into()));
        m.hook(7, 20 * S);
        m.fetched(7, Some("b".into()));
        m.listed(60 * S, Some(&open(&[(7, "b")])));
        assert!(m.due(60 * S).is_empty(), "the list shows what was read");
    }
}
