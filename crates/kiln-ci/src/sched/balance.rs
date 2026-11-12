//! The engine's load balancer (docs/lb.md "The CI engine"; docs/ci.md W05-R04, W05-R23): where a
//! leg goes is the placement kernel's pick ([`crate::lb`]), the same kernel that places commands
//! and agent sessions. The engine decides which leg goes next (its classes, fair share and
//! priorities, W05-R09, W05-R10) and which runners may take it (routing, W05-R22); the kernel
//! decides which of those it goes to: the most room by its rank (free cores, at the speed each
//! runner measured, docs/lb.md "Rank"), and between runners the rank can't tell apart, back to
//! where its kind of work last went well (docs/lb.md "Good places").
//!
//! The engine is the kernel's one owner of its legs: it tells the kernel each runner's whole
//! budget and room as its own rules count them ([`Balance::tell`]), each signature's members as
//! routing has them ([`Balance::sync`]), and how each leg's attempt ended ([`Balance::ended`]). A
//! leg's kind is the key its runner keys its runs of it by (its repository, workflow, job and
//! matrix: `sense::run::leg_key`), so the two name the same work alike.
//!
//! Pure, like the rest of the engine: runners, signatures and kinds are interned in the order
//! the engine first names them, so the same inputs give the same picks on every machine.

use std::collections::{BTreeSet, HashMap};

use crate::lb::{self, Amounts, KindId, Owner, OwnerId, RunnerId, SigId, Word};
use crate::place::msg::How;

/// A runner's measured pace, as its own kernel's words say it (docs/lb.md "Rank"): its per-core
/// speed and its slowness under the contention it measures now, each in 1/1024 of the reference
/// (0: not measured), and how many times what it measured changed. The kernel ranks free cores
/// at that speed, so of runners with as many free a faster one comes first (W05-R26).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Pace {
    pub speed: u32,
    pub slowness: u32,
    pub measured: u32,
}

/// What a pick asks of the kernel: nothing of any runner's room. Which runner takes a leg now is
/// the engine's own rules' to say, exactly ([`Balance::propose`]'s caller): the kernel ranks
/// every member by its free cores, and proposes them in that order.
const ANY_ROOM: Amounts = [0, 0, 0, 0];

/// The engine's kernel and what it has told it.
#[derive(Debug, Clone)]
pub(super) struct Balance {
    lb: Owner,
    out: lb::Out,
    /// Runners by the engine's id, interned to the kernel's dense ids in the order first told.
    ids: HashMap<String, u32>,
    names: Vec<String>,
    /// Each runner's budget and word as last told (`None`: not in the kernel now), and the epoch
    /// its next joining takes.
    told: Vec<Option<(Amounts, Word)>>,
    epochs: Vec<u32>,
    /// Each signature: the kernel's, its members as last synced, and the engine's fleet
    /// generation they were synced at.
    sigs: HashMap<String, Sig>,
    /// Each kind of leg, interned to the kernel's dense ids.
    kinds: HashMap<String, KindId>,
}

#[derive(Debug, Clone)]
struct Sig {
    id: SigId,
    members: BTreeSet<u32>,
    synced: Option<u64>,
}

impl Default for Balance {
    fn default() -> Self {
        Balance { lb: Owner::new(OwnerId(0)), out: lb::Out::default(), ids: HashMap::new(), names: Vec::new(), told: Vec::new(), epochs: Vec::new(), sigs: HashMap::new(), kinds: HashMap::new() }
    }
}

impl Balance {
    fn id(&mut self, runner: &str) -> u32 {
        if let Some(r) = self.ids.get(runner) {
            return *r;
        }
        let r = u32::try_from(self.names.len()).unwrap_or(u32::MAX);
        self.ids.insert(runner.to_owned(), r);
        self.names.push(runner.to_owned());
        self.told.push(None);
        self.epochs.push(1);
        r
    }

    fn name(&self, r: RunnerId) -> Option<&str> {
        self.names.get(r.0 as usize).map(String::as_str)
    }

    /// Runner `runner`'s whole budget and its room now, in the kernel's amounts: it joins the
    /// kernel at its first word (or its first since it went), its budget and room followed after
    /// that. Nothing is done for a word it was told already.
    pub(super) fn tell(&mut self, runner: &str, budget: Amounts, word: Word) {
        let r = self.id(runner);
        let i = r as usize;
        let epoch = self.epochs.get(i).copied().unwrap_or(1);
        let word = Word { epoch, ..word };
        let was = self.told.get(i).copied().flatten();
        self.out.clear();
        match was {
            None => self.lb.join(RunnerId(r), budget, word, &mut self.out),
            Some((b, w)) => {
                if b != budget {
                    self.lb.set_budget(RunnerId(r), budget);
                }
                if w != word {
                    let _ = self.lb.hear(lb::ToOwner::Hint { runner: RunnerId(r), word }, &mut self.out);
                }
            }
        }
        self.out.clear();
        if let Some(t) = self.told.get_mut(i) {
            *t = Some((budget, word));
        }
    }

    /// Runner `runner` is gone (its link lost): out of the kernel, every signature included,
    /// until it's told again, under its next epoch.
    pub(super) fn gone(&mut self, runner: &str) {
        let Some(&r) = self.ids.get(runner) else { return };
        let i = r as usize;
        if self.told.get(i).copied().flatten().is_none() {
            return;
        }
        self.out.clear();
        self.lb.leave(RunnerId(r), &mut self.out);
        self.out.clear();
        if let Some(t) = self.told.get_mut(i) {
            *t = None;
        }
        if let Some(e) = self.epochs.get_mut(i) {
            *e = e.saturating_add(1);
        }
        for s in self.sigs.values_mut() {
            s.members.remove(&r);
        }
    }

    /// Signature `sig`'s members, as the engine's records stand at fleet generation `generation`:
    /// those `members` names that the kernel was told of (W05-R04). Nothing is looked at when
    /// they were synced at that generation already.
    pub(super) fn sync<'a>(&mut self, sig: &str, generation: u64, members: impl Iterator<Item = &'a String>) {
        if !self.sigs.contains_key(sig) {
            let id = self.lb.sig();
            self.sigs.insert(sig.to_owned(), Sig { id, members: BTreeSet::new(), synced: None });
        }
        if self.sigs.get(sig).is_some_and(|s| s.synced == Some(generation)) {
            return;
        }
        let want: BTreeSet<u32> = members.filter_map(|m| self.ids.get(m.as_str()).copied()).filter(|r| self.told.get(*r as usize).copied().flatten().is_some()).collect();
        let Some(s) = self.sigs.get_mut(sig) else { return };
        let (id, gone, new): (SigId, Vec<u32>, Vec<u32>) = (s.id, s.members.difference(&want).copied().collect(), want.difference(&s.members).copied().collect());
        s.members = want;
        s.synced = Some(generation);
        self.out.clear();
        for r in gone {
            self.lb.remove(id, RunnerId(r));
        }
        for r in new {
            self.lb.add(id, RunnerId(r), &mut self.out);
        }
        self.out.clear();
    }

    /// Kind `kind`'s id, interned when first named. No kind (an empty key) is [`KindId::NONE`]:
    /// its legs are decided by rank and turn alone.
    fn kind(&mut self, kind: &str) -> KindId {
        if kind.is_empty() {
            return KindId::NONE;
        }
        if let Some(k) = self.kinds.get(kind) {
            return *k;
        }
        let k = KindId(u32::try_from(self.kinds.len()).unwrap_or(u32::MAX - 2));
        self.kinds.insert(kind.to_owned(), k);
        self.lb.kinds(k.0.saturating_add(1));
        k
    }

    /// The runner the kernel puts a leg of kind `kind` on, of signature `sig`'s members not set
    /// aside ([`Balance::set_aside`]): the most free by its rank (core-equivalents under
    /// contention: a leg is wide, its builds and tests using every core it finds free), its kind's
    /// good place among equals, else the next in turn (which the pick takes), passing over the
    /// runner that let its kind down when another is as free (docs/lb.md "Good places"). Whether that runner takes the
    /// leg now is the engine's rules' to say. The kernel's view isn't left changed: the engine
    /// tells it the runner's room once the leg is placed ([`Balance::tell`]). `None`: no member.
    pub(super) fn propose(&mut self, sig: &str, kind: &str) -> Option<String> {
        let s = self.sigs.get(sig)?.id;
        let k = self.kind(kind);
        self.lb.peek(s, &ANY_ROOM)?;
        self.out.clear();
        // Wide: a leg's steps run as wide as its runner's cores (docs/lb.md "Wide and narrow").
        let asked = self.lb.ask_kind_wide(s, k, ANY_ROOM, &mut self.out);
        let picked = match asked {
            Ok(lb::Asked { run, send: Some((r, _)) }) => {
                let _ = self.lb.unsent(run);
                self.name(r).map(str::to_owned)
            }
            // Waiting is the engine's (its queue), never the kernel's.
            Ok(lb::Asked { run, send: None }) => {
                let _ = self.lb.cancel(run, &mut self.out);
                None
            }
            Err(_) => None,
        };
        self.out.clear();
        picked
    }

    /// Runner `runner` can't take the leg being placed now: out of signature `sig` until
    /// [`Balance::restore`], so the next proposal is the kernel's next choice without it.
    pub(super) fn set_aside(&mut self, sig: &str, runner: &str) {
        if let (Some(s), Some(&r)) = (self.sigs.get(sig), self.ids.get(runner))
            && s.members.contains(&r)
        {
            self.lb.remove(s.id, RunnerId(r));
        }
    }

    /// The runners set aside from signature `sig` for one leg's placement are its members again.
    pub(super) fn restore(&mut self, sig: &str, aside: &[String]) {
        let Some(s) = self.sigs.get(sig) else { return };
        let id = s.id;
        let back: Vec<u32> = aside.iter().filter_map(|n| self.ids.get(n.as_str()).copied()).filter(|r| s.members.contains(r)).collect();
        self.out.clear();
        for r in back {
            self.lb.add(id, RunnerId(r), &mut self.out);
        }
        self.out.clear();
    }

    /// An attempt of a leg of kind `kind` ended on `runner` as `how` says (docs/lb.md "Good
    /// places"): it went well there (that kind's good place from now), the runner let it down (it
    /// is passed over for that kind among equals), or it failed and no one knows whose fault it
    /// was (settled by comparison: its kind's next leg goes elsewhere once). Nothing for a leg
    /// that names no kind, or an end judged by no one.
    pub(super) fn ended(&mut self, runner: &str, kind: &str, how: How) {
        if kind.is_empty() || how == How::Unsaid {
            return;
        }
        let Some(&r) = self.ids.get(runner) else { return };
        let k = self.kind(kind);
        match how {
            How::Well => self.lb.won(RunnerId(r), k),
            How::LetDown => self.lb.failed(RunnerId(r), k),
            How::Unsure => self.lb.unsure(RunnerId(r), k),
            How::Unsaid => {}
        }
    }

    /// Kind `kind`'s good place now (`None`: none): to say why a leg went where it did.
    pub(super) fn good(&self, kind: &str) -> Option<&str> {
        let k = self.kinds.get(kind)?;
        self.lb.good(*k).and_then(|r| self.name(r))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(cpu: u64) -> Word {
        Word { epoch: 1, whole: true, cpu, rest: [0; 3], ..Word::default() }
    }

    fn fleet(b: &mut Balance, rooms: &[(&str, u64)]) {
        for (n, cpu) in rooms {
            b.tell(n, [32_000, 64 << 30, 0, 0], word(*cpu));
        }
        let names: Vec<String> = rooms.iter().map(|(n, _)| (*n).to_owned()).collect();
        b.sync("s", 1, names.iter());
    }

    /// W05-R23: the kernel's pick is the member with the most room; a proposal that doesn't take
    /// moves nothing, and a runner set aside is passed over until it's restored.
    #[test]
    fn w05_r23_the_most_room_first_and_a_runner_set_aside_is_passed_over() {
        let mut b = Balance::default();
        fleet(&mut b, &[("a", 4_000), ("b", 12_000), ("c", 8_000)]);
        assert_eq!(b.propose("s", "").as_deref(), Some("b"));
        assert_eq!(b.propose("s", "").as_deref(), Some("b"));
        b.set_aside("s", "b");
        assert_eq!(b.propose("s", "").as_deref(), Some("c"));
        b.restore("s", &["b".to_owned()]);
        assert_eq!(b.propose("s", "").as_deref(), Some("b"));
        // Told less room, it's no longer first.
        b.tell("b", [32_000, 64 << 30, 0, 0], word(2_000));
        assert_eq!(b.propose("s", "").as_deref(), Some("c"));
        // Gone, it's no member; told again, it is.
        b.gone("c");
        assert_eq!(b.propose("s", "").as_deref(), Some("a"));
        b.tell("c", [32_000, 64 << 30, 0, 0], word(8_000));
        b.sync("s", 2, ["a", "b", "c"].iter().map(|s| (*s).to_owned()).collect::<Vec<_>>().iter());
        assert_eq!(b.propose("s", "").as_deref(), Some("c"));
    }

    /// W05-R04, docs/lb.md "Good places": between runners as free, equals take turns; a kind that
    /// went well on one goes back to it; one let down there is passed over for the other.
    #[test]
    fn w05_r04_equals_take_turns_and_a_kind_goes_back_to_where_it_went_well() {
        let mut b = Balance::default();
        fleet(&mut b, &[("a", 8_000), ("b", 8_000)]);
        let first = b.propose("s", "k");
        let second = b.propose("s", "k");
        assert!(first.is_some() && second.is_some() && first != second, "{first:?} then {second:?}");
        b.ended("b", "k", How::Well);
        for _ in 0..3 {
            assert_eq!(b.propose("s", "k").as_deref(), Some("b"));
        }
        assert_eq!(b.good("k"), Some("b"));
        b.ended("b", "k", How::LetDown);
        assert_eq!(b.good("k"), None);
        for _ in 0..3 {
            assert_eq!(b.propose("s", "k").as_deref(), Some("a"));
        }
        // Another kind takes its turns as before.
        let x = b.propose("s", "other");
        let y = b.propose("s", "other");
        assert!(x != y, "{x:?} then {y:?}");
    }

    /// Every member is proposed, the most free first, those with no room last (whether one takes
    /// a leg now is the engine's to say); none once all are set aside, or the signature has none.
    /// Nothing is left running or waiting in the kernel.
    #[test]
    fn every_member_in_rank_and_none_once_all_are_set_aside() {
        let mut b = Balance::default();
        fleet(&mut b, &[("a", 0), ("b", 3_000)]);
        assert_eq!(b.propose("s", "k").as_deref(), Some("b"));
        b.set_aside("s", "b");
        assert_eq!(b.propose("s", "k").as_deref(), Some("a"));
        b.set_aside("s", "a");
        assert_eq!(b.propose("s", "k"), None);
        b.restore("s", &["a".to_owned(), "b".to_owned()]);
        assert_eq!(b.propose("s", "k").as_deref(), Some("b"));
        assert_eq!(b.propose("empty", "k"), None);
        assert!(b.lb.runs().is_empty());
    }
}
