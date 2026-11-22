//! What one group holds (docs/design/reshard.md, "A group's holding"): the span it answers for,
//! the part of it fenced for a cutover, and the span it is taking in. It is kept in the group's own
//! state machine, durable with its rows, so a group answers a key only from what it holds itself:
//! a router with a stale directory, or a stale action from a driver that has since restarted, can
//! never make two groups take writes for one key.
//!
//! A group is in at most one operation at a time (the directory's rule), and operation ids only
//! grow, so one number, the last operation that ended here, tells a stale action from a live one.

use super::{OpId, Pos, Refused, Seq, Span};

/// Why a group doesn't answer a position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Away {
    /// It holds it, fenced for a cutover: ask again once the directory has flipped (the pause).
    Fenced(OpId),
    /// It doesn't hold it: the router's directory is stale.
    Elsewhere,
}

/// A fence: from `at`, a fence entry in the group's own log, nothing in `span` is written here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Fence {
    pub op: OpId,
    pub span: Span,
    pub at: Seq,
}

/// How far a span being taken in has come: copied, then caught up from the old group's log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Progress {
    /// The last position whose rows are all copied, if any.
    pub copied: Option<Pos>,
    /// Every row of the span copied.
    pub copy_done: bool,
    /// The old group's log position the first copy read at: catch-up replays from just after it.
    pub first_at: Option<Seq>,
    /// The old group's log replayed through here.
    pub replayed: Seq,
}

/// A span being taken in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Incoming {
    pub op: OpId,
    pub span: Span,
    pub progress: Progress,
}

/// What [`Holding::fence`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fenced {
    /// Fenced now: the shell writes the fence entry at `at`, in the same transaction.
    New(Seq),
    /// Already fenced at `at` by this operation: nothing to write.
    Already(Seq),
}

/// What a group holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Holding {
    owned: Option<Span>,
    fenced: Option<Fence>,
    incoming: Option<Incoming>,
    /// The last operation that ended here: anything from it or before is stale.
    ended: OpId,
}

/// Whether a change at `entry` in the old group's log is replayed over a row stamped `stored`
/// (none: no row). A copied row is stamped with the log position its copy read at, a replayed one
/// with its entry's: so replaying the old group's log in order from the first copy's position, over
/// the copied rows, leaves the span as the old group holds it at the last entry replayed, with no
/// tombstone kept for a delete (docs/design/reshard.md, "Copy, then catch-up").
#[must_use]
pub fn replays(stored: Option<Seq>, entry: Seq) -> bool {
    stored.is_none_or(|v| v < entry)
}

impl Holding {
    /// A group holding `owned`, in no operation.
    #[must_use]
    pub fn owning(owned: Option<Span>) -> Self {
        Self {
            owned,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn owned(&self) -> Option<Span> {
        self.owned
    }

    #[must_use]
    pub fn fenced(&self) -> Option<Fence> {
        self.fenced
    }

    #[must_use]
    pub fn incoming(&self) -> Option<Incoming> {
        self.incoming
    }

    /// Whether this group reads and writes `pos` now. A fenced span is refused both: after the
    /// flip its new group may already have newer writes.
    pub fn answers(&self, pos: Pos) -> Result<(), Away> {
        if let Some(f) = self.fenced.filter(|f| f.span.contains(pos)) {
            return Err(Away::Fenced(f.op));
        }
        if self.owned.is_some_and(|s| s.contains(pos)) {
            Ok(())
        } else {
            Err(Away::Elsewhere)
        }
    }

    /// Whether a row at `pos` is no longer this group's: neither held nor being taken in. Only
    /// such rows are swept, so a late or repeated sweep never deletes a row the group holds again.
    #[must_use]
    pub fn garbage(&self, pos: Pos) -> bool {
        !self.owned.is_some_and(|s| s.contains(pos))
            && !self.incoming.is_some_and(|i| i.span.contains(pos))
    }

    fn live(&self, op: OpId) -> Result<(), Refused> {
        if op <= self.ended {
            Err(Refused::Ended(op))
        } else {
            Ok(())
        }
    }

    /// Begin taking in `span` for `op` (again, after a restart: the same progress).
    pub fn receive(&mut self, op: OpId, span: Span) -> Result<Progress, Refused> {
        self.live(op)?;
        match self.incoming {
            Some(i) if i.op == op && i.span == span => return Ok(i.progress),
            Some(_) => return Err(Refused::Invalid("the group is taking in another span")),
            None => {}
        }
        if self.fenced.is_some() {
            return Err(Refused::Invalid("the group is giving up a span"));
        }
        if let Some(owned) = self.owned
            && owned.join(&span).is_none()
        {
            return Err(Refused::Invalid("a span taken in must adjoin the one held"));
        }
        self.incoming = Some(Incoming {
            op,
            span,
            progress: Progress::default(),
        });
        Ok(Progress::default())
    }

    fn taking(&mut self, op: OpId) -> Result<&mut Progress, Refused> {
        self.live(op)?;
        match &mut self.incoming {
            Some(i) if i.op == op => Ok(&mut i.progress),
            _ => Err(Refused::Ended(op)),
        }
    }

    /// A batch of `op`'s copy written: its rows through `through` (none: the batch held none),
    /// read at `at` in the old group's log; `done` when no row of the span is past it.
    pub fn copied(
        &mut self,
        op: OpId,
        through: Option<Pos>,
        done: bool,
        at: Seq,
    ) -> Result<Progress, Refused> {
        let p = self.taking(op)?;
        if p.copy_done {
            return Ok(*p);
        }
        if through.is_some() {
            p.copied = through;
        }
        if p.first_at.is_none() {
            p.first_at = Some(at);
            p.replayed = at;
        }
        p.copy_done = done;
        Ok(*p)
    }

    /// `op`'s catch-up replayed the old group's log through `through`.
    pub fn replayed(&mut self, op: OpId, through: Seq) -> Result<Progress, Refused> {
        let p = self.taking(op)?;
        if !p.copy_done {
            return Err(Refused::Invalid("catch-up begins once the copy is done"));
        }
        p.replayed = p.replayed.max(through);
        Ok(*p)
    }

    /// Fence `span` for `op`; `next` is where the fence entry would go in this group's log.
    pub fn fence(&mut self, op: OpId, span: Span, next: Seq) -> Result<Fenced, Refused> {
        if let Some(f) = self.fenced.filter(|f| f.op == op) {
            return Ok(Fenced::Already(f.at));
        }
        self.live(op)?;
        if self.fenced.is_some() || self.incoming.is_some() {
            return Err(Refused::Invalid("the group is in another operation"));
        }
        if !self.owned.is_some_and(|s| s.covers(&span)) {
            return Err(Refused::Invalid("a fence outside the span held"));
        }
        self.fenced = Some(Fence { op, span, at: next });
        Ok(Fenced::New(next))
    }

    /// Lift `op`'s fence (its operation given up); whether it was fenced or not, `op` has ended
    /// here, so its fence can't come late.
    pub fn unfence(&mut self, op: OpId) -> Result<(), Refused> {
        match self.fenced {
            Some(f) if f.op == op => self.fenced = None,
            Some(_) => return Err(Refused::Invalid("another operation's fence is up")),
            None => {}
        }
        if self.incoming.is_some_and(|i| i.op <= op) {
            return Err(Refused::Invalid("the group is taking in a span"));
        }
        self.ended = self.ended.max(op);
        Ok(())
    }

    /// The span taken in is this group's (after the directory's flip).
    pub fn activate(&mut self, op: OpId) -> Result<(), Refused> {
        let Some(i) = self.incoming.filter(|i| i.op == op) else {
            return if op <= self.ended {
                Ok(())
            } else {
                Err(Refused::Invalid("nothing taken in for the operation"))
            };
        };
        if !i.progress.copy_done {
            return Err(Refused::Invalid("the copy isn't done"));
        }
        let owned = match self.owned {
            None => i.span,
            Some(owned) => owned
                .join(&i.span)
                .ok_or(Refused::Invalid("a span taken in must adjoin the one held"))?,
        };
        self.owned = Some(owned);
        self.incoming = None;
        self.ended = self.ended.max(op);
        Ok(())
    }

    /// The fenced span is no longer this group's (after the directory's flip); its rows are
    /// garbage from now.
    pub fn release(&mut self, op: OpId) -> Result<(), Refused> {
        let Some(f) = self.fenced.filter(|f| f.op == op) else {
            return if op <= self.ended {
                Ok(())
            } else {
                Err(Refused::Invalid("no fence of the operation's"))
            };
        };
        let owned = self
            .owned
            .ok_or(Refused::Invalid("a fence outside the span held"))?;
        self.owned = owned.without(&f.span)?;
        self.fenced = None;
        self.ended = self.ended.max(op);
        Ok(())
    }

    /// Throw away what `op` was taking in (its operation given up); its rows are garbage from
    /// now, and `op` has ended here, so its copy can't come late.
    pub fn discard(&mut self, op: OpId) -> Result<(), Refused> {
        match self.incoming {
            Some(i) if i.op == op => self.incoming = None,
            Some(i) if i.op > op => return Ok(()),
            Some(_) => return Err(Refused::Invalid("the group is taking in another span")),
            None => {}
        }
        if self.fenced.is_some_and(|f| f.op <= op) {
            return Err(Refused::Invalid("the group is giving up a span"));
        }
        self.ended = self.ended.max(op);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(lo: Pos, hi: Pos) -> Span {
        Span::new(lo, hi).unwrap()
    }

    /// AC-T1.9.3: a source fenced, its span released, a target activated: at no point do both
    /// answer a position.
    #[test]
    fn t_t1_9_a_position_is_answered_by_one_holding_through_a_cutover() {
        let mut from = Holding::owning(Some(span(0, 99)));
        let mut to = Holding::owning(None);
        let op = OpId(1);
        let moving = span(50, 99);
        to.receive(op, moving).unwrap();
        to.copied(op, Some(99), true, 10).unwrap();
        assert_eq!(from.answers(60), Ok(()));
        assert_eq!(to.answers(60), Err(Away::Elsewhere));
        assert_eq!(from.fence(op, moving, 11), Ok(Fenced::New(11)));
        assert_eq!(from.fence(op, moving, 12), Ok(Fenced::Already(11)));
        assert_eq!(from.answers(60), Err(Away::Fenced(op)));
        assert_eq!(from.answers(10), Ok(()));
        to.replayed(op, 11).unwrap();
        to.activate(op).unwrap();
        assert_eq!(to.answers(60), Ok(()));
        assert_eq!(from.answers(60), Err(Away::Fenced(op)));
        assert!(!from.garbage(60));
        from.release(op).unwrap();
        assert_eq!(from.answers(60), Err(Away::Elsewhere));
        assert!(from.garbage(60));
        assert_eq!(from.owned(), Some(span(0, 49)));
        // Repeated actions change nothing; stale ones are refused.
        from.release(op).unwrap();
        to.activate(op).unwrap();
        assert_eq!(from.fence(op, span(0, 10), 13), Err(Refused::Ended(op)));
        assert_eq!(to.receive(op, moving), Err(Refused::Ended(op)));
        assert_eq!(to.copied(op, Some(1), true, 1), Err(Refused::Ended(op)));
    }

    /// An operation given up: the fence is lifted, the copy thrown away, and a late fence or copy
    /// of it is refused.
    #[test]
    fn t_t1_9_an_abandoned_operation_cant_come_back_late() {
        let mut from = Holding::owning(Some(span(0, 99)));
        let mut to = Holding::owning(Some(span(100, 199)));
        let op = OpId(4);
        to.receive(op, span(0, 99)).unwrap();
        to.copied(op, Some(40), false, 3).unwrap();
        assert!(!to.garbage(20));
        from.unfence(op).unwrap();
        to.discard(op).unwrap();
        assert!(to.garbage(20));
        assert_eq!(from.fence(op, span(0, 99), 9), Err(Refused::Ended(op)));
        assert_eq!(to.receive(op, span(0, 99)), Err(Refused::Ended(op)));
        assert_eq!(from.answers(20), Ok(()));
    }

    /// The replay rule: a change applies over a row stamped before it, never over one stamped at
    /// or after it.
    #[test]
    fn t_t1_9_a_change_replays_only_over_an_older_row() {
        assert!(replays(None, 1));
        assert!(replays(Some(4), 5));
        assert!(!replays(Some(5), 5));
        assert!(!replays(Some(6), 5));
    }
}
