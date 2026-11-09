//! What the asking node's owner is given for each runner it might pick (LB08): the link's latency
//! and rate, measured where it has samples and estimated where it doesn't, each with its
//! uncertainty and what it rests on, and the link's outages. All integers, as the placement
//! kernel's expected-finish ranking takes them.
//!
//! **Latency**, in order of what it rests on:
//! 1. the pair's own link, measured: its filtered round trip, give or take its samples' spread;
//! 2. the legs of a relayed link (today every runner-to-runner link goes through the server, and
//!    direct links between nodes are to come, docs/p2p.md): each leg as its own end measured it,
//!    plus the relay's own time, measured on the relayed link's own round trips when there are
//!    some;
//! 3. the two nodes' coordinates, give or take the out-of-sample error each announced (none known:
//!    the estimate says its uncertainty is unknown).
//!
//! **Rate**: [`super::rate::estimate`]. **Outages**: [`super::stability::Path`], a relayed link's
//! from its legs'.

use serde::{Deserialize, Serialize};

use super::coord::{Announce, Coord};
use super::fixed;
use super::rate::{self, Access, Fit, Rate};
use super::stability::Path;

/// Where a latency comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Measured,
    Legs,
    Coords,
}

/// A link's round trip, µs, give or take `err_us` (none: unknown).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Latency {
    pub us: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub err_us: Option<u64>,
    pub source: Source,
}

/// A measured round trip and its spread, ns, and how old it is, ms.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Leg {
    pub rtt_ns: u64,
    pub spread_ns: u64,
    #[serde(default)]
    pub age_ms: u64,
}

/// What a node knows of one end of a link.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct End {
    /// Its coordinate as it announced it, and how old that is, ms.
    pub coord: Option<(Announce, u64)>,
    /// Its link to the relay, as it (or the relay) measured it.
    pub leg: Option<Leg>,
    /// Its link's outages.
    pub path: Option<Path>,
    pub access: Access,
    pub site: Option<String>,
}

fn us(ns: u64) -> u64 {
    ns.div_ceil(1000)
}

/// The round trip between `a` and `b`: `direct`, the pair's own measurement if any; else through a
/// relay, the legs and the relay's own time (`relay`: its filtered value and spread, ns); else
/// their coordinates.
pub fn latency(a: &End, b: &End, direct: Option<Leg>, relay: Option<Leg>) -> Option<Latency> {
    if let Some(d) = direct {
        return Some(Latency { us: us(d.rtt_ns), err_us: Some(us(d.spread_ns / 2)), source: Source::Measured });
    }
    #[cfg(test)]
    let relayed = if super::mutant() == Some(super::Mutant::LegsIgnored) { None } else { a.leg.zip(b.leg) };
    #[cfg(not(test))]
    let relayed = a.leg.zip(b.leg);
    if let Some((la, lb)) = relayed {
        let r = relay.unwrap_or_default();
        let ns = la.rtt_ns.saturating_add(lb.rtt_ns).saturating_add(r.rtt_ns);
        let spread = (la.spread_ns.saturating_add(lb.spread_ns).saturating_add(r.spread_ns)) / 2;
        // A leg measured a while ago has changed since as its end's links drift.
        let drift = |e: &End, l: &Leg| e.coord.map_or(0, |(c, _)| fixed::mul(l.rtt_ns, super::coord::grown(c.d, l.age_ms)));
        let err = spread.saturating_add(drift(a, &la)).saturating_add(drift(b, &lb));
        return Some(Latency { us: us(ns), err_us: Some(us(err)), source: Source::Legs });
    }
    let ((ca, age_a), (cb, age_b)) = (a.coord?, b.coord?);
    let ns = ca.c.rtt(&cb.c);
    let err = ca.v.zip(cb.v).map(|(va, vb)| {
        // The out-of-sample error each vouches for, grown by its age as its links drift.
        let ea = Coord { e: va, ..ca.c }.aged(ca.d, age_a);
        let eb = Coord { e: vb, ..cb.c }.aged(cb.d, age_b);
        us(fixed::mul(ns, ea.max(eb)))
    });
    Some(Latency { us: us(ns), err_us: err, source: Source::Coords })
}

/// Everything the owner is given for one runner it might pick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency: Option<Latency>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate: Option<Rate>,
    /// Drops an hour, Q32, and the mean outage, ms.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outages: Option<Path>,
}

impl Link {
    /// What a run of `span_ms` here is expected to lose to drops, ms: the stability term.
    pub fn stall_ms(&self, span_ms: u64) -> u64 {
        self.outages.map_or(0, |p| p.stall_ms(span_ms))
    }

    /// How long `bytes` take to reach it, µs (none: no rate known).
    pub fn transfer_us(&self, bytes: u64) -> Option<u64> {
        if bytes == 0 {
            return Some(0);
        }
        self.rate.map(|r| r.time_us(bytes))
    }
}

/// The owner's view of the link from `me` to `them`: `own` its fit of its own transfers to them,
/// `site` its fit of transfers to another runner of their site, `direct` its own measurement of
/// a direct link to them, `relay` the relayed link's own time, `hops` the relays' access rates.
pub fn link(me: &End, them: &End, own: Option<&Fit>, site: Option<&Fit>, direct: Option<Leg>, relay: Option<Leg>, hops: &[Access]) -> Link {
    let latency = latency(me, them, direct, relay);
    let rate = rate::estimate(own, site, &me.access, &them.access, hops);
    let outages = match (me.path, them.path) {
        (Some(a), Some(b)) if direct.is_none() => Some(Path::of(&[a, b])),
        (a, b) => b.or(a),
    };
    Link { latency, rate, outages }
}

/// Where a run's inputs are: on which node, and how many bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Input {
    pub on: String,
    pub bytes: u64,
}

/// How long a run's inputs take to reach a candidate, µs: each input over the link from where it
/// is, one after another (none on the candidate itself costs nothing); none if a link's rate is
/// unknown. `link_from(holder)` gives the link from that holder to the candidate.
pub fn inputs_us(inputs: &[Input], candidate: &str, link_from: impl Fn(&str) -> Option<Link>) -> Option<u64> {
    let mut total = 0u64;
    for i in inputs {
        if i.on == candidate || i.bytes == 0 {
            continue;
        }
        total = total.saturating_add(link_from(&i.on)?.transfer_us(i.bytes)?);
    }
    Some(total)
}
