//! W05-R23: owner runs keep the generation inside the 24-byte record. A free record's runner
//! field holds its free-list link; the generation is even then, so no live handle can read it.
//! This retains Slab's full generation and last-freed-first reuse, without another slot header.

use super::{Handle, NIL, Run};

impl Run {
    #[inline(always)]
    pub(super) fn new(cpu: u64, sig: u32, runner: u32, state: u32, slow: u32) -> Self {
        Self { body: u64::from(super::narrow(cpu)) | (u64::from(sig) << u32::BITS), generation: 0, runner, state, slow }
    }

    /// CPU/signature are live only before STARTED. Admission consumes inflight demand
    /// before replacing that word, and every read of the admitted handle checks STARTED.
    #[inline(always)]
    pub(super) fn cpu(&self) -> u32 { self.body as u32 }

    #[inline(always)]
    pub(super) fn sig(&self) -> u32 { (self.body >> u32::BITS) as u32 }

    #[inline(always)]
    pub(super) fn at(&self) -> Handle {
        Handle { slot: self.body as u32, generation: (self.body >> u32::BITS) as u32 }
    }

    #[inline(always)]
    pub(super) fn started_at(&mut self, at: Handle) {
        self.body = u64::from(at.slot) | (u64::from(at.generation) << u32::BITS);
        self.state = super::STARTED;
        self.slow = NIL;
    }

    pub(super) fn needs_slow(need: &super::Amounts) -> bool {
        super::multi(need) || need[0] > u64::from(u32::MAX)
    }
}

#[derive(Debug, Clone)]
pub(super) struct Runs {
    slots: Vec<Run>,
    free: u32,
}

impl Runs {
    pub(super) fn new() -> Self { Self::with_capacity(0) }

    pub(super) fn with_capacity(n: usize) -> Self {
        Self { slots: Vec::with_capacity(n), free: NIL }
    }

    #[inline(always)]
    pub(super) fn insert(&mut self, mut val: Run) -> Option<Handle> {
        let slot = self.free;
        if let Some(s) = self.slots.get_mut(slot as usize) {
            self.free = s.runner;
            val.generation = s.generation.wrapping_add(1);
            *s = val;
            return Some(Handle { slot, generation: val.generation });
        }
        let slot = u32::try_from(self.slots.len()).ok().filter(|s| *s != NIL)?;
        val.generation = 1;
        self.slots.push(val);
        Some(Handle { slot, generation: 1 })
    }

    #[inline]
    fn matches(s: &Run, h: Handle) -> bool {
        #[cfg(test)]
        if crate::lb::planted(crate::lb::Mutant::NoGeneration) { return s.generation & 1 == 1; }
        s.generation == h.generation && h.generation & 1 == 1
    }

    #[inline]
    pub(super) fn get(&self, h: Handle) -> Option<&Run> {
        self.slots.get(h.slot as usize).filter(|s| Self::matches(s, h))
    }

    #[inline]
    pub(super) fn get_mut(&mut self, h: Handle) -> Option<&mut Run> {
        self.slots.get_mut(h.slot as usize).filter(|s| Self::matches(s, h))
    }

    #[inline]
    pub(super) fn remove(&mut self, h: Handle) -> Option<Run> { self.take_if(h, |_| true) }

    #[inline(always)]
    pub(super) fn take_if(&mut self, h: Handle, keep: impl FnOnce(&Run) -> bool) -> Option<Run> {
        let s = self.slots.get_mut(h.slot as usize).filter(|s| Self::matches(s, h))?;
        if !keep(s) { return None; }
        let val = *s;
        s.generation = s.generation.wrapping_add(1);
        s.runner = self.free;
        self.free = h.slot;
        Some(val)
    }

    #[inline]
    pub(super) fn at(&self, slot: u32) -> Option<&Run> {
        self.slots.get(slot as usize).filter(|s| s.generation & 1 == 1)
    }

    #[inline]
    pub(super) fn handle(&self, slot: u32) -> Option<Handle> {
        self.at(slot).map(|s| Handle { slot, generation: s.generation })
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = (Handle, &Run)> {
        self.slots.iter().zip(0u32..).filter(|(s, _)| s.generation & 1 == 1).map(|(s, slot)| (Handle { slot, generation: s.generation }, s))
    }

    pub(super) fn handles(&self) -> Vec<Handle> { self.iter().map(|(h, _)| h).collect() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lb::slab::Slab;

    /// W05-R23: each live phase retains both complete u32 values. The physical record
    /// holds the pair plus generation, runner, state and slow-slot words, with no extra tag.
    #[test]
    fn w05_r23_run_live_fields_keep_every_bit_through_admission() -> Result<(), String> {
        if std::mem::size_of::<Run>() != std::mem::size_of::<u64>() + 4 * std::mem::size_of::<u32>() {
            return Err("owner slot has unexpected padding or metadata".into());
        }
        for cpu in [0, 1, u32::MAX - 1, u32::MAX] {
            for sig in [0, 1, u32::MAX - 1, u32::MAX] {
                let mut run = Run::new(u64::from(cpu), sig, 17, super::super::STARTING, 19);
                if run.cpu() != cpu || run.sig() != sig { return Err("before-admission bits lost".into()); }
                for at in [Handle::NONE, Handle { slot: 0, generation: 1 }, Handle { slot: u32::MAX - 1, generation: u32::MAX }] {
                    run.started_at(at);
                    if run.at() != at || run.state != super::super::STARTED || run.slow != NIL {
                        return Err("admitted handle bits lost".into());
                    }
                }
            }
        }
        Ok(())
    }

    /// W05-R23: compare slot reuse, stale/invalid reads and conditional removals with the
    /// original generational slab through a mixed trace, including retained stale handles.
    #[test]
    fn w05_r23_compact_runs_preserve_generational_slab_lifetimes() -> Result<(), String> {
        let mut old = Slab::new();
        let mut new = Runs::new();
        let mut handles = vec![Handle::NONE, Handle { slot: 0, generation: 0 }];
        let mut rng = crate::lb::model::Rng(73);
        for i in 0..4096u32 {
            if rng.below(3) == 0 {
                let val = Run::new(u64::from(i), i, i, super::super::STARTING, NIL);
                let before = old.insert(val).ok_or("old full")?;
                let after = new.insert(val).ok_or("new full")?;
                if before != after { return Err(format!("insert {i}: {before:?} != {after:?}")); }
                old.get_mut(before).ok_or("old value")?.generation = before.generation;
                handles.push(before);
                handles.push(Handle { generation: before.generation.wrapping_add(1), ..before });
            } else {
                let h = *handles.get(rng.below(handles.len() as u64) as usize).ok_or("handle")?;
                if old.get(h) != new.get(h) { return Err(format!("read {i}: {h:?}")); }
                if i & 1 == 0 {
                    if old.take_if(h, |x| x.sig() & 1 == 0) != new.take_if(h, |x| x.sig() & 1 == 0) { return Err(format!("take {i}")); }
                } else if old.remove(h) != new.remove(h) { return Err(format!("remove {i}")); }
            }
            if old.iter().collect::<Vec<_>>() != new.iter().collect::<Vec<_>>() { return Err(format!("live values at {i}")); }
            if old.handles() != new.handles() { return Err(format!("live handles at {i}")); }
        }
        Ok(())
    }

    /// W05-R23: retain all generation bits and the same parity through the u32 wrap. Failed
    /// conditional removal must leave both the generation and the free-list order untouched.
    #[test]
    fn w05_r23_compact_runs_keep_generation_wrap_and_free_order() -> Result<(), String> {
        let mut runs = Runs::new();
        let first = runs.insert(Run::new(1, 2, 3, super::super::STARTING, NIL)).ok_or("insert")?;
        let second = runs.insert(Run::new(4, 5, 6, super::super::STARTING, NIL)).ok_or("insert")?;
        if runs.take_if(first, |_| false).is_some() || runs.get(first).is_none() { return Err("failed predicate removed a run".into()); }
        runs.get_mut(first).ok_or("first")?.generation = u32::MAX - 2;
        let old = Handle { generation: u32::MAX - 2, ..first };
        runs.remove(old).ok_or("remove before wrap")?;
        let high = runs.insert(Run::new(7, 8, 9, super::super::STARTING, NIL)).ok_or("insert high")?;
        if high != (Handle { generation: u32::MAX, ..first }) || runs.get(old).is_some() { return Err("generation narrowed".into()); }
        runs.remove(high).ok_or("remove at wrap")?;
        if runs.get(Handle { generation: 0, ..first }).is_some() { return Err("even generation matched".into()); }
        runs.remove(second).ok_or("remove second")?;
        let a = runs.insert(Run::new(10, 11, 12, super::super::STARTING, NIL)).ok_or("reuse second")?;
        let b = runs.insert(Run::new(13, 14, 15, super::super::STARTING, NIL)).ok_or("reuse wrapped")?;
        if a.slot != second.slot || a.generation != 3 || b != first || runs.get(high).is_some() { return Err("free-list order or generation wrap changed".into()); }
        Ok(())
    }

    /// W05-R23: Go's held-room handle does not replace before-admission demand or a
    /// non-default signature. A refused Go must wait at the same eligible runner for the
    /// original amount, rather than interpret the handle as CPU/signature.
    #[test]
    fn w05_r23_go_refusal_preserves_live_cpu_and_nondefault_signature() -> Result<(), String> {
        use super::super::*;
        let (mut owner, runners, _) = crate::lb::tests::fleet(2, [8 * CPU, 0, 0, 0]);
        let sig = owner.sig();
        let mut out = Out::default();
        owner.add(sig, RunnerId(1), &mut out);
        let mut word = runners.get(1).ok_or("runner")?.word();
        word.cpu = 0;
        owner.hear(ToOwner::Hint { runner: RunnerId(1), word }, &mut out);
        let need = [2 * CPU, 0, 0, 0];
        let ask = owner.ask(sig, need, &mut out).map_err(|e| format!("ask: {e:?}"))?;
        if ask.send.is_some() { return Err("expected Wait".into()); }
        out.to.clear();
        let at = Handle { slot: 17, generation: 9 };
        owner.hear(ToOwner::Ready { runner: RunnerId(1), run: ask.run, at, word }, &mut out);
        if owner.state(ask.run) != Some(State::Going { runner: RunnerId(1) })
            || !out.to.iter().any(|(r, m)| *r == RunnerId(1) && matches!(m, ToRunner::Go { at: h, .. } if *h == at)) {
            return Err("Ready did not carry its actual held handle into Go".into());
        }
        out.to.clear();
        owner.hear(ToOwner::Declined { runner: RunnerId(1), run: ask.run, word, why: Why::Room, of: Of::Go }, &mut out);
        if !out.to.iter().any(|(r, m)| *r == RunnerId(1) && matches!(m, ToRunner::Wait(s) if s.need == need))
            || out.to.iter().any(|(r, _)| *r != RunnerId(1)) {
            return Err(format!("Go refusal lost demand or signature: {:?}", out.to));
        }
        let run = owner.runs.get(ask.run).ok_or("run disappeared")?;
        if run.sig() != sig.0 || owner.need_of(run) != need { return Err("parked data changed".into()); }
        Ok(())
    }

    /// W05-R23: narrow CPU bookkeeping must not truncate the demand in a real wait/Go
    /// refusal/replacement sequence. Exercise both sides of u32 and the full u64 limit.
    #[test]
    fn w05_r23_compact_runs_preserve_full_cpu_demand_after_go_refusal() -> Result<(), String> {
        use super::super::*;
        for cpu in [u64::from(u32::MAX), u64::from(u32::MAX) + 1, u64::MAX] {
            let budget = [cpu, 9, 9, 1];
            let need = [cpu, 3, 4, 1];
            let (mut owner, runners, sig) = crate::lb::tests::fleet(1, budget);
            let mut out = Out::default();
            // Reserve all CPU in the advertised view, so even the u32 boundary takes Wait.
            let mut word = runners.first().ok_or("runner")?.word();
            word.cpu = 0;
            owner.hear(ToOwner::Hint { runner: RunnerId(0), word }, &mut out);
            let ask = owner.ask(sig, need, &mut out).map_err(|e| format!("ask: {e:?}"))?;
            if ask.send.is_some() || !out.to.iter().any(|(_, m)| matches!(m, ToRunner::Wait(s) if s.need == need)) { return Err(format!("initial wait lost demand {cpu}")); }
            out.to.clear();
            let at = Handle { slot: 17, generation: 1 };
            owner.hear(ToOwner::Ready { runner: RunnerId(0), run: ask.run, at, word }, &mut out);
            if !out.to.iter().any(|(_, m)| matches!(m, ToRunner::Go { run, .. } if *run == ask.run)) { return Err("no Go".into()); }
            out.to.clear();
            owner.hear(ToOwner::Declined { runner: RunnerId(0), run: ask.run, word, why: Why::Room, of: Of::Go }, &mut out);
            if !out.to.iter().any(|(_, m)| matches!(m, ToRunner::Wait(s) if s.need == need)) { return Err(format!("Go refusal truncated CPU {cpu}: {:?}", out.to)); }
            let x = owner.runs.get(ask.run).ok_or("run disappeared")?;
            if owner.need_of(x) != need { return Err("saved demand changed".into()); }
        }
        Ok(())
    }
}
