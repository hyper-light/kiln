//! Which group holds each position, where each group is placed, and the operations under way
//! (docs/design/reshard.md, "The directory"). Every position is in exactly one group's span after
//! every change, and every group holds at most one span: a change that would break either is
//! refused whole ([`Directory::check`]).
//!
//! The directory is a deterministic state machine of [`Change`]s: the shell keeps it durable (in
//! production as a replicated group's records) and applies each change in order before it carries
//! out any action that follows it. An operation's one commit point is its [`Change::Flip`]: before
//! it the operation can be given up and nothing has moved; after it the operation only goes
//! forward.

use std::collections::BTreeMap;

use super::{GroupId, NodeId, OpId, Pos, Refused, Span};

/// What an operation does to its span.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Kind {
    /// The upper part of a group's span goes to a new group beside it.
    Split,
    /// A group's whole span goes to the group next to it, and the group is retired.
    Merge,
    /// A group's whole span goes to a new group on another node, and the group is retired.
    Move,
}

impl Kind {
    /// Whether the group the span goes to is made for the operation.
    #[must_use]
    pub fn makes(self) -> bool {
        matches!(self, Self::Split | Self::Move)
    }

    /// Whether the group the span leaves is retired once it has gone.
    #[must_use]
    pub fn retires(self) -> bool {
        matches!(self, Self::Merge | Self::Move)
    }
}

/// Where an operation stands, as the directory keeps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Phase {
    /// The span is being copied to its new group and caught up; its old group still answers it.
    Copying,
    /// Its old group is told to fence the span; the last of it is being caught up.
    Cutover,
    /// The span is the new group's: the operation finishes, whatever happens.
    Flipped,
    /// Given up before the flip: the old group's fence is lifted and the copy thrown away.
    Aborting,
}

/// One operation: `span` going from group `from` to group `to`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Op {
    pub id: OpId,
    pub kind: Kind,
    pub span: Span,
    pub from: GroupId,
    pub to: GroupId,
    pub phase: Phase,
}

/// An operation the planner asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proposal {
    /// Cut `group`'s span before `at`; the upper part goes to a new group on the same node.
    Split { group: GroupId, at: Pos },
    /// `from`'s span goes to `into`, the group whose span it adjoins.
    Merge { from: GroupId, into: GroupId },
    /// `group`'s span goes to a new group on node `to`.
    Move { group: GroupId, to: NodeId },
}

/// A change to the directory, applied in order.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Change {
    /// An operation begins; `place` is the node of the group it makes, when it makes one.
    Begin { op: Op, place: Option<NodeId> },
    /// Its old group is to fence the span.
    Cutover(OpId),
    /// The span is the new group's: the commit point.
    Flip(OpId),
    /// Given up, before the flip.
    Abort(OpId),
    /// Finished (flipped) or given up (aborting): the operation is forgotten, and the group it
    /// emptied, or the group it made for nothing, is gone.
    End(OpId),
}

/// The directory.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Directory {
    generation: u64,
    /// In position order, each starting where the one before ends: every position exactly once.
    spans: Vec<(Span, GroupId)>,
    groups: BTreeMap<GroupId, NodeId>,
    ops: BTreeMap<OpId, Op>,
    next_group: u64,
    next_op: u64,
}

fn next(n: u64) -> Result<u64, Refused> {
    n.checked_add(1)
        .ok_or(Refused::Broken("an id or the generation ran out"))
}

impl Directory {
    /// One group (`GroupId(0)`) holding every position, on `node`: where every database starts,
    /// with no group count chosen by anyone (AC-T1.9.1).
    #[must_use]
    pub fn new(node: NodeId) -> Self {
        Self {
            generation: 0,
            spans: vec![(Span::ALL, GroupId(0))],
            groups: BTreeMap::from([(GroupId(0), node)]),
            ops: BTreeMap::new(),
            next_group: 1,
            next_op: 1,
        }
    }

    /// Raised by every change: a router holding an older one may be routing by a stale map.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Every span with its group, in position order.
    #[must_use]
    pub fn spans(&self) -> &[(Span, GroupId)] {
        &self.spans
    }

    /// Every group with its node, spans or not (a group made by an operation holds none until its
    /// flip).
    pub fn groups(&self) -> impl Iterator<Item = (GroupId, NodeId)> + '_ {
        self.groups.iter().map(|(g, n)| (*g, *n))
    }

    /// Whether `group`'s id has been given out (it may since have been retired): ids are never
    /// given out again.
    #[must_use]
    pub fn given_out(&self, group: GroupId) -> bool {
        group.0 < self.next_group
    }

    /// The operations under way.
    pub fn ops(&self) -> impl Iterator<Item = &Op> + '_ {
        self.ops.values()
    }

    #[must_use]
    pub fn op(&self, id: OpId) -> Option<&Op> {
        self.ops.get(&id)
    }

    /// The group holding `pos`. Always one: none only of a directory broken in memory, which
    /// [`Directory::check`] reports.
    #[must_use]
    pub fn owner(&self, pos: Pos) -> Option<GroupId> {
        let i = self.spans.partition_point(|(s, _)| s.hi() < pos);
        self.spans
            .get(i)
            .filter(|(s, _)| s.contains(pos))
            .map(|(_, g)| *g)
    }

    /// The span `group` holds, if any.
    #[must_use]
    pub fn span_of(&self, group: GroupId) -> Option<Span> {
        self.spans
            .iter()
            .find(|(_, g)| *g == group)
            .map(|(s, _)| *s)
    }

    /// The node `group` is placed on.
    #[must_use]
    pub fn node_of(&self, group: GroupId) -> Option<NodeId> {
        self.groups.get(&group).copied()
    }

    /// Whether `group` is in an operation, on either side.
    #[must_use]
    pub fn busy(&self, group: GroupId) -> bool {
        self.ops.values().any(|o| o.from == group || o.to == group)
    }

    fn free(&self, group: GroupId) -> Result<(), Refused> {
        if !self.groups.contains_key(&group) {
            return Err(Refused::NoGroup(group));
        }
        if self.busy(group) {
            return Err(Refused::Busy(group));
        }
        Ok(())
    }

    /// The change that begins `proposal`, with the ids it takes; refused if it doesn't fit the
    /// directory as it is.
    pub fn propose(&self, proposal: &Proposal) -> Result<Change, Refused> {
        let id = OpId(self.next_op);
        let made = GroupId(self.next_group);
        let (op, place) = match *proposal {
            Proposal::Split { group, at } => {
                self.free(group)?;
                let span = self.span_of(group).ok_or(Refused::NoGroup(group))?;
                let (_, upper) = span
                    .cut(at)
                    .ok_or(Refused::Invalid("a split's cut leaves a side empty"))?;
                let node = self.node_of(group).ok_or(Refused::NoGroup(group))?;
                (op(id, Kind::Split, upper, group, made), Some(node))
            }
            Proposal::Merge { from, into } => {
                self.free(from)?;
                self.free(into)?;
                let span = self.span_of(from).ok_or(Refused::NoGroup(from))?;
                let beside = self.span_of(into).ok_or(Refused::NoGroup(into))?;
                if span.join(&beside).is_none() {
                    return Err(Refused::Invalid(
                        "a merge's groups aren't next to each other",
                    ));
                }
                (op(id, Kind::Merge, span, from, into), None)
            }
            Proposal::Move { group, to } => {
                self.free(group)?;
                let span = self.span_of(group).ok_or(Refused::NoGroup(group))?;
                if self.node_of(group) == Some(to) {
                    return Err(Refused::Invalid("a move to the node the group is on"));
                }
                (op(id, Kind::Move, span, group, made), Some(to))
            }
        };
        Ok(Change::Begin { op, place })
    }

    /// Apply `change`, whole or not at all.
    pub fn apply(&mut self, change: &Change) -> Result<(), Refused> {
        let generation = next(self.generation)?;
        match change {
            Change::Begin { op, place } => self.begin(op, *place)?,
            Change::Cutover(id) => self.step(*id, &[Phase::Copying], Phase::Cutover, "copying")?,
            Change::Flip(id) => self.flip(*id)?,
            Change::Abort(id) => self.step(
                *id,
                &[Phase::Copying, Phase::Cutover],
                Phase::Aborting,
                "before its flip",
            )?,
            Change::End(id) => self.end(*id)?,
        }
        self.generation = generation;
        Ok(())
    }

    fn begin(&mut self, op: &Op, place: Option<NodeId>) -> Result<(), Refused> {
        if op.id != OpId(self.next_op) || op.phase != Phase::Copying {
            return Err(Refused::Invalid(
                "an operation begins with the next id, copying",
            ));
        }
        self.free(op.from)?;
        let span = self.span_of(op.from).ok_or(Refused::NoGroup(op.from))?;
        let fits = match op.kind {
            Kind::Split => op.span.hi() == span.hi() && op.span.lo() > span.lo(),
            Kind::Move => op.span == span,
            Kind::Merge => {
                op.span == span
                    && self
                        .span_of(op.to)
                        .and_then(|beside| span.join(&beside))
                        .is_some()
            }
        };
        if !fits {
            return Err(Refused::Invalid(
                "the operation's span doesn't fit its group's",
            ));
        }
        let next_op = next(self.next_op)?;
        let made = if op.kind.makes() {
            let node = place.ok_or(Refused::Invalid("a new group needs a node"))?;
            if op.to != GroupId(self.next_group) {
                return Err(Refused::Invalid("a new group takes the next id"));
            }
            Some((node, next(self.next_group)?))
        } else {
            if place.is_some() || op.to == op.from {
                return Err(Refused::Invalid(
                    "a merge goes into another group already placed",
                ));
            }
            self.free(op.to)?;
            None
        };
        if let Some((node, next_group)) = made {
            self.groups.insert(op.to, node);
            self.next_group = next_group;
        }
        self.next_op = next_op;
        self.ops.insert(op.id, op.clone());
        Ok(())
    }

    fn step(
        &mut self,
        id: OpId,
        from: &[Phase],
        to: Phase,
        needs: &'static str,
    ) -> Result<(), Refused> {
        let op = self.ops.get_mut(&id).ok_or(Refused::NoOp(id))?;
        if !from.contains(&op.phase) {
            return Err(Refused::Stage { op: id, needs });
        }
        op.phase = to;
        Ok(())
    }

    /// The span becomes `to`'s: the commit point.
    fn flip(&mut self, id: OpId) -> Result<(), Refused> {
        let op = self.ops.get(&id).ok_or(Refused::NoOp(id))?;
        if op.phase != Phase::Cutover {
            return Err(Refused::Stage {
                op: id,
                needs: "in its cutover",
            });
        }
        let mut spans: Vec<(Span, GroupId)> =
            Vec::with_capacity(self.spans.len().saturating_add(1));
        for (span, group) in &self.spans {
            if *group != op.from {
                spans.push((*span, *group));
                continue;
            }
            match span.without(&op.span)? {
                None => spans.push((op.span, op.to)),
                Some(rest) if rest.lo() < op.span.lo() => {
                    spans.push((rest, op.from));
                    spans.push((op.span, op.to));
                }
                Some(rest) => {
                    spans.push((op.span, op.to));
                    spans.push((rest, op.from));
                }
            }
        }
        let mut joined: Vec<(Span, GroupId)> = Vec::with_capacity(spans.len());
        for (span, group) in spans {
            match joined.last_mut() {
                Some((last, g)) if *g == group => {
                    *last = last
                        .join(&span)
                        .ok_or(Refused::Broken("a group's spans aren't next to each other"))?;
                }
                _ => joined.push((span, group)),
            }
        }
        check(&joined, &self.groups)?;
        self.spans = joined;
        if let Some(op) = self.ops.get_mut(&id) {
            op.phase = Phase::Flipped;
        }
        Ok(())
    }

    fn end(&mut self, id: OpId) -> Result<(), Refused> {
        let op = self.ops.get(&id).ok_or(Refused::NoOp(id))?;
        let gone = match op.phase {
            Phase::Flipped if op.kind.retires() => Some(op.from),
            Phase::Flipped => None,
            Phase::Aborting if op.kind.makes() => Some(op.to),
            Phase::Aborting => None,
            Phase::Copying | Phase::Cutover => {
                return Err(Refused::Stage {
                    op: id,
                    needs: "flipped or aborting",
                });
            }
        };
        if let Some(group) = gone {
            if self.span_of(group).is_some() {
                return Err(Refused::Broken("a group retired while it holds a span"));
            }
            self.groups.remove(&group);
        }
        self.ops.remove(&id);
        Ok(())
    }

    /// Every position in exactly one span, each span's group placed, and no group with two spans.
    pub fn check(&self) -> Result<(), Refused> {
        check(&self.spans, &self.groups)
    }
}

fn op(id: OpId, kind: Kind, span: Span, from: GroupId, to: GroupId) -> Op {
    Op {
        id,
        kind,
        span,
        from,
        to,
        phase: Phase::Copying,
    }
}

fn check(spans: &[(Span, GroupId)], groups: &BTreeMap<GroupId, NodeId>) -> Result<(), Refused> {
    let mut expected: Option<Pos> = Some(0);
    let mut seen = std::collections::BTreeSet::new();
    for (span, group) in spans {
        if expected != Some(span.lo()) || span.lo() > span.hi() {
            return Err(Refused::Broken("a gap or an overlap between spans"));
        }
        if !groups.contains_key(group) {
            return Err(Refused::Broken("a span's group isn't placed"));
        }
        if !seen.insert(*group) {
            return Err(Refused::Broken("a group holds two spans"));
        }
        expected = span.hi().checked_add(1);
    }
    if expected.is_some() {
        return Err(Refused::Broken("the spans stop before the last position"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split_flip(d: &mut Directory, group: GroupId, at: Pos) -> OpId {
        let c = d.propose(&Proposal::Split { group, at }).unwrap();
        d.apply(&c).unwrap();
        let Change::Begin { op, .. } = c else {
            panic!()
        };
        d.apply(&Change::Cutover(op.id)).unwrap();
        d.apply(&Change::Flip(op.id)).unwrap();
        d.apply(&Change::End(op.id)).unwrap();
        op.id
    }

    /// AC-T1.9.3: a split, a merge and a move leave every position in exactly one group.
    #[test]
    fn t_t1_9_split_merge_move_keep_every_position_in_one_group() {
        let mut d = Directory::new(NodeId(1));
        split_flip(&mut d, GroupId(0), 1 << 63);
        assert_eq!(d.owner(0), Some(GroupId(0)));
        assert_eq!(d.owner(u64::MAX), Some(GroupId(1)));
        assert_eq!(d.owner(1 << 63), Some(GroupId(1)));
        assert_eq!(d.owner((1 << 63) - 1), Some(GroupId(0)));
        d.check().unwrap();

        let c = d
            .propose(&Proposal::Move {
                group: GroupId(1),
                to: NodeId(2),
            })
            .unwrap();
        d.apply(&c).unwrap();
        d.apply(&Change::Cutover(OpId(2))).unwrap();
        d.apply(&Change::Flip(OpId(2))).unwrap();
        assert_eq!(d.owner(u64::MAX), Some(GroupId(2)));
        d.apply(&Change::End(OpId(2))).unwrap();
        assert_eq!(d.node_of(GroupId(1)), None);
        assert_eq!(d.node_of(GroupId(2)), Some(NodeId(2)));

        let c = d
            .propose(&Proposal::Merge {
                from: GroupId(2),
                into: GroupId(0),
            })
            .unwrap();
        d.apply(&c).unwrap();
        d.apply(&Change::Cutover(OpId(3))).unwrap();
        d.apply(&Change::Flip(OpId(3))).unwrap();
        d.apply(&Change::End(OpId(3))).unwrap();
        assert_eq!(d.spans(), &[(Span::ALL, GroupId(0))]);
        assert_eq!(d.groups().count(), 1);
        d.check().unwrap();
    }

    /// An operation given up before its flip changes no span, and the group it made is gone.
    #[test]
    fn t_t1_9_an_abort_before_the_flip_changes_no_span() {
        let mut d = Directory::new(NodeId(1));
        let c = d
            .propose(&Proposal::Split {
                group: GroupId(0),
                at: 7,
            })
            .unwrap();
        d.apply(&c).unwrap();
        d.apply(&Change::Cutover(OpId(1))).unwrap();
        d.apply(&Change::Abort(OpId(1))).unwrap();
        assert!(d.apply(&Change::Flip(OpId(1))).is_err());
        d.apply(&Change::End(OpId(1))).unwrap();
        assert_eq!(d.spans(), &[(Span::ALL, GroupId(0))]);
        assert_eq!(d.groups().count(), 1);
    }

    /// Refusals: a busy group, a cut leaving a side empty, a merge of groups not side by side, a
    /// flip after its commit point, an abort after it.
    #[test]
    fn t_t1_9_the_directory_refuses_what_would_break_it() {
        let mut d = Directory::new(NodeId(1));
        assert!(
            d.propose(&Proposal::Split {
                group: GroupId(0),
                at: 0
            })
            .is_err()
        );
        split_flip(&mut d, GroupId(0), 100);
        split_flip(&mut d, GroupId(1), 200);
        // 0: [0, 99], 1: [100, 199], 2: [200, max]
        assert!(
            d.propose(&Proposal::Merge {
                from: GroupId(0),
                into: GroupId(2)
            })
            .is_err()
        );
        let c = d
            .propose(&Proposal::Split {
                group: GroupId(1),
                at: 150,
            })
            .unwrap();
        d.apply(&c).unwrap();
        assert_eq!(
            d.propose(&Proposal::Merge {
                from: GroupId(0),
                into: GroupId(1)
            }),
            Err(Refused::Busy(GroupId(1)))
        );
        let id = OpId(3);
        assert!(
            d.apply(&Change::Flip(id)).is_err(),
            "no flip before the cutover"
        );
        d.apply(&Change::Cutover(id)).unwrap();
        d.apply(&Change::Flip(id)).unwrap();
        assert!(
            d.apply(&Change::Abort(id)).is_err(),
            "no abort after the flip"
        );
        let before = d.clone();
        assert!(d.apply(&Change::Flip(id)).is_err());
        assert_eq!(d, before, "a refused change changes nothing");
        d.check().unwrap();
    }
}
