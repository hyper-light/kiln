//! What's due when: a model's events, each at a time of the simulation's (since it started), taken
//! earliest first, events due together in the order they were put in.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};
use std::time::Duration;

/// Most events a queue holds by default: past it, one more is refused (counted), never dropped
/// unsaid.
pub const QUEUE_MAX: usize = 1_000_000;

/// Events by when they're due.
#[derive(Debug)]
pub struct Queue<E> {
    heap: BinaryHeap<Reverse<(Duration, u64)>>,
    events: BTreeMap<u64, E>,
    seq: u64,
    max: usize,
    refused: u64,
}

impl<E> Default for Queue<E> {
    fn default() -> Self {
        Self::new()
    }
}

impl<E> Queue<E> {
    pub fn new() -> Self {
        Self::with_max(QUEUE_MAX)
    }

    /// A queue of at most `max` events.
    pub fn with_max(max: usize) -> Self {
        Self { heap: BinaryHeap::new(), events: BTreeMap::new(), seq: 0, max, refused: 0 }
    }

    /// `e`, due at `at`: false (and counted) when the queue is full.
    pub fn push(&mut self, at: Duration, e: E) -> bool {
        if self.events.len() >= self.max {
            self.refused = self.refused.saturating_add(1);
            return false;
        }
        self.seq = self.seq.saturating_add(1);
        self.heap.push(Reverse((at, self.seq)));
        self.events.insert(self.seq, e);
        true
    }

    /// When the next event is due.
    pub fn next_at(&self) -> Option<Duration> {
        self.heap.peek().map(|Reverse((at, _))| *at)
    }

    /// The next event, and when it's due.
    pub fn pop(&mut self) -> Option<(Duration, E)> {
        while let Some(Reverse((at, seq))) = self.heap.pop() {
            if let Some(e) = self.events.remove(&seq) {
                return Some((at, e));
            }
        }
        None
    }

    /// The next event, if it's due by `now`.
    pub fn pop_due(&mut self, now: Duration) -> Option<(Duration, E)> {
        if self.next_at().is_some_and(|at| at <= now) { self.pop() } else { None }
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Events refused because the queue was full.
    pub fn refused(&self) -> u64 {
        self.refused
    }

    /// Drops every event `drop` says, keeping the rest in order (a node that crashed loses what
    /// it was due to do).
    pub fn retain(&mut self, mut keep: impl FnMut(&E) -> bool) {
        self.events.retain(|_, e| keep(e));
        let events = &self.events;
        self.heap.retain(|Reverse((_, seq))| events.contains_key(seq));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CP-T6.1, AC-T6.1.4: earliest first, ties in the order put in, bounded and saying so.
    #[test]
    fn events_come_earliest_first_ties_in_order_and_a_full_queue_refuses() {
        let mut q = Queue::with_max(4);
        let ms = Duration::from_millis;
        assert!(q.push(ms(5), "c") && q.push(ms(1), "a") && q.push(ms(5), "d") && q.push(ms(2), "b"));
        assert!(!q.push(ms(0), "e"));
        assert_eq!(q.refused(), 1);
        assert_eq!(q.pop_due(ms(0)), None);
        let order: Vec<&str> = std::iter::from_fn(|| q.pop().map(|(_, e)| e)).collect();
        assert_eq!(order, ["a", "b", "c", "d"]);
        let mut q = Queue::new();
        for (t, e) in [(3, 30), (1, 10), (2, 20)] {
            q.push(ms(t), e);
        }
        q.retain(|e| *e != 10);
        assert_eq!(q.pop(), Some((ms(2), 20)));
        assert_eq!(q.len(), 1);
    }
}
