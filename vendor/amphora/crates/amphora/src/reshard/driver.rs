//! Each operation's steps (docs/design/reshard.md, "Copy, then catch-up, then a brief cutover"),
//! as actions a shell carries out and answers: no clock, disk or network here.
//!
//! The steps: the span is copied to its new group in batches, each read at a position of the old
//! group's log; then the new group replays that log from the first copy's position, in rounds, each
//! up to where the log ended as the round began, while the old group still takes writes; once a
//! round no longer shrinks the lag (zero, or the writes made during a round now take as long to
//! replay as the round did), the old group fences the span with an entry in its own log, the new
//! group replays up to that entry, and the directory flips the span. Then the new group takes it
//! up, the old one lets it go and sweeps its rows, and a group left with nothing is retired.
//!
//! The shell's contract:
//! - Out items are carried out in order, and an action after a [`Out::Commit`] only once that
//!   change is durable in the directory.
//! - Every action is answered once, by its group, with the [`Ask`] it carried; an action whose
//!   answer is lost (its group restarted) is asked again by [`Driver::restarted`]. Answers to
//!   anything but an operation's last ask are ignored, so a late answer changes nothing.
//! - Every action is idempotent at its group, and an action from an operation that has ended there
//!   is refused ([`super::holding`]), so actions from a driver that has since restarted are
//!   harmless.
//! - A retired group answers a [`Do::Release`] or a [`Do::Retire`] asked of it again as done: it
//!   held nothing when it was retired, and a driver that restarted between the retire and the
//!   operation's end asks them again.
//! - The old group keeps its log from the first copy's position until the operation ends, or
//!   answers [`Failure::LogGone`], and the operation is given up.

use std::collections::BTreeMap;

use super::directory::{Change, Directory, Kind, Phase, Proposal};
use super::holding::Progress;
use super::{GroupId, NodeId, OpId, Refused, Seq, Span};

/// An action's ask: the driver's epoch (raised at each restart, as its directory's term) and a
/// count within it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Ask {
    pub epoch: u64,
    pub n: u64,
}

/// What a group is asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Do {
    /// Make `group`, empty, on `node`. A retired id is never made again.
    Create { group: GroupId, node: NodeId },
    /// `to` copies its next batch of `span` from `from`.
    Copy {
        from: GroupId,
        to: GroupId,
        span: Span,
    },
    /// `to` replays its next batch of `from`'s log for `span`, up to `upto`.
    Replay {
        from: GroupId,
        to: GroupId,
        span: Span,
        upto: Seq,
    },
    /// `group` fences `span` with an entry in its log.
    Fence { group: GroupId, span: Span },
    /// `group` takes up the span it took in.
    Activate { group: GroupId },
    /// `group` lets go of its fenced span and sweeps a batch of its rows.
    Release { group: GroupId, span: Span },
    /// `group` lifts its fence, if it has one.
    Unfence { group: GroupId },
    /// `group` throws away what it took in and sweeps a batch of those rows.
    Discard { group: GroupId, span: Span },
    /// `group`, holding nothing, is retired.
    Retire { group: GroupId },
}

impl Do {
    /// The group that carries it out.
    #[must_use]
    pub fn group(&self) -> GroupId {
        match *self {
            Self::Create { group, .. }
            | Self::Fence { group, .. }
            | Self::Activate { group }
            | Self::Release { group, .. }
            | Self::Unfence { group }
            | Self::Discard { group, .. }
            | Self::Retire { group } => group,
            Self::Copy { to, .. } | Self::Replay { to, .. } => to,
        }
    }
}

/// An action for a group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Action {
    pub ask: Ask,
    pub op: OpId,
    pub what: Do,
}

/// Why a group couldn't do what it was asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// The old group's log no longer reaches back to where catch-up stands.
    LogGone,
    /// The group refused it (a stale action, or one that doesn't fit what it holds).
    Refused(Refused),
    /// There is no such group.
    NoGroup,
}

/// A group's answer to an action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Created,
    /// A batch copied: the progress, and where the old group's log ends now.
    Copied {
        progress: Progress,
        head: Seq,
    },
    /// A batch replayed: the progress, and where the old group's log ends now.
    Replayed {
        progress: Progress,
        head: Seq,
    },
    /// Fenced by the entry at `at`.
    Fenced {
        at: Seq,
    },
    Activated,
    /// A batch swept; `done` when none of the span's rows is left.
    Released {
        done: bool,
    },
    Unfenced,
    /// A batch swept; `done` when none of the span's rows is left.
    Discarded {
        done: bool,
    },
    Retired,
    Failed(Failure),
}

/// A group's answer, with the ask it answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    pub ask: Ask,
    pub op: OpId,
    pub reply: Reply,
}

/// An operation that ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ended {
    pub op: OpId,
    pub kind: Kind,
    /// Flipped (true) or given up (false).
    pub done: bool,
    /// Entries of the old group's log replayed after its fence: the cutover's work, while the
    /// span takes no writes (unknown after a restart in the cutover).
    pub fence_lag: Option<u64>,
    /// Why it was given up.
    pub why: Option<Failure>,
}

/// What the shell does next, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Out {
    /// Make this change durable in the directory before anything after it.
    Commit(Change),
    Send(Action),
    Ended(Ended),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Create,
    Copy,
    /// A catch-up round, replaying to `to`; `lag` the entries it set out to replay.
    Round {
        to: Seq,
        lag: u64,
    },
    Fence,
    Final {
        at: Seq,
    },
    Activate,
    Release,
    RetireFrom,
    Unfence,
    Discard,
    RetireTo,
}

#[derive(Debug, Clone)]
struct Run {
    ask: Ask,
    what: Do,
    stage: Stage,
    replayed: Option<Seq>,
    fence_lag: Option<u64>,
    why: Option<Failure>,
}

/// The operations' driver: the directory, and each operation's step under way.
#[derive(Debug, Clone)]
pub struct Driver {
    dir: Directory,
    epoch: u64,
    n: u64,
    runs: BTreeMap<OpId, Run>,
}

impl Driver {
    /// A driver on `dir` as it is durable, in `epoch` (greater than any driver's before it): every
    /// operation under way is taken up again from its phase, each group asked where it stands.
    pub fn recover(dir: Directory, epoch: u64) -> Result<(Self, Vec<Out>), Refused> {
        let mut driver = Self {
            dir,
            epoch,
            n: 0,
            runs: BTreeMap::new(),
        };
        let ops: Vec<_> = driver.dir.ops().map(|o| (o.id, o.phase)).collect();
        let mut out = Vec::new();
        for (id, phase) in ops {
            let next = match phase {
                Phase::Copying => driver.start(id)?,
                Phase::Cutover => driver.send(id, Stage::Fence)?,
                Phase::Flipped => driver.send(id, Stage::Activate)?,
                Phase::Aborting => driver.send(id, Stage::Unfence)?,
            };
            out.push(next);
        }
        Ok((driver, out))
    }

    #[must_use]
    pub fn directory(&self) -> &Directory {
        &self.dir
    }

    /// Whether an operation is under way.
    #[must_use]
    pub fn idle(&self) -> bool {
        self.runs.is_empty()
    }

    /// Begin `proposal`.
    pub fn begin(&mut self, proposal: &Proposal) -> Result<Vec<Out>, Refused> {
        let change = self.dir.propose(proposal)?;
        let Change::Begin { op, .. } = &change else {
            return Err(Refused::Invalid("a proposal begins an operation"));
        };
        let id = op.id;
        let commit = self.commit(change)?;
        Ok(vec![commit, self.start(id)?])
    }

    /// `group` restarted: what it was asked and hadn't answered is asked again.
    pub fn restarted(&mut self, group: GroupId) -> Result<Vec<Out>, Refused> {
        let ids: Vec<OpId> = self
            .runs
            .iter()
            .filter(|(_, r)| r.what.group() == group)
            .map(|(id, _)| *id)
            .collect();
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            let ask = self.ask()?;
            if let Some(run) = self.runs.get_mut(&id) {
                run.ask = ask;
                out.push(Out::Send(Action {
                    ask,
                    op: id,
                    what: run.what,
                }));
            }
        }
        Ok(out)
    }

    /// A group's answer. One to an ask that isn't its operation's last is ignored.
    pub fn answer(&mut self, answer: Answer) -> Result<Vec<Out>, Refused> {
        let Some(run) = self.runs.get(&answer.op) else {
            return Ok(Vec::new());
        };
        if run.ask != answer.ask {
            return Ok(Vec::new());
        }
        let id = answer.op;
        let stage = run.stage;
        let phase = self.dir.op(id).map(|o| o.phase).ok_or(Refused::NoOp(id))?;
        let one = |o: Out| Ok(vec![o]);
        match (stage, answer.reply) {
            (_, Reply::Failed(why)) => {
                if matches!(phase, Phase::Copying | Phase::Cutover) {
                    if let Some(run) = self.runs.get_mut(&id) {
                        run.why = Some(why);
                    }
                    let commit = self.commit(Change::Abort(id))?;
                    Ok(vec![commit, self.send(id, Stage::Unfence)?])
                } else {
                    // Past the flip there is no going back, and the abort's steps can't fail but
                    // for a group gone: wait for it to restart ([`Driver::restarted`]).
                    Ok(Vec::new())
                }
            }
            (Stage::Create, Reply::Created) => one(self.send(id, Stage::Copy)?),
            (Stage::Copy, Reply::Copied { progress, head }) => {
                self.note(id, progress.replayed);
                if !progress.copy_done {
                    return one(self.send(id, Stage::Copy)?);
                }
                let lag = head.saturating_sub(progress.replayed);
                if lag == 0 {
                    return self.cutover(id);
                }
                one(self.send(id, Stage::Round { to: head, lag })?)
            }
            (Stage::Round { to, lag }, Reply::Replayed { progress, head }) => {
                self.note(id, progress.replayed);
                if progress.replayed < to {
                    return one(self.send(id, Stage::Round { to, lag })?);
                }
                let next = head.saturating_sub(to);
                if next == 0 || next >= lag {
                    return self.cutover(id);
                }
                one(self.send(
                    id,
                    Stage::Round {
                        to: head,
                        lag: next,
                    },
                )?)
            }
            (Stage::Fence, Reply::Fenced { at }) => {
                if let Some(run) = self.runs.get_mut(&id) {
                    run.fence_lag = run.replayed.map(|r| at.saturating_sub(r));
                }
                one(self.send(id, Stage::Final { at })?)
            }
            (Stage::Final { at }, Reply::Replayed { progress, .. }) => {
                if progress.replayed < at {
                    return one(self.send(id, Stage::Final { at })?);
                }
                let commit = self.commit(Change::Flip(id))?;
                Ok(vec![commit, self.send(id, Stage::Activate)?])
            }
            (Stage::Activate, Reply::Activated) => one(self.send(id, Stage::Release)?),
            (Stage::Release, Reply::Released { done }) => {
                let kind = self.kind(id)?;
                match (done, kind.retires()) {
                    (false, _) => one(self.send(id, Stage::Release)?),
                    (true, true) => one(self.send(id, Stage::RetireFrom)?),
                    (true, false) => self.end(id),
                }
            }
            (Stage::RetireFrom, Reply::Retired) | (Stage::RetireTo, Reply::Retired) => self.end(id),
            (Stage::Unfence, Reply::Unfenced) => one(self.send(id, Stage::Discard)?),
            (Stage::Discard, Reply::Discarded { done }) => {
                let kind = self.kind(id)?;
                match (done, kind.makes()) {
                    (false, _) => one(self.send(id, Stage::Discard)?),
                    (true, true) => one(self.send(id, Stage::RetireTo)?),
                    (true, false) => self.end(id),
                }
            }
            _ => Err(Refused::Invalid("an answer that doesn't fit the ask")),
        }
    }

    fn kind(&self, id: OpId) -> Result<Kind, Refused> {
        self.dir.op(id).map(|o| o.kind).ok_or(Refused::NoOp(id))
    }

    fn note(&mut self, id: OpId, replayed: Seq) {
        if let Some(run) = self.runs.get_mut(&id) {
            run.replayed = Some(replayed);
        }
    }

    fn ask(&mut self) -> Result<Ask, Refused> {
        self.n = self
            .n
            .checked_add(1)
            .ok_or(Refused::Broken("asks ran out"))?;
        Ok(Ask {
            epoch: self.epoch,
            n: self.n,
        })
    }

    fn commit(&mut self, change: Change) -> Result<Out, Refused> {
        self.dir.apply(&change)?;
        Ok(Out::Commit(change))
    }

    /// The first step of a copying operation.
    fn start(&mut self, id: OpId) -> Result<Out, Refused> {
        if self.kind(id)?.makes() {
            self.send(id, Stage::Create)
        } else {
            self.send(id, Stage::Copy)
        }
    }

    fn cutover(&mut self, id: OpId) -> Result<Vec<Out>, Refused> {
        let commit = self.commit(Change::Cutover(id))?;
        Ok(vec![commit, self.send(id, Stage::Fence)?])
    }

    fn end(&mut self, id: OpId) -> Result<Vec<Out>, Refused> {
        let op = self.dir.op(id).ok_or(Refused::NoOp(id))?;
        let kind = op.kind;
        let done = op.phase == Phase::Flipped;
        let commit = self.commit(Change::End(id))?;
        let run = self.runs.remove(&id);
        Ok(vec![
            commit,
            Out::Ended(Ended {
                op: id,
                kind,
                done,
                fence_lag: run.as_ref().and_then(|r| r.fence_lag),
                why: run.and_then(|r| r.why),
            }),
        ])
    }

    /// Ask the step of `stage` of operation `id`.
    fn send(&mut self, id: OpId, stage: Stage) -> Result<Out, Refused> {
        let op = self.dir.op(id).ok_or(Refused::NoOp(id))?;
        let (from, to, span) = (op.from, op.to, op.span);
        let what = match stage {
            Stage::Create => Do::Create {
                group: to,
                node: self.dir.node_of(to).ok_or(Refused::NoGroup(to))?,
            },
            Stage::Copy => Do::Copy { from, to, span },
            Stage::Round { to: upto, .. } | Stage::Final { at: upto } => Do::Replay {
                from,
                to,
                span,
                upto,
            },
            Stage::Fence => Do::Fence { group: from, span },
            Stage::Activate => Do::Activate { group: to },
            Stage::Release => Do::Release { group: from, span },
            Stage::RetireFrom => Do::Retire { group: from },
            Stage::Unfence => Do::Unfence { group: from },
            Stage::Discard => Do::Discard { group: to, span },
            Stage::RetireTo => Do::Retire { group: to },
        };
        let ask = self.ask()?;
        let run = self.runs.entry(id).or_insert(Run {
            ask,
            what,
            stage,
            replayed: None,
            fence_lag: None,
            why: None,
        });
        run.ask = ask;
        run.what = what;
        run.stage = stage;
        Ok(Out::Send(Action { ask, op: id, what }))
    }
}
