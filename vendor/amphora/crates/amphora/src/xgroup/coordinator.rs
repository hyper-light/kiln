//! The coordinator of transactions across groups (docs/design/xgroup.md): sans-IO. It says which
//! steps to send ([`Event::Send`]) and takes each one's reply once ([`Coordinator::reply`]); its
//! shell carries them to the groups. Every step sent is answered exactly once, by its reply or by
//! [`Lost`]; a lost step may still land later, and the protocol is safe either way.

use std::collections::{BTreeMap, BTreeSet};

use amphora_api::{self as api, Refusal};

use super::{Envelope, GroupId, Mark, Outcome, Reply, Step, TxnId};

/// Which protocol commits a transaction across groups (G13 step 3's two prototypes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// Two-phase commit over groups (Spanner): lock everywhere, then decide at the anchor.
    TwoPhase,
    /// Parallel Commits (CockroachDB): lock everywhere with a staging record at the anchor at once.
    Parallel,
}

/// A coordinator's settings.
#[derive(Debug, Clone)]
pub struct Config {
    /// Its identity: one process's, and its successors' after a crash.
    pub id: u64,
    pub protocol: Protocol,
    /// The group that hands out its incarnations.
    pub registry: GroupId,
    /// Every group: a new incarnation looks in each for what the old one left.
    pub groups: Vec<GroupId>,
    /// The most transactions it runs at once; past it [`Coordinator::begin`] refuses `Full`. Its
    /// shell derives it from what each group admits ([`super::sql::Cluster`]).
    pub in_flight: usize,
}

/// A transaction: keys it reads and keys it writes (`None` deletes). A key may be both: it is read
/// as committed before the transaction and written by it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Txn {
    pub reads: Vec<Vec<u8>>,
    pub writes: Vec<(Vec<u8>, Option<Vec<u8>>)>,
}

/// What the client hears.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// Committed in every group; the reads' values, as committed before it.
    Committed {
        reads: Vec<(Vec<u8>, Option<Vec<u8>>)>,
    },
    /// Aborted in every group.
    Aborted(Why),
    /// This incarnation was fenced first: the one after it decides.
    Unknown,
}

/// Why a transaction aborted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Why {
    /// Another transaction held a lock on this key.
    Conflict(Vec<u8>),
    /// A lock step's outcome was lost.
    Lost,
    /// A group had already finished it: another decided it.
    Late,
}

/// A step's outcome the shell couldn't learn: it may land later, or never.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lost;

/// A step to carry to a group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Send {
    /// What its reply names.
    pub id: u64,
    pub group: GroupId,
    /// The transaction it is about (none for the incarnation's own steps).
    pub txn: Option<TxnId>,
    pub envelope: Envelope,
}

/// What the coordinator asks of its shell or tells it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Send(Send),
    /// The client's answer; `rounds`: the group commits it waited for, one after another.
    Answer {
        txn: TxnId,
        answer: Answer,
        rounds: u32,
    },
    /// Every step of the transaction has come back, and every group it touched has resolved it.
    Finished {
        txn: TxnId,
        outcome: Outcome,
        recovered: bool,
    },
    /// Recovery is done: transactions may begin.
    Ready,
    /// A newer incarnation took over: this one stops; its unanswered transactions are Unknown.
    Fenced,
}

/// What a coordinator has counted (metrics proposed in docs/design/xgroup.md, "Metrics").
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub committed: u64,
    pub aborted: u64,
    pub unknown: u64,
    pub recovered_committed: u64,
    pub recovered_aborted: u64,
    /// Steps sent again after their outcome was lost.
    pub retried: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Begin,
    Scan,
    Lock,
    Decide,
    Recover,
    Prevent,
    Resolve,
}

#[derive(Debug)]
struct Sent {
    kind: Kind,
    group: GroupId,
    txn: Option<TxnId>,
    step: Step,
}

enum State {
    Starting,
    Scanning {
        waiting: BTreeSet<GroupId>,
        found: BTreeMap<TxnId, (GroupId, BTreeSet<GroupId>)>,
    },
    Recovering,
    Running,
    Fenced,
}

enum Phase {
    Locking {
        waiting: BTreeSet<GroupId>,
    },
    Deciding,
    Recovering,
    Preventing {
        waiting: BTreeSet<GroupId>,
        absent: bool,
    },
    Resolving {
        outcome: Outcome,
        waiting: BTreeSet<GroupId>,
        /// Parallel Commits' explicit commit, sent beside the resolutions.
        deciding: bool,
    },
}

struct Run {
    anchor: GroupId,
    /// Where it must be resolved.
    groups: BTreeSet<GroupId>,
    phase: Phase,
    /// Its steps not yet come back.
    outstanding: usize,
    answered: bool,
    recovered: bool,
    reads: Vec<(Vec<u8>, Option<Vec<u8>>)>,
    rounds: u32,
    why: Option<Why>,
}

/// One incarnation of a coordinator.
pub struct Coordinator {
    config: Config,
    epoch: u64,
    state: State,
    next_seq: u64,
    runs: BTreeMap<TxnId, Run>,
    sent: BTreeMap<u64, Sent>,
    next_send: u64,
    counts: Counts,
}

fn broken(why: &str) -> Refusal {
    api::invalid("cross-group transaction", why)
}

fn unexpected(kind: Kind, reply: &Reply) -> Refusal {
    broken(&format!("a group answered {reply:?} to a {kind:?} step"))
}

fn one_more(n: u64) -> Result<u64, Refusal> {
    n.checked_add(1)
        .ok_or_else(|| broken("a coordinator count overflowed"))
}

/// The transaction's shape, refused before anything is sent: the API's own bounds on a
/// transaction's keys and bytes (amphora-api's `TX_MAX`, `KEY_MAX`, `VALUE_MAX`, `REQUEST_MAX`).
fn validate(txn: &Txn) -> Result<(), Refusal> {
    let keys = txn
        .reads
        .len()
        .checked_add(txn.writes.len())
        .ok_or_else(|| broken("too many keys"))?;
    if keys == 0 {
        return Err(broken("it reads and writes nothing"));
    }
    api::bound("cross-group transaction keys", keys, api::TX_MAX)?;
    let mut bytes = 0usize;
    let mut add = |n: usize| -> Result<(), Refusal> {
        bytes = bytes
            .checked_add(n)
            .ok_or_else(|| broken("size overflow"))?;
        Ok(())
    };
    let mut read = BTreeSet::new();
    for key in &txn.reads {
        api::key(key)?;
        add(key.len())?;
        if !read.insert(key.as_slice()) {
            return Err(broken("it reads a key twice"));
        }
    }
    let mut written = BTreeSet::new();
    for (key, value) in &txn.writes {
        api::key(key)?;
        add(key.len())?;
        if !written.insert(key.as_slice()) {
            return Err(broken("it writes a key twice"));
        }
        if let Some(value) = value {
            api::bound("value", value.len(), api::VALUE_MAX)?;
            add(value.len())?;
        }
    }
    api::bound("cross-group transaction bytes", bytes, api::REQUEST_MAX)
}

impl Coordinator {
    /// A new incarnation: it asks the registry for its epoch, then recovers what earlier ones left.
    pub fn start(config: Config) -> Result<(Self, Vec<Event>), Refusal> {
        if !config.groups.contains(&config.registry) {
            return Err(broken("the registry is not among the groups"));
        }
        if config.in_flight == 0 {
            return Err(broken("a coordinator must run at least one transaction"));
        }
        let mut coordinator = Self {
            config,
            epoch: 0,
            state: State::Starting,
            next_seq: 0,
            runs: BTreeMap::new(),
            sent: BTreeMap::new(),
            next_send: 0,
            counts: Counts::default(),
        };
        let mut out = Vec::new();
        let registry = coordinator.config.registry;
        coordinator.send(&mut out, registry, None, Kind::Begin, Step::Begin)?;
        Ok((coordinator, out))
    }

    /// Its incarnation (0 until the registry answers).
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    #[must_use]
    pub fn counts(&self) -> Counts {
        self.counts
    }

    /// Whether transactions may begin.
    #[must_use]
    pub fn ready(&self) -> bool {
        matches!(self.state, State::Running)
    }

    /// Transactions not yet finished, recovered ones among them.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.runs.len()
    }

    /// The oldest transaction it still runs, or the next it would begin: once recovery is done,
    /// every transaction of it below is finished in every group.
    fn floor(&self) -> Option<Mark> {
        match self.state {
            State::Running => Some(self.runs.keys().next().map_or(
                Mark {
                    epoch: self.epoch,
                    seq: self.next_seq,
                },
                |txn| txn.mark(),
            )),
            _ => None,
        }
    }

    fn send(
        &mut self,
        out: &mut Vec<Event>,
        group: GroupId,
        txn: Option<TxnId>,
        kind: Kind,
        step: Step,
    ) -> Result<(), Refusal> {
        let id = self.next_send;
        self.next_send = one_more(id)?;
        if let Some(txn) = txn
            && let Some(run) = self.runs.get_mut(&txn)
        {
            run.outstanding = run
                .outstanding
                .checked_add(1)
                .ok_or_else(|| broken("too many steps out"))?;
        }
        let envelope = Envelope {
            coordinator: self.config.id,
            epoch: self.epoch,
            floor: self.floor(),
            step: step.clone(),
        };
        self.sent.insert(
            id,
            Sent {
                kind,
                group,
                txn,
                step,
            },
        );
        out.push(Event::Send(Send {
            id,
            group,
            txn,
            envelope,
        }));
        Ok(())
    }

    /// Begins `txn`, each key in the group `route` names. Refused while recovering, once fenced,
    /// with `in_flight` transactions running, or for a shape the API's bounds refuse.
    pub fn begin(
        &mut self,
        txn: Txn,
        route: impl Fn(&[u8]) -> GroupId,
    ) -> Result<(TxnId, Vec<Event>), Refusal> {
        match self.state {
            State::Running => {}
            State::Fenced => {
                return Err(Refusal::Fenced(
                    "a newer incarnation of this coordinator took over".into(),
                ));
            }
            State::Starting | State::Scanning { .. } | State::Recovering => {
                return Err(Refusal::Unavailable(
                    "the coordinator is recovering what its last incarnation left".into(),
                ));
            }
        }
        if self.runs.len() >= self.config.in_flight {
            return Err(Refusal::Full {
                limit: self.config.in_flight,
            });
        }
        validate(&txn)?;
        type Plan = (Vec<Vec<u8>>, Vec<(Vec<u8>, Option<Vec<u8>>)>);
        let mut plan: BTreeMap<GroupId, Plan> = BTreeMap::new();
        // The anchor: the group of the first key written (CockroachDB's record goes with it), or
        // of the first read when nothing is written.
        let first = txn
            .writes
            .first()
            .map(|(key, _)| key)
            .or_else(|| txn.reads.first());
        let anchor = first.map(|key| route(key)).unwrap_or(self.config.registry);
        for key in txn.reads {
            plan.entry(route(&key)).or_default().0.push(key);
        }
        for (key, value) in txn.writes {
            plan.entry(route(&key)).or_default().1.push((key, value));
        }
        if let Some(group) = plan.keys().find(|g| !self.config.groups.contains(g)) {
            return Err(broken(&format!(
                "a key routes to group {group}, which isn't one"
            )));
        }
        let id = TxnId {
            coordinator: self.config.id,
            epoch: self.epoch,
            seq: self.next_seq,
        };
        self.next_seq = one_more(self.next_seq)?;
        let participants: Vec<GroupId> = plan.keys().copied().collect();
        self.runs.insert(
            id,
            Run {
                anchor,
                groups: participants.iter().copied().collect(),
                phase: Phase::Locking {
                    waiting: participants.iter().copied().collect(),
                },
                outstanding: 0,
                answered: false,
                recovered: false,
                reads: Vec::new(),
                rounds: 1,
                why: None,
            },
        );
        let mut out = Vec::new();
        let parallel = self.config.protocol == Protocol::Parallel;
        for (group, (reads, writes)) in plan {
            let stage = (parallel && group == anchor).then(|| participants.clone());
            let step = Step::Lock {
                txn: id,
                anchor,
                reads,
                writes,
                stage,
            };
            self.send(&mut out, group, Some(id), Kind::Lock, step)?;
        }
        Ok((id, out))
    }

    /// Step `id`'s reply. A reply no run waits for (after fencing) is dropped: nothing needs it.
    /// An error is a broken invariant (a group answering what the protocol can't), never a vote.
    pub fn reply(&mut self, id: u64, reply: Result<Reply, Lost>) -> Result<Vec<Event>, Refusal> {
        let mut out = Vec::new();
        let Some(sent) = self.sent.remove(&id) else {
            return Ok(out);
        };
        if reply == Ok(Reply::Fenced) {
            self.fence(&mut out);
            return Ok(out);
        }
        if reply.is_err() && sent.kind != Kind::Lock {
            // Every step but a lock is idempotent: the same again, until it is answered.
            self.counts.retried = one_more(self.counts.retried)?;
        }
        match sent.kind {
            Kind::Begin => match reply {
                Ok(Reply::Epoch(epoch)) => self.scan(&mut out, epoch)?,
                Err(Lost) => self.send(&mut out, sent.group, None, Kind::Begin, sent.step)?,
                Ok(other) => return Err(unexpected(sent.kind, &other)),
            },
            Kind::Scan => self.on_scan(&mut out, sent, reply)?,
            _ => self.on_run(&mut out, sent, reply)?,
        }
        Ok(out)
    }

    fn fence(&mut self, out: &mut Vec<Event>) {
        self.state = State::Fenced;
        for (txn, run) in std::mem::take(&mut self.runs) {
            if !run.answered {
                self.counts.unknown = self.counts.unknown.saturating_add(1);
                out.push(Event::Answer {
                    txn,
                    answer: Answer::Unknown,
                    rounds: run.rounds,
                });
            }
        }
        self.sent.clear();
        out.push(Event::Fenced);
    }

    fn scan(&mut self, out: &mut Vec<Event>, epoch: u64) -> Result<(), Refusal> {
        if epoch == 0 {
            return Err(broken("the registry answered incarnation 0"));
        }
        self.epoch = epoch;
        let groups = self.config.groups.clone();
        self.state = State::Scanning {
            waiting: groups.iter().copied().collect(),
            found: BTreeMap::new(),
        };
        for group in groups {
            let step = Step::Scan {
                below: epoch,
                after: None,
            };
            self.send(out, group, None, Kind::Scan, step)?;
        }
        Ok(())
    }

    fn on_scan(
        &mut self,
        out: &mut Vec<Event>,
        sent: Sent,
        reply: Result<Reply, Lost>,
    ) -> Result<(), Refusal> {
        let State::Scanning { waiting, found } = &mut self.state else {
            return Err(broken("a scan answered outside recovery"));
        };
        match reply {
            Err(Lost) => return self.send(out, sent.group, None, Kind::Scan, sent.step),
            Ok(Reply::Open { txns, more }) => {
                let last = txns.last().map(|(txn, _)| *txn);
                for (txn, anchor) in txns {
                    found
                        .entry(txn)
                        .or_insert_with(|| (anchor, BTreeSet::new()))
                        .1
                        .insert(sent.group);
                }
                if more && let Some(after) = last {
                    let step = Step::Scan {
                        below: self.epoch,
                        after: Some(after),
                    };
                    return self.send(out, sent.group, None, Kind::Scan, step);
                }
                waiting.remove(&sent.group);
                if !waiting.is_empty() {
                    return Ok(());
                }
            }
            Ok(other) => return Err(unexpected(sent.kind, &other)),
        }
        let found = match std::mem::replace(&mut self.state, State::Recovering) {
            State::Scanning { found, .. } => found,
            _ => BTreeMap::new(),
        };
        for (txn, (anchor, groups)) in found {
            self.runs.insert(
                txn,
                Run {
                    anchor,
                    groups,
                    phase: Phase::Recovering,
                    outstanding: 0,
                    answered: true,
                    recovered: true,
                    reads: Vec::new(),
                    rounds: 0,
                    why: None,
                },
            );
            self.send(out, anchor, Some(txn), Kind::Recover, Step::Recover { txn })?;
        }
        self.ready_if_recovered(out);
        Ok(())
    }

    fn ready_if_recovered(&mut self, out: &mut Vec<Event>) {
        if matches!(self.state, State::Recovering) && !self.runs.values().any(|run| run.recovered) {
            self.state = State::Running;
            out.push(Event::Ready);
        }
    }

    fn on_run(
        &mut self,
        out: &mut Vec<Event>,
        sent: Sent,
        reply: Result<Reply, Lost>,
    ) -> Result<(), Refusal> {
        let Some(txn) = sent.txn else {
            return Err(broken(
                "a transaction's step came back without its transaction",
            ));
        };
        let protocol = self.config.protocol;
        let Some(run) = self.runs.get_mut(&txn) else {
            return Ok(());
        };
        run.outstanding = run
            .outstanding
            .checked_sub(1)
            .ok_or_else(|| broken("a step came back twice"))?;
        let mut next: Vec<(GroupId, Kind, Step)> = Vec::new();
        let mut answer = None;
        let resolve_all = |run: &Run, outcome: Outcome, next: &mut Vec<(GroupId, Kind, Step)>| {
            for group in &run.groups {
                next.push((*group, Kind::Resolve, Step::Resolve { txn, outcome }));
            }
        };
        match (sent.kind, reply) {
            (Kind::Lock, reply) => {
                // A lock's reply after the transaction turned to aborting needs nothing: its lock,
                // if it landed, goes with the resolution, or the resolution came first and the
                // group refused it Late.
                let Phase::Locking { waiting } = &mut run.phase else {
                    return self.finish(out, txn);
                };
                waiting.remove(&sent.group);
                let all_in = waiting.is_empty();
                let failed = match reply {
                    Ok(Reply::Locked { reads }) => {
                        run.reads.extend(reads);
                        None
                    }
                    Ok(Reply::Conflict { key }) => Some(Why::Conflict(key)),
                    Ok(Reply::Late) => Some(Why::Late),
                    Err(Lost) => Some(Why::Lost),
                    Ok(other) => return Err(unexpected(sent.kind, &other)),
                };
                match (failed, protocol) {
                    (Some(why), Protocol::TwoPhase) => {
                        // Nothing decided it, and only this incarnation could: it aborts here and
                        // now, and a recovery after a crash finds no decision and aborts too.
                        answer = Some(Answer::Aborted(why));
                        run.phase = Phase::Resolving {
                            outcome: Outcome::Aborted,
                            waiting: run.groups.clone(),
                            deciding: false,
                        };
                        resolve_all(run, Outcome::Aborted, &mut next);
                    }
                    (Some(why), Protocol::Parallel) => {
                        // Every lock may have landed (a lost one too): the staging record must turn
                        // aborted before anyone hears so, or a recovery could find it committed.
                        run.why = Some(why);
                        run.rounds = run.rounds.saturating_add(1);
                        run.phase = Phase::Deciding;
                        let decide = Step::Decide {
                            txn,
                            outcome: Outcome::Aborted,
                        };
                        next.push((run.anchor, Kind::Decide, decide));
                    }
                    (None, Protocol::TwoPhase) if all_in => {
                        run.rounds = run.rounds.saturating_add(1);
                        run.phase = Phase::Deciding;
                        let decide = Step::Decide {
                            txn,
                            outcome: Outcome::Committed,
                        };
                        next.push((run.anchor, Kind::Decide, decide));
                    }
                    (None, Protocol::Parallel) if all_in => {
                        // Staging and every lock in place: committed. The explicit commit and the
                        // resolutions go out together; a recovery meanwhile finds every lock (or
                        // its committed done row) and commits too.
                        answer = Some(Answer::Committed {
                            reads: std::mem::take(&mut run.reads),
                        });
                        run.phase = Phase::Resolving {
                            outcome: Outcome::Committed,
                            waiting: run.groups.clone(),
                            deciding: true,
                        };
                        let decide = Step::Decide {
                            txn,
                            outcome: Outcome::Committed,
                        };
                        next.push((run.anchor, Kind::Decide, decide));
                        resolve_all(run, Outcome::Committed, &mut next);
                    }
                    (None, _) => {}
                }
            }
            (Kind::Decide | Kind::Recover, Ok(Reply::Decided(outcome))) => match &mut run.phase {
                Phase::Resolving {
                    outcome: answered,
                    deciding,
                    ..
                } if *deciding && sent.kind == Kind::Decide => {
                    if *answered != outcome {
                        return Err(broken(&format!(
                            "the anchor decided {outcome:?} after the client heard {answered:?}"
                        )));
                    }
                    *deciding = false;
                }
                Phase::Deciding | Phase::Recovering => {
                    if !run.answered {
                        answer = Some(match outcome {
                            Outcome::Committed => Answer::Committed {
                                reads: std::mem::take(&mut run.reads),
                            },
                            Outcome::Aborted => {
                                Answer::Aborted(run.why.take().unwrap_or(Why::Late))
                            }
                        });
                    }
                    run.phase = Phase::Resolving {
                        outcome,
                        waiting: run.groups.clone(),
                        deciding: false,
                    };
                    resolve_all(run, outcome, &mut next);
                }
                _ => return Err(broken("a decision came back that no step waited for")),
            },
            (Kind::Recover, Ok(Reply::Staging(participants))) => {
                run.groups.extend(participants.iter().copied());
                run.phase = Phase::Preventing {
                    waiting: participants.iter().copied().collect(),
                    absent: false,
                };
                for group in participants {
                    next.push((group, Kind::Prevent, Step::Prevent { txn }));
                }
            }
            (Kind::Prevent, Ok(Reply::Present(present))) => {
                let Phase::Preventing { waiting, absent } = &mut run.phase else {
                    return Err(broken("a prevention came back outside recovery"));
                };
                waiting.remove(&sent.group);
                *absent |= !present;
                if waiting.is_empty() {
                    // Committed exactly when every participant held its locks: Parallel Commits'
                    // implicit commit. A participant without them now never will.
                    let outcome = if *absent {
                        Outcome::Aborted
                    } else {
                        Outcome::Committed
                    };
                    run.phase = Phase::Deciding;
                    next.push((run.anchor, Kind::Decide, Step::Decide { txn, outcome }));
                }
            }
            (Kind::Resolve, Ok(Reply::Resolved)) => {
                if let Phase::Resolving { waiting, .. } = &mut run.phase {
                    waiting.remove(&sent.group);
                }
            }
            (kind, Err(Lost)) => next.push((sent.group, kind, sent.step)),
            (_, Ok(Reply::Late)) => {
                return Err(broken(
                    "a group says a transaction the coordinator still runs is finished",
                ));
            }
            (kind, Ok(other)) => return Err(unexpected(kind, &other)),
        }
        if let Some(answer) = answer {
            run.answered = true;
            let rounds = run.rounds;
            match answer {
                Answer::Committed { .. } => {
                    self.counts.committed = one_more(self.counts.committed)?
                }
                Answer::Aborted(_) => self.counts.aborted = one_more(self.counts.aborted)?,
                Answer::Unknown => self.counts.unknown = one_more(self.counts.unknown)?,
            }
            out.push(Event::Answer {
                txn,
                answer,
                rounds,
            });
        }
        for (group, kind, step) in next {
            self.send(out, group, Some(txn), kind, step)?;
        }
        self.finish(out, txn)
    }

    /// Ends `txn` once every step of it has come back and every group has resolved it.
    fn finish(&mut self, out: &mut Vec<Event>, txn: TxnId) -> Result<(), Refusal> {
        let Some(run) = self.runs.get(&txn) else {
            return Ok(());
        };
        let Phase::Resolving {
            outcome,
            waiting,
            deciding,
        } = &run.phase
        else {
            return Ok(());
        };
        if run.outstanding != 0 || !waiting.is_empty() || *deciding {
            return Ok(());
        }
        let (outcome, recovered) = (*outcome, run.recovered);
        self.runs.remove(&txn);
        if recovered {
            match outcome {
                Outcome::Committed => {
                    self.counts.recovered_committed = one_more(self.counts.recovered_committed)?;
                }
                Outcome::Aborted => {
                    self.counts.recovered_aborted = one_more(self.counts.recovered_aborted)?;
                }
            }
        }
        out.push(Event::Finished {
            txn,
            outcome,
            recovered,
        });
        self.ready_if_recovered(out);
        Ok(())
    }
}
