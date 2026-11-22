//! Amphora: GROUP BY's groups in a hash table (Amphora's `docs/design/executor.md`, decision 4).
//!
//! A GROUP BY whose rows don't arrive in group order sorted every row and then aggregated the
//! sorted run of each group. Here each row is aggregated into its group's accumulators where it
//! arrives: the group is found by hashing its keys as the hash join and DISTINCT do (so keys that
//! compare equal hash alike: an integer and the equal real, text under its collation), and the
//! bytecode loads the group's accumulators into its registers for the row's aggregate steps and
//! saves them back. A group keeps the values its first row gave the query's non-aggregate
//! expressions (the GROUP BY keys first), as the sorter path keeps its first sorted row's. At the
//! end the groups are sorted in the order a sorter gives them and drained in that order.
//!
//! The table holds at most a budget of bytes: past it, a row of a group the table doesn't hold is
//! refused, and the bytecode sends the row to the GROUP BY sorter as before. A group is so wholly
//! in one or the other, and the sorter's groups are merged with the table's by key as they are
//! emitted, so the output stays in group order.

use crate::alloc::TryClone;
use crate::translate::collate::CollationSeq;
use crate::types::{compare_immutable, KeyInfo, Value, ValueRef};
use crate::vdbe::hash_table::{hash_join_key, keys_equal_distinct};
use crate::vdbe::Register;
use crate::{LimboError, Result};
use rustc_hash::FxHashMap;
use smallvec::SmallVec;
use std::cmp::Ordering;

/// Bytes a group costs beyond its values: the group, its vectors and its place in the index.
const GROUP_OVERHEAD: usize = 96;

/// A group: what its first row gave the non-aggregate expressions, and its accumulators.
struct Group {
    values: Vec<Value>,
    accs: Vec<Register>,
}

fn value_bytes(v: &Value) -> usize {
    let held = match v.as_ref() {
        ValueRef::Text(t) => t.as_str().len(),
        ValueRef::Blob(b) => b.len(),
        ValueRef::Null | ValueRef::Numeric(_) => 0,
    };
    held.saturating_add(std::mem::size_of::<Value>())
}

/// The groups of `num_values` values that are neither text nor blob and `num_accs` accumulators a
/// table of `budget` bytes holds, as `find` charges them: what the planner counts on grouping in
/// the table before the sorter takes the rest (`docs/design/planner.md` decision 7).
pub fn groups_in_budget(num_values: usize, num_accs: usize, budget: usize) -> usize {
    let per_group = num_values
        .saturating_mul(std::mem::size_of::<Value>())
        .saturating_add(num_accs.saturating_mul(std::mem::size_of::<Register>()))
        .saturating_add(GROUP_OVERHEAD);
    budget.checked_div(per_group).unwrap_or(0)
}

fn missing(what: &str) -> LimboError {
    LimboError::InternalError(format!("group table: {what}"))
}

/// One GROUP BY's groups.
pub struct GroupTable {
    /// Leading values that are the group's keys.
    num_keys: usize,
    collations: Vec<CollationSeq>,
    groups: Vec<Group>,
    by_hash: FxHashMap<u64, SmallVec<[u32; 2]>>,
    bytes: usize,
    budget: usize,
    /// The group the last find loaded, for its save.
    current: Option<u32>,
    /// The groups in output order, once sorted, and the next to drain.
    order: Vec<u32>,
    next: usize,
}

impl GroupTable {
    pub fn new(num_keys: usize, collations: Vec<CollationSeq>, budget: usize) -> Self {
        Self {
            num_keys,
            collations,
            groups: Vec::new(),
            by_hash: FxHashMap::default(),
            bytes: 0,
            budget,
            current: None,
            order: Vec::new(),
            next: 0,
        }
    }

    /// The group whose keys are the first `num_keys` of `values`, made from copies of them with
    /// `num_accs` empty accumulators if there is none; None if there is none and making it would
    /// pass the budget. The group found becomes the current one.
    pub fn find_or_insert(&mut self, values: &[&Value], num_accs: usize) -> Result<Option<u32>> {
        let keys: SmallVec<[ValueRef; 4]> = values
            .get(..self.num_keys)
            .ok_or_else(|| missing("fewer values than keys"))?
            .iter()
            .map(|v| v.as_ref())
            .collect();
        let hash = hash_join_key(&keys, &self.collations);
        if let Some(candidates) = self.by_hash.get(&hash) {
            for &g in candidates {
                let group = usize::try_from(g)
                    .ok()
                    .and_then(|i| self.groups.get(i))
                    .ok_or_else(|| missing("an indexed group is gone"))?;
                let held = group
                    .values
                    .get(..self.num_keys)
                    .ok_or_else(|| missing("a group with fewer values than keys"))?;
                if keys_equal_distinct(held, &keys, &self.collations) {
                    self.current = Some(g);
                    return Ok(Some(g));
                }
            }
        }
        let size = values
            .iter()
            .map(|v| value_bytes(v))
            .fold(GROUP_OVERHEAD, usize::saturating_add)
            .saturating_add(num_accs.saturating_mul(std::mem::size_of::<Register>()));
        if self.bytes.saturating_add(size) > self.budget {
            self.current = None;
            return Ok(None);
        }
        let g = u32::try_from(self.groups.len()).map_err(|_| LimboError::IntegerOverflow)?;
        let mut copied = Vec::new();
        copied.try_reserve_exact(values.len())?;
        for v in values {
            copied.push(v.try_clone()?);
        }
        let mut accs = Vec::new();
        accs.try_reserve_exact(num_accs)?;
        accs.resize_with(num_accs, || Register::Value(Value::Null));
        self.groups.try_reserve(1)?;
        self.groups.push(Group {
            values: copied,
            accs,
        });
        self.by_hash.entry(hash).or_default().push(g);
        self.bytes = self.bytes.saturating_add(size);
        self.current = Some(g);
        Ok(Some(g))
    }

    /// The current group's accumulators.
    pub fn current_accumulators(&mut self) -> Result<&mut [Register]> {
        let g = self.current.ok_or_else(|| missing("no current group"))?;
        let group = usize::try_from(g)
            .ok()
            .and_then(|i| self.groups.get_mut(i))
            .ok_or_else(|| missing("the current group is gone"))?;
        Ok(&mut group.accs)
    }

    /// Puts the groups in the order `key_info` gives their keys, a sorter's order, to be drained.
    pub fn sort(&mut self, key_info: &[KeyInfo]) -> Result<()> {
        let n = u32::try_from(self.groups.len()).map_err(|_| LimboError::IntegerOverflow)?;
        self.order.clear();
        self.order.try_reserve_exact(self.groups.len())?;
        self.order.extend(0..n);
        let (groups, num_keys) = (&self.groups, self.num_keys);
        let keys = |g: u32| {
            usize::try_from(g)
                .ok()
                .and_then(|i| groups.get(i))
                .and_then(|group| group.values.get(..num_keys))
                .unwrap_or_default()
        };
        self.order.sort_by(|&a, &b| {
            let (ka, kb) = (keys(a), keys(b));
            if ka.len() < key_info.len() || kb.len() < key_info.len() {
                return Ordering::Equal;
            }
            compare_immutable(ka.iter(), kb.iter(), key_info)
        });
        self.next = 0;
        Ok(())
    }

    /// The next group in sorted order whose keys order before `bound` (any, with no bound): its
    /// values, the result columns among them in `result_mask`'s places, and its accumulators,
    /// moved out of the table (each group is drained once). None once no such group is left.
    pub fn drain_next(
        &mut self,
        bound: Option<&[ValueRef]>,
        key_info: &[KeyInfo],
    ) -> Result<Option<(Vec<Value>, Vec<Register>)>> {
        let Some(&g) = self.order.get(self.next) else {
            return Ok(None);
        };
        let group = usize::try_from(g)
            .ok()
            .and_then(|i| self.groups.get_mut(i))
            .ok_or_else(|| missing("a sorted group is gone"))?;
        if let Some(bound) = bound {
            let keys = group
                .values
                .get(..self.num_keys)
                .ok_or_else(|| missing("a group with fewer values than keys"))?;
            if keys.len() < key_info.len() || bound.len() < key_info.len() {
                return Err(missing("fewer keys than the order compares"));
            }
            if compare_immutable(keys.iter(), bound.iter(), key_info) != Ordering::Less {
                return Ok(None);
            }
        }
        self.next = self.next.saturating_add(1);
        Ok(Some((
            std::mem::take(&mut group.values),
            std::mem::take(&mut group.accs),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use amphora_engine_parser::ast::SortOrder;

    fn int(i: i64) -> Value {
        Value::from_i64(i)
    }

    fn asc() -> Vec<KeyInfo> {
        vec![KeyInfo {
            sort_order: SortOrder::Asc,
            collation: CollationSeq::Binary,
            nulls_order: None,
        }]
    }

    fn drain_keys(t: &mut GroupTable, bound: Option<&Value>) -> Vec<Value> {
        let bound: Option<Vec<ValueRef>> = bound.map(|b| vec![b.as_ref()]);
        let mut out = Vec::new();
        while let Some((values, _)) = t.drain_next(bound.as_deref(), &asc()).unwrap() {
            out.push(values[0].clone());
        }
        out
    }

    /// Do: find groups for keys 3, 1, 3, 2, 1.0 and NULL twice, then sort and drain before 2,
    /// then the rest. Expect: one group per distinct key (1 and 1.0 one group, the NULLs one, as
    /// GROUP BY groups them), each found again for its later rows; drained NULL and 1 before 2,
    /// then 2 and 3.
    #[test]
    fn rows_find_their_groups_and_groups_drain_in_key_order() {
        let mut t = GroupTable::new(1, vec![CollationSeq::Binary], usize::MAX);
        let keys = [
            int(3),
            int(1),
            int(3),
            int(2),
            Value::from_f64(1.0),
            Value::Null,
            Value::Null,
        ];
        let found: Vec<u32> = keys
            .iter()
            .map(|k| t.find_or_insert(&[k], 1).unwrap().unwrap())
            .collect();
        assert_eq!(found, vec![0, 1, 0, 2, 1, 3, 3]);
        assert_eq!(t.groups.len(), 4);
        t.sort(&asc()).unwrap();
        assert_eq!(drain_keys(&mut t, Some(&int(2))), vec![Value::Null, int(1)]);
        assert_eq!(drain_keys(&mut t, None), vec![int(2), int(3)]);
    }

    /// Do: a budget for about two groups; find three keys, then the first again. Expect: the third
    /// refused (None), the first still found: past the budget only new groups are refused.
    #[test]
    fn past_its_budget_the_table_refuses_only_new_groups() {
        let one = GROUP_OVERHEAD + std::mem::size_of::<Value>() + std::mem::size_of::<Register>();
        let mut t = GroupTable::new(1, vec![CollationSeq::Binary], one * 2);
        assert_eq!(t.find_or_insert(&[&int(1)], 1).unwrap(), Some(0));
        assert_eq!(t.find_or_insert(&[&int(2)], 1).unwrap(), Some(1));
        assert_eq!(t.find_or_insert(&[&int(3)], 1).unwrap(), None);
        assert_eq!(t.find_or_insert(&[&int(1)], 1).unwrap(), Some(0));
    }
}
