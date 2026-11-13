//! hex-sim's runs of the learned charge (joint plan step 14, AC-T7.4.1; `docs/design/learned-charge.md`, "Measured in
//! hex-sim"): one node window by window, its VMs admitted, booked and shared by [`super::Node`] as a node's shell
//! drives it, and what the rule gives them measured: density, fairness and the guarantee.
//!
//! A VM's use is an agent's, thinking while its harness waits on its model and bursting in tool calls: a recorded
//! session's, where the run is given traces (`Scenario::traces`, `bench/charge/record_trace.py`), each VM playing one
//! in the order it started and ending once it has played, as hex-cli's `charge_fleet` replays them (item 3); else drawn
//! per VM from the published agent studies' ranges ([`Agent::draw`]), the generator's input space, as `generate.rs`'s
//! draws are. The kernel is modelled by what the rule gives it: CPU shared
//! by water-filling on bookings ([`super::shares`], as `cpu.weight` shares it), memory held at its budget where the
//! node holds it there (Linux's `memory.high`) and at its ceiling where nothing does yet (`Awaits(GuestRoom)`).

use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

use hex_api::CapacityResources as Amounts;

use super::trace::{Played, Trace};
use super::{Bounds, ChargeError, ClassKey, Cpu, Node, Resort, Sample};
use crate::model::Class;
use crate::rng::Rng;

/// What one agent's use is drawn from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Agent {
    /// Its tool calls' share of its time, per mille: 48 to 285, TraceLab's 4.8% of session wall-clock to
    /// AgentCgroup's 28.5% of sampled time (research f73, f75).
    pub tool_per_mille: u64,
    /// A tool call's mean length in windows: 1 to 2 s (AgentCgroup's bursts, f75).
    pub tool_windows: u64,
    /// Its CPU while it thinks: under 5% of a core (DSec: 90% of sandboxes use at most 5% of what they ask, f78).
    pub idle_milli: u64,
    /// Its CPU in a tool call: from a core to AgentCgroup's peak, 15.4 times its 13.2%-of-a-core average (f75).
    pub burst_milli: u64,
    /// Its memory while it thinks, and at a tool call's peak: 0.8 to 4 GiB at the peak (Chang's median 0.8 GB,
    /// AgentCgroup's 2 to 4 GB, f74, f75), the base below it.
    pub base_bytes: u64,
    pub peak_bytes: u64,
}

const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;
/// A tool call's memory peak: 0.8 GiB (Chang's median) to 4 GiB (AgentCgroup's most), and the least base below it.
const PEAK_LEAST: u64 = 800 * MIB;
const PEAK_MOST: u64 = 4 * GIB;
const BASE_LEAST: u64 = 256 * MIB;

impl Agent {
    /// One agent for a VM whose allocation is `ceiling` and enforcement window `window`.
    pub fn draw(rng: &mut Rng, ceiling: Amounts, window: Duration) -> Agent {
        let windows_in = |d: Duration| {
            u64::try_from(d.as_nanos().checked_div(window.as_nanos()).unwrap_or(0))
                .unwrap_or(u64::MAX)
                .max(1)
        };
        let peak = rng.range(PEAK_LEAST, PEAK_MOST).min(ceiling.mem_bytes);
        Agent {
            tool_per_mille: rng.range(48, 285),
            tool_windows: rng.range(
                windows_in(Duration::from_secs(1)),
                windows_in(Duration::from_secs(2)),
            ),
            idle_milli: rng.range(1, 50).min(ceiling.cpu_milli),
            // 15.4 x 132 milli-CPU = 2,032.8, to the milli-CPU above.
            burst_milli: rng.range(1_000, 2_033).min(ceiling.cpu_milli),
            base_bytes: rng.range(BASE_LEAST, peak.max(BASE_LEAST)).min(peak),
            peak_bytes: peak,
        }
    }
}

/// A VM's use, window by window.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Use {
    agent: Agent,
    tool: bool,
    left: u64,
    /// From this window it asks for its whole ceiling (the riser).
    rises_at: Option<u64>,
}

impl Use {
    fn new(agent: Agent, rises_at: Option<u64>) -> Self {
        Self {
            agent,
            tool: false,
            left: 0,
            rises_at,
        }
    }

    /// What it asks in window `at`: CPU in milli-CPU and memory in bytes. Each state lasts a draw uniform to twice
    /// its mean (as `generate.rs` draws gaps and lifetimes); thinking's mean is a tool call's times the share it
    /// leaves.
    fn ask(&mut self, rng: &mut Rng, at: u64, ceiling: Amounts) -> (u64, u64) {
        if self.rises_at.is_some_and(|from| at >= from) {
            return (ceiling.cpu_milli, ceiling.mem_bytes);
        }
        if self.left == 0 {
            self.tool = !self.tool;
            let a = self.agent;
            let mean = if self.tool {
                a.tool_windows
            } else {
                a.tool_windows
                    .saturating_mul(1_000u64.saturating_sub(a.tool_per_mille))
                    .checked_div(a.tool_per_mille)
                    .unwrap_or(a.tool_windows)
            };
            self.left = rng.range(1, mean.saturating_mul(2).max(1));
        }
        self.left = self.left.saturating_sub(1);
        if self.tool {
            (self.agent.burst_milli, self.agent.peak_bytes)
        } else {
            (self.agent.idle_milli, self.agent.base_bytes)
        }
    }
}

/// One run's inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scenario {
    pub seed: u64,
    pub window: Duration,
    /// The node's room for Hex: its fleet's budget.
    pub capacity: Amounts,
    /// Every VM's allocation: its ceiling.
    pub envelope: Amounts,
    pub vcpus: u32,
    /// VMs in line to start at the run's start.
    pub vms: usize,
    pub windows: u64,
    /// Each VM's life once started, in windows, drawn uniform between the two.
    pub life: (u64, u64),
    /// Whether the node holds each VM's memory at its budget (Linux's `memory.high`).
    pub rooms: bool,
    /// From window `at`, the `nth` VM to start asks for its whole ceiling; it lives the run out. One playing a trace
    /// rises there or once its trace has played, whichever comes first, as the driver's riser does.
    pub riser: Option<(u64, usize)>,
    /// Recorded traces its VMs replay (`super::trace`), the `n`th VM to start playing the `n`th, round the list, and
    /// ending once it has played (its life its trace's, `life` unread); none, and every VM's use is drawn.
    pub traces: Vec<Trace>,
}

/// What a run measured.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub admitted: u64,
    /// Starts refused for room (each a back-off, retried only once the node's room changed).
    pub refused: u64,
    /// The most VMs at once, and VMs summed over the windows (their mean is this over the windows).
    pub peak_live: usize,
    pub live_windows: u64,
    /// What today's envelope charge admits at once: the fewest of capacity over envelope, dimension by dimension.
    pub envelope_density: u64,
    /// VM-windows in which a VM was given less CPU than `min(its ask, its booking)` while the bookings fit the
    /// capacity: C4's guarantee broken. Zero.
    pub guarantee_misses: u64,
    /// Windows whose CPU asks passed the capacity, and in them the least and the sum of Jain's index of what VMs were
    /// given over what they asked, in parts per million.
    pub contended: u64,
    pub jain_least_ppm: u64,
    pub jain_sum_ppm: u128,
    /// CPU asked and CPU given, milli-CPU-windows.
    pub asked_milli: u128,
    pub given_milli: u128,
    /// VM-windows whose memory was held below what it asked, at its budget.
    pub held_back: u64,
    /// VMs the last resort ended, and of them those within their budget then: C5's rule broken. Zero.
    pub ended: u64,
    pub ended_within: u64,
    /// The riser: windows its charge passed its booking, VM-windows its neighbours were given less than
    /// `min(ask, booking)`, and whether the last resort ended it.
    pub riser_over: u64,
    pub neighbours_short: u64,
    pub riser_ended: bool,
    /// Starts refused for room while the riser lived: the node backing off.
    pub refused_while_rising: u64,
}

impl Report {
    /// Jain's mean index over the contended windows, in parts per million.
    pub fn jain_mean_ppm(&self) -> u64 {
        u64::try_from(
            self.jain_sum_ppm
                .checked_div(u128::from(self.contended))
                .unwrap_or(0),
        )
        .unwrap_or(u64::MAX)
    }
}

/// Jain, Chiu and Hawe's fairness index (DEC-TR-301, 1984) of `shares`, each in parts per million: `(sum x)^2 / (n
/// sum x^2)`, a million when every share is equal.
pub(super) fn jain(shares: &[u64]) -> u64 {
    let (sum, squares) = shares.iter().fold((0u128, 0u128), |(s, q), x| {
        let x = u128::from(*x);
        (s.saturating_add(x), q.saturating_add(x.saturating_mul(x)))
    });
    let n = u128::try_from(shares.len()).unwrap_or(u128::MAX);
    let parts = sum
        .saturating_mul(sum)
        .saturating_mul(1_000_000)
        .checked_div(n.saturating_mul(squares))
        .unwrap_or(1_000_000);
    u64::try_from(parts).unwrap_or(u64::MAX)
}

/// A VM's use in a run: drawn from an agent's ranges, or a recorded trace played.
enum Uses {
    Drawn(Use),
    Played(Played),
}

/// A VM in a run: its use, when it ends, whether it is the riser, and when it rises.
struct Live {
    used: Uses,
    ends: u64,
    riser: bool,
    rises_at: Option<u64>,
}

impl Live {
    /// What it asks in window `at`, of its `ceiling`, over `window`: CPU in milli-CPU and memory in bytes.
    fn ask(&mut self, rng: &mut Rng, at: u64, ceiling: Amounts, window: Duration) -> (u64, u64) {
        match &mut self.used {
            Uses::Drawn(used) => used.ask(rng, at, ceiling),
            Uses::Played(_) if self.rises_at.is_some_and(|from| at >= from) => {
                (ceiling.cpu_milli, ceiling.mem_bytes)
            }
            Uses::Played(played) => played.ask(window).unwrap_or((0, 0)),
        }
    }
}

/// Runs `s` and reports what it measured.
pub fn run(s: &Scenario) -> Result<Report, ChargeError> {
    let mut rng = Rng::new(s.seed);
    let mut node: Node<u32> = Node::new(
        s.window,
        s.capacity,
        Bounds {
            vms: s.vms.max(1),
            classes: 1,
        },
    )?;
    // Its VMs are admitted at their envelope; the sim models no VMM of their own, so none shows past it.
    let class = ClassKey::new(Class::Shards, [14; 32], s.envelope);
    let mut report = Report {
        envelope_density: envelope_density(s.capacity, s.envelope),
        jain_least_ppm: 1_000_000,
        ..Report::default()
    };
    let count = u32::try_from(s.vms).map_err(|_| ChargeError::Overflow("a run's VMs"))?;
    let mut line: VecDeque<u32> = (0..count).collect();
    let mut live: BTreeMap<u32, Live> = BTreeMap::new();
    let mut started = 0usize;
    let mut refused_at: Option<Amounts> = None;
    let riser_rises = s.riser.map(|(at, _)| at);
    for at in 0..s.windows {
        // Starts: the line's first, while the room has changed since its last refusal (B3: a refusal carries the
        // room, so a start is not asked again into the same room).
        while let Some(&id) = line.front() {
            let room = node.room()?;
            if refused_at == Some(room) {
                break;
            }
            match node.admit(id, class, 0, s.vcpus) {
                Ok(_) => {
                    line.pop_front();
                    let riser = s.riser.is_some_and(|(_, nth)| nth == started);
                    let traced = started
                        .checked_rem(s.traces.len())
                        .and_then(|n| s.traces.get(n));
                    let vm = match traced {
                        // Its trace's life: it is live the windows its trace lasts, ending in the last of them. The
                        // riser lives the run out, so it is there to rise, rising once its trace has played if that
                        // comes first.
                        Some(trace) => {
                            let played = Played::new(trace);
                            let windows = played.windows(s.window);
                            let (ends, rises_at) = if riser {
                                let played_out = at.saturating_add(windows);
                                (
                                    s.windows,
                                    Some(
                                        riser_rises.map_or(played_out, |from| from.min(played_out)),
                                    ),
                                )
                            } else {
                                (at.saturating_add(windows.saturating_sub(1)), None)
                            };
                            Live {
                                used: Uses::Played(played),
                                ends,
                                riser,
                                rises_at,
                            }
                        }
                        None => {
                            let agent = Agent::draw(&mut rng, s.envelope, s.window);
                            let drawn = rng.range(s.life.0, s.life.1);
                            // The riser lives the run out, so it is there to rise.
                            let (life, rises_at) = if riser {
                                (s.windows, riser_rises)
                            } else {
                                (drawn, None)
                            };
                            Live {
                                used: Uses::Drawn(Use::new(agent, rises_at)),
                                ends: at.saturating_add(life),
                                riser,
                                rises_at,
                            }
                        }
                    };
                    live.insert(id, vm);
                    started = started.saturating_add(1);
                    report.admitted = report.admitted.saturating_add(1);
                    refused_at = None;
                }
                Err(ChargeError::NoRoom { room, .. }) => {
                    report.refused = report.refused.saturating_add(1);
                    let rising = live
                        .values()
                        .any(|vm| vm.riser && vm.rises_at.is_some_and(|from| at >= from));
                    if rising {
                        report.refused_while_rising = report.refused_while_rising.saturating_add(1);
                    }
                    refused_at = Some(room);
                    break;
                }
                Err(why) => return Err(why),
            }
        }
        report.peak_live = report.peak_live.max(live.len());
        report.live_windows = report
            .live_windows
            .saturating_add(u64::try_from(live.len()).unwrap_or(u64::MAX));

        // What each asks, and the budgets and bookings in force for the window.
        let budgets: BTreeMap<u32, u64> = node
            .budgets()?
            .into_iter()
            .map(|(id, b)| (id, b.bytes))
            .collect();
        let mut asks: BTreeMap<u32, u64> = BTreeMap::new();
        let mut memory: BTreeMap<u32, u64> = BTreeMap::new();
        for (id, vm) in &mut live {
            let (cpu, mem) = vm.ask(&mut rng, at, s.envelope, s.window);
            asks.insert(*id, cpu.min(s.envelope.cpu_milli));
            memory.insert(*id, mem.min(s.envelope.mem_bytes));
        }
        let bookings: BTreeMap<u32, u64> = live
            .keys()
            .filter_map(|id| node.booking(id).map(|b| (*id, b.cpu_milli)))
            .collect();
        let weights_fit = bookings
            .values()
            .map(|b| (*b).max(1))
            .fold(0u64, u64::saturating_add)
            <= s.capacity.cpu_milli;
        let given = node.shares(s.capacity.cpu_milli, &asks)?;
        let rising = live
            .values()
            .any(|vm| vm.riser && vm.rises_at.is_some_and(|from| at >= from));
        let asked: u64 = asks.values().fold(0, |a, b| a.saturating_add(*b));
        let contended = asked > s.capacity.cpu_milli;
        let mut fractions = Vec::new();
        for (id, got) in &given {
            let ask = asks.get(id).copied().unwrap_or(0);
            let booking = bookings.get(id).copied().unwrap_or(0);
            report.asked_milli = report.asked_milli.saturating_add(u128::from(ask));
            report.given_milli = report.given_milli.saturating_add(u128::from(*got));
            let short = weights_fit && *got < ask.min(booking.max(1));
            if short {
                report.guarantee_misses = report.guarantee_misses.saturating_add(1);
            }
            let riser = live.get(id).is_some_and(|vm| vm.riser);
            if short && !riser && rising {
                report.neighbours_short = report.neighbours_short.saturating_add(1);
            }
            if contended && ask > 0 {
                let parts = u128::from(*got)
                    .saturating_mul(1_000_000)
                    .checked_div(u128::from(ask))
                    .unwrap_or(1_000_000);
                fractions.push(u64::try_from(parts).unwrap_or(u64::MAX));
            }
        }
        if contended {
            report.contended = report.contended.saturating_add(1);
            let index = jain(&fractions);
            report.jain_least_ppm = report.jain_least_ppm.min(index);
            report.jain_sum_ppm = report.jain_sum_ppm.saturating_add(u128::from(index));
        }

        // Each VM read as its node reads it: the CPU it was given, the memory it holds.
        for (id, got) in &given {
            let ask = memory.get(id).copied().unwrap_or(0);
            let held = match (s.rooms, budgets.get(id)) {
                (true, Some(budget)) => ask.min(*budget),
                _ => ask,
            };
            if held < ask {
                report.held_back = report.held_back.saturating_add(1);
            }
            let cpu = s
                .window
                .checked_mul(u32::try_from(*got).unwrap_or(u32::MAX))
                .and_then(|d| d.checked_div(1_000))
                .ok_or(ChargeError::Overflow("a simulated VM's CPU"))?;
            node.observe(
                *id,
                &Sample {
                    cpu: Cpu::Whole(cpu),
                    span: s.window,
                    mem_bytes: held,
                    disk_bytes: Some(0),
                    cap_milli: None,
                },
            )?;
        }
        node.settle()?;
        for (id, _) in node.over() {
            if live.get(&id).is_some_and(|vm| vm.riser) {
                report.riser_over = report.riser_over.saturating_add(1);
            }
        }

        // The last resort, then the VMs whose lives end.
        if let Resort::End { vm: id, .. } = node.last_resort()? {
            let within = node
                .budgets()?
                .into_iter()
                .any(|(vm, budget)| vm == id && budget.room >= 0);
            report.ended = report.ended.saturating_add(1);
            if within {
                report.ended_within = report.ended_within.saturating_add(1);
            }
            if live.get(&id).is_some_and(|vm| vm.riser) {
                report.riser_ended = true;
            }
            node.end(&id)?;
            live.remove(&id);
        }
        let done: Vec<u32> = live
            .iter()
            .filter(|(_, vm)| vm.ends <= at)
            .map(|(id, _)| *id)
            .collect();
        for id in done {
            node.end(&id)?;
            live.remove(&id);
        }
    }
    Ok(report)
}

/// What the envelope charge admits at once: the fewest whole envelopes the capacity holds in a dimension asked.
pub fn envelope_density(capacity: Amounts, envelope: Amounts) -> u64 {
    [
        (capacity.cpu_milli, envelope.cpu_milli),
        (capacity.mem_bytes, envelope.mem_bytes),
        (capacity.disk_bytes, envelope.disk_bytes),
    ]
    .into_iter()
    .filter(|(_, each)| *each > 0)
    .map(|(all, each)| all.checked_div(each).unwrap_or(0))
    .min()
    .unwrap_or(0)
}
