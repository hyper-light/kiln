//! A search over partitions (CP-T1.10, AC-T1.10.4; docs/design/replication.md, "Search over
//! partitions"): each partition asked at once, on a thread of its own, and their answers merged
//! newest first. A partition is whatever answers for its share of the rows: a group of a
//! [`crate::groups::Groups`] store, or a replicated group's replica on this node, read at its
//! closed time ([`crate::database::Database::query_closed`]). A pure core with a thread shell: the
//! caller says how to ask one partition and how its rows order.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

/// `ask` on every partition at once: the first on the caller's thread, each other on a scoped
/// thread of its own, so a search takes about its slowest partition's time, not their sum. One
/// answer a partition, in partition order; the first partition's error if any failed. Threads are
/// bounded by the partitions the caller holds, one each.
pub fn fan_out<P, T, F>(partitions: &[P], ask: F) -> anyhow::Result<Vec<T>>
where
    P: Sync,
    T: Send,
    F: Fn(&P) -> anyhow::Result<T> + Sync,
{
    let Some((first, rest)) = partitions.split_first() else {
        return Ok(Vec::new());
    };
    std::thread::scope(|scope| {
        let ask = &ask;
        let mut spawned = Vec::with_capacity(rest.len());
        for partition in rest {
            spawned.push(
                std::thread::Builder::new()
                    .name("amphora.search".into())
                    .spawn_scoped(scope, move || ask(partition))?,
            );
        }
        let mut answers = Vec::with_capacity(partitions.len());
        answers.push(ask(first));
        for handle in spawned {
            answers.push(
                handle.join().unwrap_or_else(|_| {
                    Err(anyhow::anyhow!("a search partition's thread panicked"))
                }),
            );
        }
        answers.into_iter().collect()
    })
}

/// One row at the head of its partition's answer, ordered by its key, the earlier partition first
/// among equal keys, so a merge is the same every time.
struct Head<K, T> {
    key: K,
    partition: usize,
    row: T,
}
impl<K: Ord, T> PartialEq for Head<K, T> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl<K: Ord, T> Eq for Head<K, T> {}
impl<K: Ord, T> PartialOrd for Head<K, T> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl<K: Ord, T> Ord for Head<K, T> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.key
            .cmp(&other.key)
            .then_with(|| other.partition.cmp(&self.partition))
    }
}

/// The `limit` rows with the largest keys among `answers`, largest first: each partition's
/// answer already largest first and at most `limit` long, as `ORDER BY key DESC LIMIT limit`
/// gives it. A k-way merge: it holds one row a partition and takes at most `limit`.
pub fn newest<K: Ord, T>(answers: Vec<Vec<(K, T)>>, limit: usize) -> Vec<(K, T)> {
    let mut rows: Vec<std::vec::IntoIter<(K, T)>> =
        answers.into_iter().map(Vec::into_iter).collect();
    let mut heads = BinaryHeap::with_capacity(rows.len());
    for (partition, answer) in rows.iter_mut().enumerate() {
        if let Some((key, row)) = answer.next() {
            heads.push(Head {
                key,
                partition,
                row,
            });
        }
    }
    let mut merged = Vec::with_capacity(limit.min(heads.len().saturating_mul(limit)));
    while merged.len() < limit {
        let Some(head) = heads.pop() else {
            break;
        };
        if let Some((key, row)) = rows.get_mut(head.partition).and_then(Iterator::next) {
            heads.push(Head {
                key,
                partition: head.partition,
                row,
            });
        }
        merged.push((head.key, head.row));
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AC-T1.10.4: partitions' answers merge newest first to the limit, equal keys in partition
    /// order, whatever their lengths.
    #[test]
    fn answers_merge_newest_first_to_the_limit() {
        let answers = vec![
            vec![(9, "a9"), (5, "a5"), (1, "a1")],
            vec![],
            vec![(8, "c8"), (5, "c5"), (4, "c4"), (2, "c2")],
            vec![(10, "d10")],
        ];
        assert_eq!(
            newest(answers.clone(), 5),
            vec![(10, "d10"), (9, "a9"), (8, "c8"), (5, "a5"), (5, "c5")]
        );
        assert_eq!(newest(answers.clone(), 0), vec![]);
        assert_eq!(newest(answers, 100).len(), 8);
        assert_eq!(newest::<u64, ()>(Vec::new(), 3), vec![]);
    }

    /// AC-T1.10.4: the merge is the same as sorting every row and taking the limit, over many
    /// shapes (an oracle test).
    #[test]
    fn the_merge_is_the_sorted_union_cut_to_the_limit() {
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..500 {
            let partitions = usize::try_from(next() % 6).unwrap();
            let limit = usize::try_from(next() % 12).unwrap();
            let mut answers = Vec::new();
            let mut all = Vec::new();
            for p in 0..partitions {
                let mut keys: Vec<u64> = (0..next() % 15).map(|_| next() % 40).collect();
                keys.sort_unstable_by(|a, b| b.cmp(a));
                keys.truncate(limit);
                all.extend(keys.iter().map(|&k| (k, p)));
                answers.push(keys.into_iter().map(|k| (k, p)).collect::<Vec<_>>());
            }
            all.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
            all.truncate(limit);
            assert_eq!(newest(answers, limit), all);
        }
    }

    /// AC-T1.10.4: every partition is asked at once. Each ask waits until every ask has begun,
    /// which asked one after another the first never sees: it gives up after a counted hang
    /// guard (about a minute of waits) and fails the search, never a verdict on time.
    #[test]
    fn every_partition_is_asked_at_once() {
        const ROUNDS: usize = 60_000;
        let partitions: Vec<u64> = (0..6).collect();
        let begun = (std::sync::Mutex::new(0usize), std::sync::Condvar::new());
        let answers = fan_out(&partitions, |p| {
            let mut count = begun.0.lock().unwrap();
            *count += 1;
            begun.1.notify_all();
            for _ in 0..ROUNDS {
                if *count == partitions.len() {
                    return Ok(p * 10);
                }
                count = begun
                    .1
                    .wait_timeout(count, std::time::Duration::from_millis(1))
                    .unwrap()
                    .0;
            }
            anyhow::bail!("partition {p} was asked alone")
        })
        .unwrap();
        assert_eq!(answers, vec![0, 10, 20, 30, 40, 50]);
        let failed = fan_out(&partitions, |p| {
            if *p == 3 {
                anyhow::bail!("partition 3 refused")
            }
            Ok(*p)
        });
        assert_eq!(failed.unwrap_err().to_string(), "partition 3 refused");
        assert!(fan_out(&Vec::<u64>::new(), |p| Ok(*p)).unwrap().is_empty());
    }
}
