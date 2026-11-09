//! A runner's own network rate (LB08), from its interfaces' byte counters, which every system
//! keeps and any user may read (Linux's `/proc/net/dev`, the `getifaddrs` counters on macOS,
//! `GetIfTable2` on Windows; the shell reads them through `sysinfo`): read at the runner's own
//! sample, so no probe and no timer of its own.
//!
//! Each reading's rate is what the busiest interface moved since the last reading, each way: the
//! busiest, not the sum, as a tunnel's bytes (Tailscale, a VPN) cross a physical interface too and
//! would count twice. The most it was ever seen to move is a lower bound on what it can (passive:
//! only what its own work moved), and holds until its interfaces change (another network, another
//! link), when it's learned again. What's free for a new run is that less what moves now.

use serde::{Deserialize, Serialize};

use super::rate::Access;

/// One interface's counters, bytes since it came up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Counters {
    pub name: String,
    pub rx: u64,
    pub tx: u64,
}

/// What a runner says of its network in its word: bytes a second now, the most seen, and its
/// link's speed where the system says.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Word {
    pub rx: u64,
    pub tx: u64,
    #[serde(flatten)]
    pub access: Access,
}

impl Word {
    /// What a new run may move here, each way, bytes a second: the most seen less what moves now.
    pub fn free(&self) -> (u64, u64) {
        (self.access.peak_rx.saturating_sub(self.rx), self.access.peak_tx.saturating_sub(self.tx))
    }
}

/// The counters' last reading and what they showed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Meter {
    last: Option<(i64, Vec<Counters>)>,
    pub word: Word,
}

impl Meter {
    /// A reading at `at` (ms, the runner's clock), sorted or not, and the link's speed if known.
    pub fn read(&mut self, at: i64, mut now: Vec<Counters>, link: Option<u64>) {
        now.sort_by(|a, b| a.name.cmp(&b.name));
        let Some((then, before)) = self.last.take() else {
            self.last = Some((at, now));
            self.word.access.link = link;
            return;
        };
        let gap = u64::try_from(at.saturating_sub(then)).unwrap_or(0);
        let names = |v: &[Counters]| v.iter().map(|c| c.name.clone()).collect::<Vec<_>>();
        if names(&before) != names(&now) || link != self.word.access.link {
            // Other interfaces (another network, another link): what it can do is learned again.
            self.word = Word { access: Access { link, ..Access::default() }, ..Word::default() };
            self.last = Some((at, now));
            return;
        }
        if gap > 0 {
            let (mut rx, mut tx) = (0u64, 0u64);
            for (b, n) in before.iter().zip(now.iter()) {
                // A counter that went back started again: no reading from it this time.
                let per_s = |from: u64, to: u64| to.checked_sub(from).map_or(0, |d| super::fixed::sat(u128::from(d) * 1000 / u128::from(gap)));
                rx = rx.max(per_s(b.rx, n.rx));
                tx = tx.max(per_s(b.tx, n.tx));
            }
            self.word.rx = rx;
            self.word.tx = tx;
            self.word.access.peak_rx = self.word.access.peak_rx.max(rx);
            self.word.access.peak_tx = self.word.access.peak_tx.max(tx);
        }
        self.last = Some((at, now));
    }
}
