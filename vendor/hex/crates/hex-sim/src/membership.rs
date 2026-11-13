//! SWIM membership with Lifeguard, as a deterministic discrete-event model of memberlist's protocol and
//! constants: S8's competitor (`docs/design/hex-sim.md` H41). It is not hyper-swim's detector, which
//! times each pair from its measured round trips (NFD-E) rather than memberlist's fixed period and
//! timeout. One seed, virtual nanoseconds, one SplitMix64 generator and B-tree maps
//! decide a run (H1); events at equal times run in the order they were scheduled; the event queue,
//! each member's relayed probes and the run's steps are bounded, past which a typed [`SimError`]
//! (H5). Handlers schedule events and never call one another in a cycle (CLAUDE.md §11).
//!
//! **The protocol** (Das, Gupta and Motivala, "SWIM", DSN 2002, §3-§4; memberlist v0.7.0 as the
//! reference implementation). Each member probes one other per probe interval, in randomized
//! round-robin: it shuffles the others once per round (SWIM §4.3) and skips members it holds dead.
//! A ping wants an ack within the probe timeout; without one the prober asks `indirect_checks`
//! random members it holds alive to ping the target for it (ping-req) and waits until the interval
//! ends for an indirect ack. Without one it suspects the target at the incarnation it knows and
//! broadcasts the suspicion. Every member that hears a suspicion starts its own suspicion timer, as
//! memberlist's do; a member that hears itself suspected (or declared dead) refutes, raising its
//! incarnation past the accused one and broadcasting alive. A suspicion whose timer runs out before
//! a refutation is heard is declared dead and broadcast; a member hearing dead marks it dead unless
//! it already holds a later incarnation. Alive at a higher incarnation clears suspect and dead.
//!
//! **Timing** is memberlist's `DefaultLANConfig` (config.go:312-320): probe interval 1 s, probe
//! timeout 500 ms, 3 indirect checks, suspicion multiplier 4, suspicion max-timeout multiplier 6,
//! awareness max multiplier 8. The suspicion minimum is `SuspicionMult × max(1, log10(n)) ×
//! ProbeInterval` with the scale truncated to thousandths (util.go:71-73); the maximum is
//! `SuspicionMaxTimeoutMult × min` (state.go:1222).
//!
//! **Lifeguard** (Dadgar, Phillips and Currey, "Lifeguard: Local Health Awareness for More Accurate
//! Failure Detection", arXiv:1707.00788v2), each part switched off together for the SWIM baseline:
//! - *Local health multiplier* (LHM, `0..awareness_max_multiplier`) scales a member's probe timeout
//!   and interval by `LHM + 1`. It rises by the expected nacks that didn't come when a probe fails
//!   (by 1 when none were expected), and by 1 when the member refutes a suspicion or death of itself;
//!   it falls by 1 on a probe that got an ack.
//! - *Nacks*: a relay that hasn't heard the target within 80% of the probe timeout tells the prober so;
//!   a prober missing nacks learns that its own processing, not the target, is slow.
//! - *Buddy system*: a ping to a member the prober suspects carries the suspicion, so the target
//!   refutes as soon as it is probed.
//! - *Dynamic suspicion*: a suspicion's timeout starts at the maximum and shrinks with independent
//!   confirmations `c` towards the minimum, `max − (log(c+1)/log(k+1))·(max − min)`, truncated to
//!   milliseconds and floored at the minimum (suspicion.go:90-91), with `k = SuspicionMult − 2`
//!   expected confirmations (state.go:1210). Computed here in fixed point, without floats.
//!
//! The SWIM baseline's suspicion timeout is the minimum, fixed; it has no LHM, nack or buddy.
//!
//! **Simplifications**, each stated: gossip is modelled as direct delivery of every broadcast to
//! every other member after its own one-way delay, not piggybacked on probes, so dissemination time
//! is one network delay; the cluster size `n` is fixed (memberlist estimates it from its live view);
//! there is no TCP fallback ping. A message's one-way delay is drawn per message from
//! `net_min_ns..=net_max_ns`.
//!
//! **Faults.** A crashed member neither answers nor sends; a message to it is counted lost. A slow
//! member processes each message it receives `delay_ns` after it arrives and in arrival order (a
//! queue: Lifeguard's "slow message processing"); its own timers run on time.

use std::collections::{BTreeMap, BTreeSet};

use crate::error::SimError;
use crate::model::Time;
use crate::rng::Rng;
use crate::trace::TraceHash;

/// Members a run holds: every member keeps a view of all `n`, so memory is `n²` entries (16 bytes
/// each, 64 MiB at this bound).
pub const MAX_MEMBERS: u32 = 2_048;

/// The largest suspicion multiplier accepted: the confirmation table holds `SuspicionMult − 1`
/// timeouts and each suspicion at most that many confirmers.
pub const MAX_SUSPICION_MULT: u32 = 64;

/// Fractional bits of the fixed-point logarithms.
const FRAC: u32 = 60;
const MILLISECOND: Time = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    pub members: u32,
    /// Lifeguard's LHM, nacks, buddy system and dynamic suspicion; off is the SWIM baseline.
    pub lifeguard: bool,
    /// With Lifeguard, a gossiped suspicion confirms one already held only when its sender was in good
    /// local health (LHM 0) as it sent it. Off is Lifeguard as published, where any other member's
    /// suspicion counts: slow members, each suspecting healthy peers on its own, then confirm each
    /// other's false suspicions and shrink the timeout past the refutation.
    pub health_gated_confirmations: bool,
    pub probe_interval_ns: Time,
    pub probe_timeout_ns: Time,
    pub indirect_checks: u32,
    pub suspicion_mult: u32,
    pub suspicion_max_timeout_mult: u32,
    pub awareness_max_multiplier: u32,
    /// A relay nacks when it hasn't heard the target by this share of the probe timeout.
    pub nack_per_mille: u64,
    pub net_min_ns: Time,
    pub net_max_ns: Time,
    /// The run ends at this virtual time.
    pub end_ns: Time,
    /// End early once every crashed member is dead at every live member.
    pub until_detected: bool,
    pub max_events: usize,
    pub max_steps: u64,
    /// Ping-reqs one member relays at once.
    pub max_relayed: usize,
}

impl Config {
    /// memberlist v0.7.0's `DefaultLANConfig` (config.go:312-320), Lifeguard's 80% nack point
    /// (arXiv:1707.00788v2 §IV), and a LAN's one-way delay of 100 µs to 2 ms (chosen for the model,
    /// not measured).
    pub fn lan(members: u32, lifeguard: bool) -> Self {
        Self {
            members,
            lifeguard,
            health_gated_confirmations: false,
            probe_interval_ns: 1_000_000_000,
            probe_timeout_ns: 500_000_000,
            indirect_checks: 3,
            suspicion_mult: 4,
            suspicion_max_timeout_mult: 6,
            awareness_max_multiplier: 8,
            nack_per_mille: 800,
            net_min_ns: 100_000,
            net_max_ns: 2_000_000,
            end_ns: 120_000_000_000,
            until_detected: false,
            max_events: 1 << 22,
            max_steps: 1 << 32,
            max_relayed: 64,
        }
    }

    /// The suspicion timeout's minimum and maximum for this cluster size (util.go:71-73,
    /// state.go:1222).
    pub fn suspicion_bounds(&self) -> Result<(Time, Time), SimError> {
        let scale = milli_log10(u64::from(self.members))?.max(1_000);
        let min = Time::from(self.suspicion_mult)
            .checked_mul(scale)
            .and_then(|x| x.checked_mul(self.probe_interval_ns))
            .ok_or(SimError::TimeOverflow)?
            .checked_div(1_000)
            .ok_or(SimError::TimeOverflow)?;
        let max = Time::from(self.suspicion_max_timeout_mult)
            .checked_mul(min)
            .ok_or(SimError::TimeOverflow)?;
        Ok((min, max))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// The member stops answering and sending at `at_ns`.
    Crash { member: u32, at_ns: Time },
    /// Every message the member receives in `from_ns..until_ns` is processed `delay_ns` after it
    /// arrives, in arrival order.
    Slow {
        member: u32,
        from_ns: Time,
        until_ns: Time,
        delay_ns: Time,
    },
}

/// When a crashed member was found: the first suspicion and death declaration of it by any member,
/// and the time every live member held it dead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Detection {
    pub member: u32,
    pub crashed_at: Time,
    pub first_suspected: Option<Time>,
    pub first_dead: Option<Time>,
    pub all_dead: Option<Time>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// One per crash fault, in the order the faults were given.
    pub crashes: Vec<Detection>,
    /// Death declarations of healthy members (never crashed, never slow), one per (declarer, target):
    /// Lifeguard's false positives.
    pub false_positives: u64,
    /// Distinct healthy members declared dead by anyone.
    pub false_positive_members: u64,
    /// Death declarations of slow (never crashed) members.
    pub slow_declared: u64,
    /// Suspicions raised by a member's own failed probe.
    pub suspicions: u64,
    pub refutations: u64,
    pub messages: u64,
    /// Messages that reached a crashed member.
    pub lost: u64,
    /// Acks, indirect acks and nacks that arrived after their probe ended.
    pub late: u64,
    pub steps: u64,
    pub end: Time,
    pub trace_hash: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Alive,
    Suspect,
    Dead,
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    state: State,
    incarnation: u64,
}

#[derive(Debug)]
struct Probe {
    target: u32,
    seq: u64,
    acked: bool,
    expected_nacks: u32,
    nacks: u32,
}

#[derive(Debug)]
struct Relay {
    requester: u32,
    seq: u64,
    acked: bool,
}

#[derive(Debug)]
struct Suspicion {
    start: Time,
    /// The suspecter that started it and every confirmer since, at most `k + 1`.
    seen: BTreeSet<u32>,
    confirmations: usize,
    generation: u64,
}

#[derive(Debug, Clone, Copy)]
struct SlowWindow {
    from: Time,
    until: Time,
    delay: Time,
}

#[derive(Debug)]
struct Member {
    view: Vec<Entry>,
    order: Vec<u32>,
    next: usize,
    lhm: u32,
    seq: u64,
    probe: Option<Probe>,
    relayed: BTreeMap<u64, Relay>,
    suspicions: BTreeMap<u32, Suspicion>,
    crashed: bool,
    /// When the member's last received message is processed: later ones wait their turn.
    busy_until: Time,
    slow: Vec<SlowWindow>,
}

#[derive(Debug, Clone, Copy)]
enum Msg {
    Ping {
        seq: u64,
        buddy: Option<u64>,
    },
    Ack {
        seq: u64,
    },
    PingReq {
        seq: u64,
        target: u32,
    },
    IndirectAck {
        seq: u64,
    },
    Nack {
        seq: u64,
    },
    /// `healthy`: the sender's LHM was 0 as it sent it.
    Suspect {
        target: u32,
        incarnation: u64,
        healthy: bool,
    },
    Alive {
        target: u32,
        incarnation: u64,
    },
    Dead {
        target: u32,
        incarnation: u64,
    },
}

impl Msg {
    fn words(self) -> [u64; 4] {
        match self {
            Self::Ping { seq, buddy } => [
                1,
                seq,
                u64::from(buddy.is_some()),
                buddy.unwrap_or_default(),
            ],
            Self::Ack { seq } => [2, seq, 0, 0],
            Self::PingReq { seq, target } => [3, seq, u64::from(target), 0],
            Self::IndirectAck { seq } => [4, seq, 0, 0],
            Self::Nack { seq } => [5, seq, 0, 0],
            Self::Suspect {
                target,
                incarnation,
                healthy,
            } => [6, u64::from(target), incarnation, u64::from(healthy)],
            Self::Alive {
                target,
                incarnation,
            } => [7, u64::from(target), incarnation, 0],
            Self::Dead {
                target,
                incarnation,
            } => [8, u64::from(target), incarnation, 0],
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Event {
    Tick {
        member: u32,
    },
    ProbeTimeout {
        member: u32,
        seq: u64,
    },
    ProbeEnd {
        member: u32,
        seq: u64,
    },
    Deliver {
        from: u32,
        to: u32,
        msg: Msg,
    },
    Process {
        from: u32,
        to: u32,
        msg: Msg,
    },
    RelayNack {
        relay: u32,
        seq: u64,
    },
    RelayExpire {
        relay: u32,
        seq: u64,
    },
    SuspicionTimeout {
        observer: u32,
        target: u32,
        generation: u64,
    },
    Crash {
        member: u32,
    },
}

impl Event {
    fn words(self) -> [u64; 7] {
        let message = |tag: u64, from: u32, to: u32, msg: Msg| {
            let [a, b, c, d] = msg.words();
            [tag, u64::from(from), u64::from(to), a, b, c, d]
        };
        match self {
            Self::Tick { member } => [1, u64::from(member), 0, 0, 0, 0, 0],
            Self::ProbeTimeout { member, seq } => [2, u64::from(member), seq, 0, 0, 0, 0],
            Self::ProbeEnd { member, seq } => [3, u64::from(member), seq, 0, 0, 0, 0],
            Self::Deliver { from, to, msg } => message(4, from, to, msg),
            Self::Process { from, to, msg } => message(5, from, to, msg),
            Self::RelayNack { relay, seq } => [6, u64::from(relay), seq, 0, 0, 0, 0],
            Self::RelayExpire { relay, seq } => [7, u64::from(relay), seq, 0, 0, 0, 0],
            Self::SuspicionTimeout {
                observer,
                target,
                generation,
            } => [
                8,
                u64::from(observer),
                u64::from(target),
                generation,
                0,
                0,
                0,
            ],
            Self::Crash { member } => [9, u64::from(member), 0, 0, 0, 0, 0],
        }
    }
}

/// `log2(x)` in fixed point with [`FRAC`] fractional bits, truncated: the integer part from the
/// leading bit, then one fractional bit per squaring of the mantissa (a bounded loop of `FRAC`).
fn log2_fixed(x: u64) -> Result<u128, SimError> {
    let refused = SimError::Config("a logarithm's argument");
    let int = 63u32
        .checked_sub(x.leading_zeros())
        .ok_or(refused.clone())?;
    let one = 1u128.checked_shl(FRAC).ok_or(refused.clone())?;
    let two = one.checked_mul(2).ok_or(refused.clone())?;
    let mut y = u128::from(x)
        .checked_shl(FRAC)
        .and_then(|v| v.checked_shr(int))
        .ok_or(refused.clone())?;
    let mut result = u128::from(int).checked_shl(FRAC).ok_or(refused.clone())?;
    let mut bit = one.checked_shr(1).ok_or(refused.clone())?;
    for _ in 0..FRAC {
        y = y
            .checked_mul(y)
            .and_then(|v| v.checked_shr(FRAC))
            .ok_or(refused.clone())?;
        if y >= two {
            y = y.checked_shr(1).ok_or(refused.clone())?;
            result = result.checked_add(bit).ok_or(refused.clone())?;
        }
        bit = bit.checked_shr(1).ok_or(refused.clone())?;
    }
    Ok(result)
}

/// `floor(1000 · log10(n))`, `0` for `n ≤ 1` (memberlist's `max(1, n)`): exact at powers of ten,
/// elsewhere from the fixed-point logarithms, whose truncation error (below 2^-50) moves no other
/// `n` up to [`MAX_MEMBERS`] across a thousandth.
fn milli_log10(n: u64) -> Result<u64, SimError> {
    if n <= 1 {
        return Ok(0);
    }
    let mut power = 1u64;
    let mut exponent = 0u64;
    while power < n {
        match power.checked_mul(10) {
            Some(next) => {
                power = next;
                exponent = exponent.checked_add(1).ok_or(SimError::TimeOverflow)?;
            }
            None => break,
        }
    }
    if power == n {
        return exponent.checked_mul(1_000).ok_or(SimError::TimeOverflow);
    }
    let milli = log2_fixed(n)?
        .checked_mul(1_000)
        .and_then(|v| v.checked_div(log2_fixed(10).ok()?))
        .ok_or(SimError::Config("a logarithm's argument"))?;
    u64::try_from(milli).map_err(|_| SimError::Config("a logarithm's argument"))
}

/// The suspicion timeout after `c` confirmations, for each `c` in `0..=k`, and the minimum.
fn confirmation_table(min: Time, max: Time, k: u32) -> Result<Vec<Time>, SimError> {
    if k == 0 {
        return Ok(vec![min]);
    }
    let refused = SimError::Config("the suspicion timeouts");
    let span = u128::from(max.checked_sub(min).ok_or(refused.clone())?);
    let denominator = log2_fixed(u64::from(k).checked_add(1).ok_or(refused.clone())?)?;
    let mut table = Vec::with_capacity(usize::try_from(k).map_err(|_| refused.clone())?);
    for c in 0..=u64::from(k) {
        let fraction = log2_fixed(c.checked_add(1).ok_or(refused.clone())?)?
            .checked_shl(FRAC)
            .and_then(|v| v.checked_div(denominator))
            .ok_or(refused.clone())?;
        let cut = fraction
            .checked_mul(span)
            .and_then(|v| v.checked_shr(FRAC))
            .and_then(|v| Time::try_from(v).ok())
            .ok_or(refused.clone())?;
        let raw = max.checked_sub(cut).ok_or(refused.clone())?;
        let truncated = raw
            .checked_div(MILLISECOND)
            .and_then(|v| v.checked_mul(MILLISECOND))
            .ok_or(refused.clone())?;
        table.push(truncated.max(min));
    }
    Ok(table)
}

fn index(member: u32) -> Result<usize, SimError> {
    usize::try_from(member).map_err(|_| SimError::Config("member index"))
}

fn bump(counter: &mut u64) -> Result<(), SimError> {
    *counter = counter
        .checked_add(1)
        .ok_or(SimError::Config("a counter overflowed"))?;
    Ok(())
}

/// Scale `base` by a member's local health, `LHM + 1`.
fn scaled(base: Time, lhm: u32) -> Result<Time, SimError> {
    base.checked_mul(
        Time::from(lhm)
            .checked_add(1)
            .ok_or(SimError::TimeOverflow)?,
    )
    .ok_or(SimError::TimeOverflow)
}

struct Sim<'a> {
    config: &'a Config,
    table: Vec<Time>,
    k: usize,
    members: Vec<Member>,
    /// Never crashed and never slow.
    healthy: Vec<bool>,
    slow: Vec<bool>,
    /// Crashed member to its [`Detection`]'s index.
    crash_index: BTreeMap<u32, usize>,
    undetected: usize,
    false_positive_members: BTreeSet<u32>,
    queue: BTreeMap<(Time, u64), Event>,
    order: u64,
    generation: u64,
    now: Time,
    rng: Rng,
    hash: TraceHash,
    report: Report,
}

/// Run `config.members` members under `faults` from `seed` until `config.end_ns` (or, with
/// `until_detected`, until every crashed member is dead at every live member).
pub fn run(config: &Config, faults: &[Fault], seed: u64) -> Result<Report, SimError> {
    if config.members < 2 || config.members > MAX_MEMBERS {
        return Err(SimError::Config("members must be in 2..=MAX_MEMBERS"));
    }
    if config.probe_timeout_ns == 0 || config.probe_timeout_ns >= config.probe_interval_ns {
        return Err(SimError::Config(
            "the probe timeout must be positive and shorter than the probe interval",
        ));
    }
    if config.suspicion_mult == 0
        || config.suspicion_mult > MAX_SUSPICION_MULT
        || config.suspicion_max_timeout_mult == 0
        || config.awareness_max_multiplier == 0
        || config.net_min_ns > config.net_max_ns
        || config.nack_per_mille > 1_000
    {
        return Err(SimError::Config("a membership parameter is out of range"));
    }
    let (min, max) = config.suspicion_bounds()?;
    let k = config.suspicion_mult.saturating_sub(2);
    let table = if config.lifeguard {
        confirmation_table(min, max, k)?
    } else {
        vec![min]
    };
    let n = index(config.members)?;
    let mut sim = Sim {
        config,
        k: table.len().saturating_sub(1),
        table,
        members: Vec::with_capacity(n),
        healthy: vec![true; n],
        slow: vec![false; n],
        crash_index: BTreeMap::new(),
        undetected: 0,
        false_positive_members: BTreeSet::new(),
        queue: BTreeMap::new(),
        order: 0,
        generation: 0,
        now: 0,
        rng: Rng::new(seed),
        hash: TraceHash::default(),
        report: Report::default(),
    };
    for m in 0..config.members {
        let mut order: Vec<u32> = (0..config.members).filter(|&o| o != m).collect();
        sim.shuffle(&mut order)?;
        sim.members.push(Member {
            view: vec![
                Entry {
                    state: State::Alive,
                    incarnation: 0,
                };
                n
            ],
            order,
            next: 0,
            lhm: 0,
            seq: 0,
            probe: None,
            relayed: BTreeMap::new(),
            suspicions: BTreeMap::new(),
            crashed: false,
            busy_until: 0,
            slow: Vec::new(),
        });
    }
    for fault in faults {
        match *fault {
            Fault::Crash { member, at_ns } => {
                *sim.healthy
                    .get_mut(index(member)?)
                    .ok_or(SimError::Config("a fault names no member"))? = false;
                if !sim.crash_index.contains_key(&member) {
                    sim.crash_index.insert(member, sim.report.crashes.len());
                    sim.report.crashes.push(Detection {
                        member,
                        crashed_at: at_ns,
                        first_suspected: None,
                        first_dead: None,
                        all_dead: None,
                    });
                    sim.schedule(at_ns, Event::Crash { member })?;
                }
            }
            Fault::Slow {
                member,
                from_ns,
                until_ns,
                delay_ns,
            } => {
                let at = index(member)?;
                *sim.healthy
                    .get_mut(at)
                    .ok_or(SimError::Config("a fault names no member"))? = false;
                *sim.slow
                    .get_mut(at)
                    .ok_or(SimError::Config("a fault names no member"))? = true;
                sim.member_mut(member)?.slow.push(SlowWindow {
                    from: from_ns,
                    until: until_ns,
                    delay: delay_ns,
                });
            }
        }
    }
    for &member in sim.crash_index.keys() {
        if let Some(slow) = sim.slow.get_mut(index(member)?) {
            *slow = false;
        }
    }
    sim.undetected = sim.report.crashes.len();
    let last_phase = config
        .probe_interval_ns
        .checked_sub(1)
        .ok_or(SimError::TimeOverflow)?;
    for member in 0..config.members {
        let phase = sim.rng.range(0, last_phase);
        sim.schedule(phase, Event::Tick { member })?;
    }
    while let Some(((at, _), event)) = sim.queue.pop_first() {
        if at > config.end_ns {
            break;
        }
        bump(&mut sim.report.steps)?;
        if sim.report.steps > config.max_steps {
            return Err(SimError::StepBudget(config.max_steps));
        }
        sim.now = at;
        sim.hash.word(at);
        for word in event.words() {
            sim.hash.word(word);
        }
        sim.handle(event)?;
        if config.until_detected && !sim.report.crashes.is_empty() && sim.undetected == 0 {
            break;
        }
    }
    sim.report.false_positive_members = u64::try_from(sim.false_positive_members.len())
        .map_err(|_| SimError::Config("a counter overflowed"))?;
    sim.report.end = sim.now;
    sim.report.trace_hash = sim.hash.finish();
    Ok(sim.report)
}

impl Sim<'_> {
    fn schedule(&mut self, at: Time, event: Event) -> Result<(), SimError> {
        if self.queue.len() >= self.config.max_events {
            return Err(SimError::Full {
                what: "the event queue",
                bound: self.config.max_events,
            });
        }
        self.order = self
            .order
            .checked_add(1)
            .ok_or(SimError::StepBudget(self.config.max_steps))?;
        self.queue.insert((at, self.order), event);
        Ok(())
    }

    fn after(&self, delay: Time) -> Result<Time, SimError> {
        self.now.checked_add(delay).ok_or(SimError::TimeOverflow)
    }

    fn member(&self, member: u32) -> Result<&Member, SimError> {
        self.members
            .get(index(member)?)
            .ok_or(SimError::Config("a member that isn't in the run"))
    }

    fn member_mut(&mut self, member: u32) -> Result<&mut Member, SimError> {
        self.members
            .get_mut(index(member)?)
            .ok_or(SimError::Config("a member that isn't in the run"))
    }

    fn entry(&self, observer: u32, target: u32) -> Result<Entry, SimError> {
        self.member(observer)?
            .view
            .get(index(target)?)
            .copied()
            .ok_or(SimError::Config("a member that isn't in the run"))
    }

    fn entry_mut(&mut self, observer: u32, target: u32) -> Result<&mut Entry, SimError> {
        let at = index(target)?;
        self.member_mut(observer)?
            .view
            .get_mut(at)
            .ok_or(SimError::Config("a member that isn't in the run"))
    }

    /// Fisher-Yates over the run's generator.
    fn shuffle(&mut self, items: &mut [u32]) -> Result<(), SimError> {
        let mut i = items.len();
        while i > 1 {
            i = i.checked_sub(1).ok_or(SimError::Config("shuffle"))?;
            let bound = u64::try_from(i).map_err(|_| SimError::Config("shuffle"))?;
            let j = usize::try_from(self.rng.range(0, bound))
                .map_err(|_| SimError::Config("shuffle"))?;
            items.swap(i, j);
        }
        Ok(())
    }

    fn send(&mut self, from: u32, to: u32, msg: Msg) -> Result<(), SimError> {
        bump(&mut self.report.messages)?;
        let delay = self
            .rng
            .range(self.config.net_min_ns, self.config.net_max_ns);
        let at = self.after(delay)?;
        self.schedule(at, Event::Deliver { from, to, msg })
    }

    /// Gossip, modelled as direct delivery to every other member.
    fn broadcast(&mut self, from: u32, msg: Msg) -> Result<(), SimError> {
        for to in 0..self.config.members {
            if to != from {
                self.send(from, to, msg)?;
            }
        }
        Ok(())
    }

    fn handle(&mut self, event: Event) -> Result<(), SimError> {
        match event {
            Event::Tick { member } => self.tick(member),
            Event::ProbeTimeout { member, seq } => self.probe_timeout(member, seq),
            Event::ProbeEnd { member, seq } => self.probe_end(member, seq),
            Event::Deliver { from, to, msg } => self.deliver(from, to, msg),
            Event::Process { from, to, msg } => {
                if self.member(to)?.crashed {
                    bump(&mut self.report.lost)
                } else {
                    self.process(from, to, msg)
                }
            }
            Event::RelayNack { relay, seq } => self.relay_nack(relay, seq),
            Event::RelayExpire { relay, seq } => {
                self.member_mut(relay)?.relayed.remove(&seq);
                Ok(())
            }
            Event::SuspicionTimeout {
                observer,
                target,
                generation,
            } => self.suspicion_timeout(observer, target, generation),
            Event::Crash { member } => self.crash(member),
        }
    }

    /// The next member of the round-robin that isn't held dead, reshuffling at a round's end; at
    /// most two passes over the list.
    fn next_target(&mut self, member: u32) -> Result<Option<u32>, SimError> {
        let len = self.member(member)?.order.len();
        let budget = len.checked_mul(2).ok_or(SimError::Config("members"))?;
        for _ in 0..budget {
            if self.member(member)?.next >= len {
                let mut order = std::mem::take(&mut self.member_mut(member)?.order);
                self.shuffle(&mut order)?;
                let m = self.member_mut(member)?;
                m.order = order;
                m.next = 0;
            }
            let m = self.member_mut(member)?;
            let candidate = *m
                .order
                .get(m.next)
                .ok_or(SimError::Config("the probe order"))?;
            m.next = m
                .next
                .checked_add(1)
                .ok_or(SimError::Config("the probe order"))?;
            if self.entry(member, candidate)?.state != State::Dead {
                return Ok(Some(candidate));
            }
        }
        Ok(None)
    }

    fn tick(&mut self, member: u32) -> Result<(), SimError> {
        if self.member(member)?.crashed {
            return Ok(());
        }
        let lhm = self.member(member)?.lhm;
        let interval = scaled(self.config.probe_interval_ns, lhm)?;
        let timeout = scaled(self.config.probe_timeout_ns, lhm)?;
        if let Some(target) = self.next_target(member)? {
            let entry = self.entry(member, target)?;
            let buddy = if self.config.lifeguard && entry.state == State::Suspect {
                Some(entry.incarnation)
            } else {
                None
            };
            let m = self.member_mut(member)?;
            m.seq = m.seq.checked_add(1).ok_or(SimError::Config("probe seq"))?;
            let seq = m.seq;
            m.probe = Some(Probe {
                target,
                seq,
                acked: false,
                expected_nacks: 0,
                nacks: 0,
            });
            self.send(member, target, Msg::Ping { seq, buddy })?;
            let at = self.after(timeout)?;
            self.schedule(at, Event::ProbeTimeout { member, seq })?;
            let at = self.after(interval)?;
            self.schedule(at, Event::ProbeEnd { member, seq })?;
        }
        let at = self.after(interval)?;
        self.schedule(at, Event::Tick { member })
    }

    fn probe_timeout(&mut self, member: u32, seq: u64) -> Result<(), SimError> {
        let m = self.member(member)?;
        let target = match &m.probe {
            Some(p) if p.seq == seq && !p.acked && !m.crashed => p.target,
            _ => return Ok(()),
        };
        let mut candidates: Vec<u32> = Vec::new();
        for (other, entry) in (0..self.config.members).zip(m.view.iter()) {
            if other != member && other != target && entry.state == State::Alive {
                candidates.push(other);
            }
        }
        let want = usize::try_from(self.config.indirect_checks)
            .map_err(|_| SimError::Config("indirect checks"))?
            .min(candidates.len());
        // A partial Fisher-Yates: the first `want` of a uniform shuffle.
        for i in 0..want {
            let last = u64::try_from(candidates.len().saturating_sub(1))
                .map_err(|_| SimError::Config("indirect checks"))?;
            let low = u64::try_from(i).map_err(|_| SimError::Config("indirect checks"))?;
            let j = usize::try_from(self.rng.range(low, last))
                .map_err(|_| SimError::Config("indirect checks"))?;
            candidates.swap(i, j);
        }
        let relays: Vec<u32> = candidates.into_iter().take(want).collect();
        if self.config.lifeguard {
            let expected =
                u32::try_from(relays.len()).map_err(|_| SimError::Config("indirect checks"))?;
            if let Some(p) = self.member_mut(member)?.probe.as_mut() {
                p.expected_nacks = expected;
            }
        }
        for relay in relays {
            self.send(member, relay, Msg::PingReq { seq, target })?;
        }
        Ok(())
    }

    fn probe_end(&mut self, member: u32, seq: u64) -> Result<(), SimError> {
        let lifeguard = self.config.lifeguard;
        let cap = self.config.awareness_max_multiplier.saturating_sub(1);
        let m = self.member_mut(member)?;
        if m.crashed || m.probe.as_ref().is_none_or(|p| p.seq != seq) {
            return Ok(());
        }
        let Some(probe) = m.probe.take() else {
            return Ok(());
        };
        if probe.acked {
            if lifeguard {
                m.lhm = m.lhm.saturating_sub(1);
            }
            return Ok(());
        }
        if lifeguard {
            let delta = if probe.expected_nacks > 0 {
                probe.expected_nacks.saturating_sub(probe.nacks)
            } else {
                1
            };
            m.lhm = m.lhm.saturating_add(delta).min(cap);
        }
        bump(&mut self.report.suspicions)?;
        let incarnation = self.entry(member, probe.target)?.incarnation;
        let healthy = self.member(member)?.lhm == 0;
        if self.suspect(member, probe.target, incarnation, member, healthy)? {
            self.broadcast(
                member,
                Msg::Suspect {
                    target: probe.target,
                    incarnation,
                    healthy,
                },
            )?;
        }
        Ok(())
    }

    /// `observer` hears (or concludes) that `from` suspects `target` at `incarnation`, `from` in good
    /// local health or not. True when it starts a suspicion or counts a new confirmation.
    fn suspect(
        &mut self,
        observer: u32,
        target: u32,
        incarnation: u64,
        from: u32,
        from_healthy: bool,
    ) -> Result<bool, SimError> {
        let entry = self.entry(observer, target)?;
        if incarnation < entry.incarnation || entry.state == State::Dead {
            return Ok(false);
        }
        let now = self.now;
        let k = self.k;
        let deadline = if entry.state == State::Suspect {
            if !self.config.lifeguard || (self.config.health_gated_confirmations && !from_healthy) {
                return Ok(false);
            }
            self.generation = self
                .generation
                .checked_add(1)
                .ok_or(SimError::Config("suspicion generation"))?;
            let generation = self.generation;
            let Some(s) = self.member_mut(observer)?.suspicions.get_mut(&target) else {
                return Ok(false);
            };
            if s.confirmations >= k || s.seen.contains(&from) {
                return Ok(false);
            }
            s.seen.insert(from);
            s.confirmations = s
                .confirmations
                .checked_add(1)
                .ok_or(SimError::Config("confirmations"))?;
            s.generation = generation;
            let (start, confirmations) = (s.start, s.confirmations);
            let timeout = *self
                .table
                .get(confirmations)
                .ok_or(SimError::Config("the suspicion timeouts"))?;
            (
                start
                    .checked_add(timeout)
                    .ok_or(SimError::TimeOverflow)?
                    .max(now),
                generation,
            )
        } else {
            *self.entry_mut(observer, target)? = Entry {
                state: State::Suspect,
                incarnation,
            };
            self.generation = self
                .generation
                .checked_add(1)
                .ok_or(SimError::Config("suspicion generation"))?;
            let generation = self.generation;
            self.member_mut(observer)?.suspicions.insert(
                target,
                Suspicion {
                    start: now,
                    seen: BTreeSet::from([from]),
                    confirmations: 0,
                    generation,
                },
            );
            if self.member(target)?.crashed {
                let at = self.crash_index.get(&target).copied();
                if let Some(d) = at.and_then(|i| self.report.crashes.get_mut(i)) {
                    d.first_suspected.get_or_insert(now);
                }
            }
            let timeout = *self
                .table
                .first()
                .ok_or(SimError::Config("the suspicion timeouts"))?;
            (self.after(timeout)?, generation)
        };
        let (at, generation) = deadline;
        self.schedule(
            at,
            Event::SuspicionTimeout {
                observer,
                target,
                generation,
            },
        )?;
        Ok(true)
    }

    fn suspicion_timeout(
        &mut self,
        observer: u32,
        target: u32,
        generation: u64,
    ) -> Result<(), SimError> {
        let m = self.member_mut(observer)?;
        if m.crashed
            || m.suspicions
                .get(&target)
                .is_none_or(|s| s.generation != generation)
        {
            return Ok(());
        }
        m.suspicions.remove(&target);
        let incarnation = self.entry(observer, target)?.incarnation;
        *self.entry_mut(observer, target)? = Entry {
            state: State::Dead,
            incarnation,
        };
        let at = index(target)?;
        if self.healthy.get(at).copied().unwrap_or(false) {
            bump(&mut self.report.false_positives)?;
            self.false_positive_members.insert(target);
        } else if self.slow.get(at).copied().unwrap_or(false) {
            bump(&mut self.report.slow_declared)?;
        }
        if self.member(target)?.crashed {
            let now = self.now;
            let at = self.crash_index.get(&target).copied();
            if let Some(d) = at.and_then(|i| self.report.crashes.get_mut(i)) {
                d.first_dead.get_or_insert(now);
            }
        }
        self.check_all_dead(target)?;
        self.broadcast(
            observer,
            Msg::Dead {
                target,
                incarnation,
            },
        )
    }

    /// Record when every live member first holds the crashed `target` dead.
    fn check_all_dead(&mut self, target: u32) -> Result<(), SimError> {
        let Some(&at) = self.crash_index.get(&target) else {
            return Ok(());
        };
        if !self.member(target)?.crashed
            || self
                .report
                .crashes
                .get(at)
                .is_none_or(|d| d.all_dead.is_some())
        {
            return Ok(());
        }
        let column = index(target)?;
        let all = (0..self.config.members)
            .zip(self.members.iter())
            .filter(|&(o, m)| o != target && !m.crashed)
            .all(|(_, m)| m.view.get(column).is_some_and(|e| e.state == State::Dead));
        if all {
            let now = self.now;
            if let Some(d) = self.report.crashes.get_mut(at) {
                d.all_dead = Some(now);
            }
            self.undetected = self.undetected.saturating_sub(1);
        }
        Ok(())
    }

    fn crash(&mut self, member: u32) -> Result<(), SimError> {
        let m = self.member_mut(member)?;
        m.crashed = true;
        m.probe = None;
        m.relayed.clear();
        m.suspicions.clear();
        let targets: Vec<u32> = self.crash_index.keys().copied().collect();
        for target in targets {
            self.check_all_dead(target)?;
        }
        Ok(())
    }

    fn deliver(&mut self, from: u32, to: u32, msg: Msg) -> Result<(), SimError> {
        let now = self.now;
        let m = self.member_mut(to)?;
        if m.crashed {
            return bump(&mut self.report.lost);
        }
        let extra = m
            .slow
            .iter()
            .filter(|w| w.from <= now && now < w.until)
            .map(|w| w.delay)
            .max()
            .unwrap_or(0);
        let at = now
            .checked_add(extra)
            .ok_or(SimError::TimeOverflow)?
            .max(m.busy_until);
        m.busy_until = at;
        if at == now {
            self.process(from, to, msg)
        } else {
            self.schedule(at, Event::Process { from, to, msg })
        }
    }

    fn process(&mut self, from: u32, to: u32, msg: Msg) -> Result<(), SimError> {
        match msg {
            Msg::Ping { seq, buddy } => {
                if let Some(accused) = buddy
                    && self.config.lifeguard
                    && accused >= self.entry(to, to)?.incarnation
                {
                    self.refute(to, accused)?;
                }
                self.send(to, from, Msg::Ack { seq })
            }
            Msg::Ack { seq } => {
                let m = self.member_mut(to)?;
                if let Some(p) = m.probe.as_mut().filter(|p| p.seq == seq) {
                    p.acked = true;
                    return Ok(());
                }
                let Some(relay) = m.relayed.get_mut(&seq).filter(|r| !r.acked) else {
                    return bump(&mut self.report.late);
                };
                relay.acked = true;
                let (requester, seq) = (relay.requester, relay.seq);
                self.send(to, requester, Msg::IndirectAck { seq })
            }
            Msg::PingReq { seq, target } => {
                let bound = self.config.max_relayed;
                let m = self.member_mut(to)?;
                if m.relayed.len() >= bound {
                    return Err(SimError::Full {
                        what: "a member's relayed probes",
                        bound,
                    });
                }
                m.seq = m.seq.checked_add(1).ok_or(SimError::Config("probe seq"))?;
                let own = m.seq;
                m.relayed.insert(
                    own,
                    Relay {
                        requester: from,
                        seq,
                        acked: false,
                    },
                );
                self.send(
                    to,
                    target,
                    Msg::Ping {
                        seq: own,
                        buddy: None,
                    },
                )?;
                if self.config.lifeguard {
                    let nack_after = self
                        .config
                        .probe_timeout_ns
                        .checked_mul(self.config.nack_per_mille)
                        .ok_or(SimError::TimeOverflow)?
                        .checked_div(1_000)
                        .ok_or(SimError::TimeOverflow)?;
                    let at = self.after(nack_after)?;
                    self.schedule(
                        at,
                        Event::RelayNack {
                            relay: to,
                            seq: own,
                        },
                    )?;
                }
                let at = self.after(self.config.probe_timeout_ns)?;
                self.schedule(
                    at,
                    Event::RelayExpire {
                        relay: to,
                        seq: own,
                    },
                )
            }
            Msg::IndirectAck { seq } => {
                match self.member_mut(to)?.probe.as_mut().filter(|p| p.seq == seq) {
                    Some(p) => {
                        p.acked = true;
                        Ok(())
                    }
                    None => bump(&mut self.report.late),
                }
            }
            Msg::Nack { seq } => {
                match self
                    .member_mut(to)?
                    .probe
                    .as_mut()
                    .filter(|p| p.seq == seq && !p.acked)
                {
                    Some(p) => {
                        p.nacks = p.nacks.saturating_add(1).min(p.expected_nacks);
                        Ok(())
                    }
                    None => bump(&mut self.report.late),
                }
            }
            Msg::Suspect {
                target,
                incarnation,
                healthy,
            } => {
                if target == to {
                    if incarnation >= self.entry(to, to)?.incarnation {
                        self.refute(to, incarnation)?;
                    }
                    return Ok(());
                }
                self.suspect(to, target, incarnation, from, healthy)
                    .map(|_| ())
            }
            Msg::Alive {
                target,
                incarnation,
            } => {
                if target == to || incarnation <= self.entry(to, target)?.incarnation {
                    return Ok(());
                }
                *self.entry_mut(to, target)? = Entry {
                    state: State::Alive,
                    incarnation,
                };
                self.member_mut(to)?.suspicions.remove(&target);
                Ok(())
            }
            Msg::Dead {
                target,
                incarnation,
            } => {
                let entry = self.entry(to, target)?;
                if target == to {
                    if incarnation >= entry.incarnation {
                        self.refute(to, incarnation)?;
                    }
                    return Ok(());
                }
                if entry.state == State::Dead || incarnation < entry.incarnation {
                    return Ok(());
                }
                *self.entry_mut(to, target)? = Entry {
                    state: State::Dead,
                    incarnation,
                };
                self.member_mut(to)?.suspicions.remove(&target);
                self.check_all_dead(target)
            }
        }
    }

    fn relay_nack(&mut self, relay: u32, seq: u64) -> Result<(), SimError> {
        let m = self.member(relay)?;
        if m.crashed {
            return Ok(());
        }
        match m.relayed.get(&seq) {
            Some(r) if !r.acked => {
                let (requester, seq) = (r.requester, r.seq);
                self.send(relay, requester, Msg::Nack { seq })
            }
            _ => Ok(()),
        }
    }

    /// `member` heard itself accused at `accused`: it takes an incarnation past it and says so.
    fn refute(&mut self, member: u32, accused: u64) -> Result<(), SimError> {
        let incarnation = self
            .entry(member, member)?
            .incarnation
            .max(accused)
            .checked_add(1)
            .ok_or(SimError::Config("incarnation"))?;
        self.entry_mut(member, member)?.incarnation = incarnation;
        bump(&mut self.report.refutations)?;
        if self.config.lifeguard {
            let cap = self.config.awareness_max_multiplier.saturating_sub(1);
            let m = self.member_mut(member)?;
            m.lhm = m.lhm.saturating_add(1).min(cap);
        }
        self.broadcast(
            member,
            Msg::Alive {
                target: member,
                incarnation,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// S8: memberlist's scale at known sizes (util.go:71-73: `max(1, log10(n))`, in thousandths).
    #[test]
    fn the_node_scale_matches_memberlists() {
        assert_eq!(milli_log10(1).unwrap(), 0);
        assert_eq!(milli_log10(10).unwrap(), 1_000);
        assert_eq!(milli_log10(32).unwrap(), 1_505);
        assert_eq!(milli_log10(100).unwrap(), 2_000);
        assert_eq!(milli_log10(128).unwrap(), 2_107);
        assert_eq!(milli_log10(1_000).unwrap(), 3_000);
        assert_eq!(milli_log10(2_048).unwrap(), 3_311);
    }

    /// S8: do 32 members on the LAN defaults; expect min 6.02 s, max 36.12 s, and with k = 2 the
    /// timeouts max, max − log(2)/log(3)·(max − min) to the millisecond, and min.
    #[test]
    fn suspicion_timeouts_shrink_from_max_to_min() {
        let config = Config::lan(32, true);
        let (min, max) = config.suspicion_bounds().unwrap();
        assert_eq!(min, 6_020_000_000);
        assert_eq!(max, 36_120_000_000);
        let table = confirmation_table(min, max, 2).unwrap();
        // 36.12 − 0.63093 × 30.1 = 17.129 s.
        assert_eq!(table, vec![max, 17_129_000_000, min]);
    }
}
