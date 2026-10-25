//! A goal stack as a graph (docs/goals.md, "Side by side"; docs/agentic.md G1 and G2): what each
//! goal waits for, which goals are ready, and who takes each one that's ready. It's pure: the driver
//! hands it the stack as it stands and the room the agent has for subagents, then carries out what
//! comes back.
//!
//! - **What a goal waits for.** First, what it says it needs (`needs`): given when it was added,
//!   taken from a plan's graph for that plan's steps, or set by the agent when it ordered its
//!   stack. Second, what it is: a part waits for what its goal waits for, and a goal waits for its
//!   parts. A goal that says nothing is *unordered*. While subagents can take goals, the agent says
//!   what each one needs before it goes anywhere ([`Out::order`]): order is inferred first, then
//!   goals fan out. When subagents can't take goals, an unordered goal goes *in turn*: it waits for
//!   every goal still to go before it, as every goal did before goals ran side by side. So does a
//!   goal kiln couldn't get ordered ([`Needs::InTurn`]). A part that says what it waits for has no
//!   place in the stack's order of its own: it takes its goal's.
//! - **A plan run's steps** laid on the stack as goals of their own ([`Node::run`]) are ordered by
//!   their plan: they're neither ordered against each other nor wait in turn for each other.
//! - **Ready.** A goal is ready when it's still to go, isn't under way anywhere, is ordered, and
//!   nothing it waits for is still to go. Each goes as soon as it's ready (greedily), the ready ones
//!   by round over the whole stack (breadth first), then in the stack's order. A goal met once its
//!   parts are (one with a plan) is never ready itself.
//! - **Who takes it** ([`assign`]). While subagents are in play (two goals or more at work, ready
//!   or waiting to be ordered at once), each ready goal goes to a subagent of its own while there's
//!   room, the rest wait for room, and the agent coordinates: it takes only what's its own
//!   ([`Takes::Agent`]: a goal back from its subagent unmet, planning a goal, starting a plan's run).
//!   With nothing to fan out, the agent takes the first ready goal itself, as it always has. While
//!   goals wait for it to order them, it takes nothing new, so it's free to answer.
//!
//! Its work is bounded by the stack (500 goals at the most) and how far parts nest, with no
//! recursion.

use std::collections::{HashMap, HashSet, VecDeque};

/// How far up a part's goals go (a part of a part …), at the most.
const DEPTH: usize = 16;

/// What a goal says it waits for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) enum Needs {
    /// These goals of its stack, by id (none: nothing).
    Given(Vec<String>),
    /// It says nothing yet. While subagents can take goals the agent orders it; otherwise it goes
    /// in turn.
    #[default]
    Unknown,
    /// In turn, whatever subagents can do (after every goal still to go before it): kiln asked for
    /// its order and didn't get it in time.
    InTurn,
}

/// Who may take a goal up once it's ready.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum Takes {
    /// The agent, or a subagent of its own.
    #[default]
    Anyone,
    /// The agent itself: a goal back from its subagent unmet, or the agent's own work for a goal
    /// (planning it, starting the plan run it waits on).
    Agent,
    /// No one: a goal met once its parts are (one with a plan, its steps its parts).
    Nobody,
}

/// Where a goal is under way, when it isn't for the agent to take.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum Away {
    #[default]
    No,
    /// On subagents of the agent's: one started for it, or the agents of its plan's run.
    Sub,
    /// On another agent: a program's track worker (docs/programs.md).
    Other,
}

/// A goal of the stack as the graph sees it. A stack's goals come in its order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Node {
    pub id: String,
    /// Still to go: pending, or under way.
    pub open: bool,
    /// Under way on the agent itself.
    pub here: bool,
    pub away: Away,
    /// The goal it's a part of.
    pub part_of: Option<String>,
    pub needs: Needs,
    /// Who may take it up once it's ready.
    pub takes: Takes,
    /// The plan run it's a step of, when the run's steps are goals of the stack of their own (not a
    /// goal's parts): its plan orders them.
    pub run: Option<String>,
}

/// A stack as a graph ([`build`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Graph {
    /// For each goal still to go, by id, the goals still to go that it waits for.
    pub waits: HashMap<String, Vec<String>>,
    /// Each goal's round over the whole stack: 0 when it waits for nothing, otherwise one more
    /// than the latest round among the goals it waits for.
    pub round: HashMap<String, usize>,
    /// Goals still to go that wait for the agent to say what they need, in the stack's order.
    pub unordered: Vec<String>,
    /// Goals still to go that are held by a loop of waiting: in one, or waiting for one. A loop is
    /// refused when it would be made ([`loops`]).
    pub looped: Vec<String>,
}

/// The goals `n` is a part of, nearest first.
fn ancestors<'a>(n: &'a Node, by_id: &HashMap<&str, &'a Node>) -> Vec<&'a Node> {
    let mut out: Vec<&Node> = Vec::new();
    let mut at = n;
    for _ in 0..DEPTH {
        let Some(p) = at.part_of.as_deref().and_then(|p| by_id.get(p).copied()) else { break };
        if p.id == n.id || out.iter().any(|o| o.id == p.id) {
            break;
        }
        out.push(p);
        at = p;
    }
    out
}

/// What each goal waits for, in the stack's order: (the goal, soft). A soft wait is only "the goal
/// before it", and it gives way to a loop.
type Edges<'a> = Vec<(&'a str, Vec<(&'a str, bool)>)>;

/// Each goal's round, and the goals no round reaches (in a loop of waiting, or waiting for one).
fn rounds<'a>(edges: &Edges<'a>) -> (HashMap<&'a str, usize>, Vec<&'a str>) {
    let mut left: HashMap<&str, usize> = HashMap::new();
    let mut after: HashMap<&str, Vec<&str>> = HashMap::new();
    for (id, e) in edges {
        left.insert(*id, e.len());
        for (d, _) in e {
            after.entry(*d).or_default().push(*id);
        }
    }
    let mut best: HashMap<&str, usize> = HashMap::new();
    let mut done: HashMap<&str, usize> = HashMap::new();
    let mut queue: VecDeque<&str> = edges.iter().filter(|(_, e)| e.is_empty()).map(|(id, _)| *id).collect();
    while let Some(id) = queue.pop_front() {
        let r = best.get(id).copied().unwrap_or(0);
        done.insert(id, r);
        for &j in after.get(id).map_or(&[][..], Vec::as_slice) {
            let b = best.entry(j).or_insert(0);
            *b = (*b).max(r.saturating_add(1));
            if let Some(k) = left.get_mut(j) {
                *k = k.saturating_sub(1);
                if *k == 0 {
                    queue.push_back(j);
                }
            }
        }
    }
    let stuck = edges.iter().map(|(id, _)| *id).filter(|id| !done.contains_key(id)).collect();
    (done, stuck)
}

/// Whether `from` waits for `to`, directly or through others, in `of` (each goal's waits).
fn reaches(of: &HashMap<&str, Vec<&str>>, from: &str, to: &str) -> bool {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut todo: Vec<&str> = vec![from];
    while let Some(at) = todo.pop() {
        if !seen.insert(at) {
            continue;
        }
        for d in of.get(at).map_or(&[][..], Vec::as_slice) {
            if *d == to {
                return true;
            }
            todo.push(d);
        }
    }
    false
}

/// The graph of `nodes`, the stack's goals in its order. `infer`: subagents can take its goals, so
/// a goal still to go that says nothing waits for the agent to order it, unless nothing else is
/// still to go to order it against (but its own parts, the goals it's a part of, or its plan run's
/// other steps). Without `infer`, such a goal goes in turn: it waits for every goal still to go
/// before it in the stack's order (one handed to another agent passed over, and a step its plan run's
/// other steps), so the stack runs in its order, as it always has.
pub(super) fn build(nodes: &[Node], infer: bool) -> Graph {
    let by_id: HashMap<&str, &Node> = nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    let ups: HashMap<&str, Vec<&Node>> = nodes.iter().map(|n| (n.id.as_str(), ancestors(n, &by_id))).collect();
    let above = |n: &str, of: &str| ups.get(of).is_some_and(|u| u.iter().any(|a| a.id == n));
    // Goals one can't be ordered against the other: one a part of the other, or steps of one run.
    let kin = |a: &Node, b: &Node| above(&a.id, &b.id) || above(&b.id, &a.id) || (a.run.is_some() && a.run == b.run);
    let in_turn = |n: &Node| n.needs == Needs::InTurn || (n.needs == Needs::Unknown && !infer);
    let mut unordered: Vec<String> = Vec::new();
    let mut own: HashMap<&str, Vec<(&str, bool)>> = HashMap::new();
    for (k, n) in nodes.iter().enumerate() {
        let w: Vec<(&str, bool)> = match &n.needs {
            Needs::Given(v) => v.iter().map(String::as_str).filter(|d| *d != n.id && by_id.contains_key(d)).map(|d| (d, false)).collect(),
            _ if in_turn(n) => {
                // Every goal still to go before it. The nearest that goes in turn itself (and is no
                // run's step) waits for those before it already, so it stands for them: each goal's
                // waits stay few however long the stack.
                let mut w: Vec<(&str, bool)> = Vec::new();
                for m in nodes.iter().take(k).rev().filter(|m| m.open && m.away != Away::Other && !kin(n, m)) {
                    w.push((m.id.as_str(), true));
                    if in_turn(m) && m.run.is_none() {
                        break;
                    }
                }
                w
            }
            _ => {
                let waiting = n.open && !n.here && n.away == Away::No;
                if waiting && nodes.iter().any(|m| m.open && m.id != n.id && !kin(n, m)) {
                    unordered.push(n.id.clone());
                }
                Vec::new()
            }
        };
        own.insert(n.id.as_str(), w);
    }
    let mut edges: Edges<'_> = Vec::with_capacity(nodes.len());
    for n in nodes {
        let up = ups.get(n.id.as_str()).map_or(&[][..], Vec::as_slice);
        let mut e: Vec<(&str, bool)> = own.get(n.id.as_str()).cloned().unwrap_or_default();
        // What the goals it's a part of say they need; and where they stand in the stack's order,
        // for a part that says what it waits for (it has no place in that order of its own).
        let placed = !matches!(n.needs, Needs::Given(_));
        for a in up {
            e.extend(own.get(a.id.as_str()).into_iter().flatten().filter(|(_, soft)| !soft || !placed).copied());
        }
        e.extend(nodes.iter().filter(|p| p.part_of.as_deref() == Some(n.id.as_str())).map(|p| (p.id.as_str(), false)));
        // Never itself, nor a goal it's a part of (that one waits for it).
        e.retain(|(d, _)| *d != n.id && !up.iter().any(|a| a.id == *d));
        let mut kept: Vec<(&str, bool)> = Vec::new();
        for (d, soft) in e {
            match kept.iter_mut().find(|(x, _)| *x == d) {
                Some(k) => k.1 = k.1 && soft,
                None => kept.push((d, soft)),
            }
        }
        edges.push((n.id.as_str(), kept));
    }
    let open: HashSet<&str> = nodes.iter().filter(|n| n.open).map(|n| n.id.as_str()).collect();
    let (mut round, mut stuck) = rounds(&edges);
    // A loop through "the goal before it" isn't one: those waits give way, a pass at a time, until
    // only loops of what goals say remain (refused as they're made, held should one get here). A
    // goal that has ended waits for nothing any more.
    for _ in 0..DEPTH.saturating_add(2) {
        if stuck.is_empty() {
            break;
        }
        let of: HashMap<&str, Vec<&str>> = edges.iter().map(|(id, e)| (*id, e.iter().map(|(d, _)| *d).collect())).collect();
        let mut gave = false;
        for (id, e) in &mut edges {
            if !stuck.contains(id) {
                continue;
            }
            let had = e.len();
            if open.contains(id) {
                e.retain(|(d, soft)| !(*soft && stuck.contains(d) && reaches(&of, d, id)));
            } else {
                e.clear();
            }
            gave |= e.len() < had;
        }
        if !gave {
            break;
        }
        (round, stuck) = rounds(&edges);
    }
    let waits = edges.iter().filter(|(id, _)| open.contains(id)).map(|(id, e)| ((*id).to_owned(), e.iter().filter(|(d, _)| open.contains(d)).map(|(d, _)| (*d).to_owned()).collect())).collect();
    Graph {
        waits,
        round: round.into_iter().map(|(k, v)| (k.to_owned(), v)).collect(),
        unordered,
        looped: nodes.iter().filter(|n| n.open && stuck.contains(&n.id.as_str())).map(|n| n.id.clone()).collect(),
    }
}

/// The goals that a loop of waiting would hold if the stack's goals were as `nodes` say (none when
/// there's no loop): what goals say counts, and "the goal before it" gives way.
pub(super) fn loops(nodes: &[Node]) -> Vec<String> {
    build(nodes, true).looped
}

/// The goals ready to go out now, in the order they go: by round, then in the stack's order. A goal
/// met once its parts are is never one of them.
pub(super) fn ready<'a>(g: &Graph, nodes: &'a [Node]) -> Vec<&'a Node> {
    let by_id: HashMap<&str, &Node> = nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    let unordered = |n: &Node| g.unordered.contains(&n.id);
    let mut out: Vec<(usize, usize, &Node)> = nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| {
            n.open
                && n.takes != Takes::Nobody
                && !n.here
                && n.away == Away::No
                && !unordered(n)
                && !g.looped.contains(&n.id)
                && g.waits.get(n.id.as_str()).is_none_or(Vec::is_empty)
                && ancestors(n, &by_id).iter().all(|a| a.away == Away::No && !a.here && !unordered(a))
        })
        .map(|(k, n)| (g.round.get(n.id.as_str()).copied().unwrap_or(usize::MAX), k, n))
        .collect();
    out.sort_by_key(|(r, k, _)| (*r, *k));
    out.into_iter().map(|(_, _, n)| n).collect()
}

/// The room the agent has for subagents now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(test), expect(dead_code, reason = "G2 hands ready goals to subagents; until it does, the driver has no room (`Never`)"))]
pub(super) enum Room {
    /// No maximum: its Subagents setting says none.
    Any,
    /// This many more may be at work at once.
    Left(usize),
    /// None at all: the agent may start no agents (it's as deep as agents may go), or its stack
    /// runs goals one at a time (`side_by_side` off). The agent does every goal itself.
    Never,
}

/// What goes where now ([`assign`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Out {
    /// The goal the agent itself takes up next.
    pub agent: Option<String>,
    /// The goals that go to subagents of their own now, in the order they go.
    pub subagents: Vec<String>,
    /// Goals the agent is to order (say what each needs) before they go anywhere.
    pub order: Vec<String>,
}

/// Who takes `g`'s ready goals, with `room` for subagents. While subagents are in play (the agent's
/// own goal under way, the goals out on subagents, those ready and those waiting to be ordered come
/// to two or more), each ready goal goes to a subagent while there's room, the rest wait for one to
/// free up, and the agent coordinates, taking only what's its own ([`Takes::Agent`]). With nothing
/// to fan out, or no room ever, the agent takes the first ready goal itself. While goals wait for it
/// to order them, it takes nothing new.
pub(super) fn assign(g: &Graph, nodes: &[Node], room: Room) -> Out {
    let by_id: HashMap<&str, &Node> = nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    let here = nodes.iter().any(|n| n.open && n.here);
    let subs_out = nodes.iter().filter(|n| n.open && n.away == Away::Sub && !ancestors(n, &by_id).iter().any(|a| a.away == Away::Sub)).count();
    let ready = ready(g, nodes);
    let mut out = Out { order: g.unordered.clone(), ..Out::default() };
    // Free for a goal of its own: none under way, and none waiting for it to say what goals need.
    let free = !here && out.order.is_empty();
    let first = |its: fn(&Node) -> bool| ready.iter().find(|n| its(n)).filter(|_| free).map(|n| n.id.clone());
    let fan = match room {
        Room::Never => None,
        Room::Any => Some(usize::MAX),
        Room::Left(k) => Some(k),
    };
    match fan {
        Some(k) if usize::from(here) + subs_out + ready.len() + out.order.len() >= 2 => {
            out.agent = first(|n| n.takes == Takes::Agent);
            out.subagents = ready.iter().filter(|n| n.takes == Takes::Anyone).take(k).map(|n| n.id.clone()).collect();
        }
        _ => out.agent = first(|_| true),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str) -> Node {
        Node { id: id.into(), open: true, ..Node::default() }
    }

    fn given(id: &str, needs: &[&str]) -> Node {
        Node { needs: Needs::Given(needs.iter().map(|s| (*s).to_owned()).collect()), ..node(id) }
    }

    fn part(mut n: Node, of: &str) -> Node {
        n.part_of = Some(of.into());
        n
    }

    fn met(mut n: Node) -> Node {
        n.open = false;
        n
    }

    fn ids<'a>(v: &[&'a Node]) -> Vec<&'a str> {
        v.iter().map(|n| n.id.as_str()).collect()
    }

    fn waits<'a>(g: &'a Graph, id: &str) -> Vec<&'a str> {
        g.waits.get(id).map(|v| v.iter().map(String::as_str).collect()).unwrap_or_default()
    }

    fn subs(v: &[&str]) -> Out {
        Out { subagents: v.iter().map(|s| (*s).to_owned()).collect(), ..Out::default() }
    }

    /// With no room for subagents ever, goals that say nothing wait for the one before them: the
    /// stack runs one goal at a time, in its order, as it always has.
    #[test]
    fn g1_goals_that_say_nothing_go_in_turn_with_no_room_ever() {
        let nodes = vec![node("a"), node("b"), node("c")];
        let g = build(&nodes, false);
        assert_eq!(waits(&g, "b"), ["a"]);
        assert_eq!(waits(&g, "c"), ["b"]);
        assert_eq!(ids(&ready(&g, &nodes)), ["a"]);
        assert_eq!(assign(&g, &nodes, Room::Never), Out { agent: Some("a".into()), ..Out::default() });
        // A goal ended out of turn: the next waits for the goal still to go before it.
        let nodes = vec![node("a"), met(node("b")), node("c")];
        assert_eq!(waits(&build(&nodes, false), "c"), ["a"]);
    }

    /// In turn, the stack's order is what it was: a goal handed to another agent is passed over, a
    /// goal's parts come before it, and a goal put before a goal with parts comes after those parts.
    #[test]
    fn g1_in_turn_the_stack_goes_in_its_order_as_before() {
        let nodes = vec![Node { away: Away::Other, ..node("x") }, part(node("p1"), "g"), part(node("p2"), "g"), node("a"), node("g"), node("h")];
        let g = build(&nodes, false);
        assert_eq!(ids(&ready(&g, &nodes)), ["p1"]);
        assert_eq!(waits(&g, "p1"), Vec::<&str>::new());
        assert_eq!(waits(&g, "a"), ["p2"]);
        assert_eq!(waits(&g, "g"), ["a", "p1", "p2"]);
        assert_eq!(waits(&g, "h"), ["g"]);
        assert!(g.looped.is_empty(), "{:?}", g.looped);
    }

    /// A goal and its own parts aren't ordered against each other: a goal alone with its parts
    /// needs no ordering, but two parts of it do.
    #[test]
    fn g1_a_goal_alone_with_its_parts_needs_no_ordering() {
        let nodes = vec![part(node("p1"), "g"), node("g")];
        let g = build(&nodes, true);
        assert!(g.unordered.is_empty(), "{:?}", g.unordered);
        assert_eq!(ids(&ready(&g, &nodes)), ["p1"]);
        let nodes = vec![part(node("p1"), "g"), part(node("p2"), "g"), node("g")];
        assert_eq!(build(&nodes, true).unordered, ["p1", "p2"]);
    }

    /// While subagents can take goals, goals that say nothing wait for the agent to order them, and
    /// none goes out until it has; one under way already isn't asked about.
    #[test]
    fn g1_unordered_goals_wait_for_the_agent_to_order_them() {
        let nodes = vec![Node { here: true, ..node("a") }, node("b"), node("c")];
        let g = build(&nodes, true);
        assert_eq!(g.unordered, ["b", "c"]);
        assert!(ready(&g, &nodes).is_empty());
        assert_eq!(assign(&g, &nodes, Room::Any), Out { order: vec!["b".into(), "c".into()], ..Out::default() });
    }

    /// A goal that's the only one still to go has nothing to be ordered against: it's ready, and
    /// with nothing to fan out it's the agent's own.
    #[test]
    fn g1_a_lone_goal_needs_no_ordering() {
        let nodes = vec![met(node("a")), node("b")];
        let g = build(&nodes, true);
        assert!(g.unordered.is_empty());
        assert_eq!(assign(&g, &nodes, Room::Any), Out { agent: Some("b".into()), ..Out::default() });
    }

    /// A goal waits for its parts, and a part for what its goal waits for: a planned goal's steps
    /// start once what the goal needs has ended, and the goal ends after them.
    #[test]
    fn g1_parts_wait_for_what_their_goal_waits_for_and_the_goal_for_its_parts() {
        let stack = |a: Node| vec![a, part(given("s1", &[]), "p"), part(given("s2", &["s1"]), "p"), given("p", &["a"]), given("d", &["p"])];
        let nodes = stack(given("a", &[]));
        let g = build(&nodes, true);
        assert_eq!(waits(&g, "s1"), ["a"]);
        assert_eq!(waits(&g, "p"), ["a", "s1", "s2"]);
        assert_eq!(ids(&ready(&g, &nodes)), ["a"]);
        let nodes = stack(met(given("a", &[])));
        assert_eq!(ids(&ready(&build(&nodes, true), &nodes)), ["s1"]);
    }

    /// A loop of what goals say is found, and its goals are held while the rest go on. A loop that
    /// runs through "the goal before it" isn't one: that wait gives way.
    #[test]
    fn g1_a_loop_is_found_and_one_through_the_goal_before_gives_way() {
        let nodes = vec![given("a", &["b"]), given("b", &["a"]), given("c", &[])];
        assert_eq!(loops(&nodes), ["a", "b"]);
        assert_eq!(ids(&ready(&build(&nodes, true), &nodes)), ["c"]);
        let nodes = vec![given("a", &["b"]), node("b"), node("c")];
        let g = build(&nodes, false);
        assert!(g.looped.is_empty(), "{:?}", g.looped);
        assert_eq!(waits(&g, "c"), ["b"]);
        assert_eq!(ids(&ready(&g, &nodes)), ["b"]);
        assert!(loops(&nodes).is_empty());
    }

    /// Goals kiln couldn't get ordered go in turn, whatever subagents can do.
    #[test]
    fn g1_goals_kiln_couldnt_get_ordered_go_in_turn() {
        let turn = |id: &str| Node { needs: Needs::InTurn, ..node(id) };
        let nodes = vec![turn("a"), turn("b"), turn("c")];
        let g = build(&nodes, true);
        assert!(g.unordered.is_empty());
        assert_eq!(ids(&ready(&g, &nodes)), ["a"]);
        assert_eq!(waits(&g, "c"), ["b"]);
    }

    /// Ready goals that don't wait for each other fan out to subagents, and the agent coordinates.
    #[test]
    fn g2_ready_goals_fan_out_and_the_agent_coordinates() {
        let nodes = vec![given("a", &[]), given("b", &[]), given("c", &["a"])];
        assert_eq!(assign(&build(&nodes, true), &nodes, Room::Any), subs(&["a", "b"]));
    }

    /// One ready goal with nothing else at work is the agent's own; beside a goal under way on the
    /// agent, or one out on a subagent, it goes to a subagent.
    #[test]
    fn g2_a_lone_ready_goal_is_the_agents_and_one_beside_others_a_subagents() {
        let nodes = vec![given("a", &[]), given("b", &["a"])];
        assert_eq!(assign(&build(&nodes, true), &nodes, Room::Any).agent.as_deref(), Some("a"));
        let busy = vec![Node { here: true, ..given("a", &[]) }, given("b", &[])];
        assert_eq!(assign(&build(&busy, true), &busy, Room::Any), subs(&["b"]));
        let out = vec![Node { away: Away::Sub, ..given("a", &[]) }, given("b", &[])];
        assert_eq!(assign(&build(&out, true), &out, Room::Any), subs(&["b"]));
    }

    /// The room bounds how many go out, and the rest wait for room while the agent coordinates.
    /// With no room ever, the agent takes them one at a time.
    #[test]
    fn g2_room_bounds_the_subagents_and_the_rest_wait() {
        let nodes = vec![given("a", &[]), given("b", &[]), given("c", &[])];
        let g = build(&nodes, true);
        assert_eq!(assign(&g, &nodes, Room::Left(2)), subs(&["a", "b"]));
        assert_eq!(assign(&g, &nodes, Room::Left(0)), Out::default());
        assert_eq!(assign(&g, &nodes, Room::Never), Out { agent: Some("a".into()), ..Out::default() });
    }

    /// Greedy over the whole stack: a plan's steps and other goals go out together, from the one
    /// pool, each as soon as what it waits for has ended.
    #[test]
    fn g2_a_plans_steps_and_other_goals_go_out_from_one_pool() {
        let stack = |s1: Node, s2: Node, x: Node| vec![part(s1, "p"), part(s2, "p"), part(given("s3", &["s1", "s2"]), "p"), given("p", &[]), x, given("y", &["p", "x"])];
        let nodes = stack(given("s1", &[]), given("s2", &[]), given("x", &[]));
        assert_eq!(assign(&build(&nodes, true), &nodes, Room::Any), subs(&["s1", "s2", "x"]));
        let later = stack(met(given("s1", &[])), Node { away: Away::Sub, ..given("s2", &[]) }, met(given("x", &[])));
        assert_eq!(assign(&build(&later, true), &later, Room::Any), Out::default());
    }

    /// Breadth first: of the goals ready at once, those of an earlier round over the whole stack go
    /// first when there's room for only some.
    #[test]
    fn g2_an_earlier_round_goes_first_when_room_is_short() {
        let nodes = vec![met(given("a", &[])), given("b", &["a"]), given("c", &[])];
        let g = build(&nodes, true);
        assert_eq!(ids(&ready(&g, &nodes)), ["c", "b"]);
        assert_eq!(assign(&g, &nodes, Room::Left(1)).subagents, ["c"]);
    }

    /// A goal back from its subagent unmet is the agent's, while the rest fan out beside it.
    #[test]
    fn g2_a_goal_back_from_its_subagent_is_the_agents() {
        let nodes = vec![Node { takes: Takes::Agent, ..given("a", &[]) }, given("b", &[])];
        assert_eq!(assign(&build(&nodes, true), &nodes, Room::Any), Out { agent: Some("a".into()), subagents: vec!["b".into()], ..Out::default() });
    }

    /// In turn is after every goal still to go before it, not only the nearest: goals that say what
    /// they wait for (a plan's steps) can end out of the stack's order, and a goal in turn after
    /// them waits for each. The nearest goal in turn stands for those before it.
    #[test]
    fn g1_in_turn_a_goal_waits_for_every_goal_still_to_go_before_it() {
        let nodes = vec![node("a"), given("s1", &[]), given("s2", &["s1"]), given("s3", &[]), node("h"), node("k")];
        let g = build(&nodes, false);
        assert_eq!(waits(&g, "h"), ["s3", "s2", "s1", "a"]);
        assert_eq!(waits(&g, "k"), ["h"]);
        // s3 ended first: h still waits for s1 and s2.
        let nodes = vec![node("a"), given("s1", &[]), given("s2", &["s1"]), met(given("s3", &[])), node("h")];
        assert_eq!(waits(&build(&nodes, false), "h"), ["s2", "s1", "a"]);
    }

    /// A plan run's steps laid on the stack as goals of their own are ordered by their plan. In turn,
    /// a step that waits for no other waits for the goals before the run, not for the run's other
    /// steps, and a goal in turn after the run waits for every one of them. While subagents can take
    /// goals, those steps are ordered against the goals around the run, not each other, and a run
    /// alone on the stack has nothing to be ordered against.
    #[test]
    fn g1_a_plan_runs_steps_are_ordered_by_their_plan() {
        let step = |n: Node| Node { run: Some("r".into()), ..n };
        let nodes = vec![node("a"), step(node("s1")), step(given("s2", &["s1"])), step(node("s3")), node("h")];
        let g = build(&nodes, false);
        assert_eq!((waits(&g, "s1"), waits(&g, "s2"), waits(&g, "s3")), (vec!["a"], vec!["s1"], vec!["a"]));
        assert_eq!(waits(&g, "h"), ["s3", "s2", "s1", "a"]);
        let after_a = vec![met(node("a")), step(node("s1")), step(given("s2", &["s1"])), step(node("s3")), node("h")];
        assert_eq!(ids(&ready(&build(&after_a, false), &after_a)), ["s1", "s3"]);
        assert_eq!(build(&nodes, true).unordered, ["a", "s1", "s3", "h"]);
        let alone = vec![step(node("s1")), step(given("s2", &["s1"])), step(node("s3"))];
        let g = build(&alone, true);
        assert!(g.unordered.is_empty(), "{:?}", g.unordered);
        assert_eq!(assign(&g, &alone, Room::Any), subs(&["s1", "s3"]));
    }

    /// A goal's plan steps (its parts, saying what they wait for) take the goal's place in the
    /// stack's order: they start once what comes before the goal has ended. The goal, met once they
    /// are, is never taken up itself.
    #[test]
    fn g1_a_goals_plan_steps_take_its_place_in_the_stacks_order() {
        let with_plan = |n: Node| Node { takes: Takes::Nobody, ..n };
        let nodes = vec![node("a"), part(given("s1", &[]), "p"), part(given("s2", &["s1"]), "p"), with_plan(node("p")), node("h")];
        let g = build(&nodes, false);
        assert_eq!(waits(&g, "s1"), ["a"]);
        assert_eq!(waits(&g, "s2"), ["s1", "a"]);
        assert_eq!(waits(&g, "p"), ["a", "s1", "s2"]);
        assert_eq!(waits(&g, "h"), ["p"]);
        let done = vec![met(node("a")), part(met(given("s1", &[])), "p"), part(met(given("s2", &["s1"])), "p"), with_plan(node("p")), node("h")];
        let g = build(&done, false);
        assert!(ready(&g, &done).is_empty(), "a goal with a plan is met by its parts, not taken up");
        assert_eq!(assign(&g, &done, Room::Any), Out::default());
    }

    /// While goals wait for the agent to say what they need, it takes nothing new, so it's free to
    /// answer. Those waiting are in play: an ordered goal ready beside them goes to a subagent.
    #[test]
    fn g2_while_goals_wait_to_be_ordered_the_agent_takes_nothing_new() {
        let nodes = vec![given("x", &[]), node("y")];
        let g = build(&nodes, true);
        assert_eq!(g.unordered, ["y"]);
        assert_eq!(assign(&g, &nodes, Room::Any), Out { subagents: vec!["x".into()], order: vec!["y".into()], ..Out::default() });
        let nodes = vec![given("x", &["y"]), node("y")];
        assert_eq!(assign(&build(&nodes, true), &nodes, Room::Any), Out { order: vec!["y".into()], ..Out::default() });
    }

    /// While subagents are in play, the agent takes only its own work (planning a goal, a goal back
    /// from its subagent). With no room ever, it takes the first ready goal, whatever it is.
    #[test]
    fn g2_the_agent_keeps_only_its_own_work_while_subagents_are_in_play() {
        let own = |n: Node| Node { takes: Takes::Agent, ..n };
        let nodes = vec![given("x", &[]), own(given("plan it", &[])), given("y", &[])];
        let g = build(&nodes, true);
        assert_eq!(assign(&g, &nodes, Room::Any), Out { agent: Some("plan it".into()), subagents: vec!["x".into(), "y".into()], ..Out::default() });
        assert_eq!(assign(&g, &nodes, Room::Left(1)), Out { agent: Some("plan it".into()), subagents: vec!["x".into()], ..Out::default() });
        assert_eq!(assign(&g, &nodes, Room::Never).agent.as_deref(), Some("x"));
    }

    /// A goal out on another agent (a program's worker) isn't a subagent of this one's, and the
    /// parts of a goal out on a subagent go with it: neither is ready here.
    #[test]
    fn g2_goals_away_are_not_ready_here() {
        let nodes = vec![Node { away: Away::Other, ..given("a", &[]) }, given("b", &[])];
        assert_eq!(assign(&build(&nodes, true), &nodes, Room::Any).agent.as_deref(), Some("b"));
        let nodes = vec![part(given("p1", &[]), "g"), Node { away: Away::Sub, ..given("g", &[]) }, given("h", &["g"])];
        assert!(ready(&build(&nodes, true), &nodes).is_empty());
    }
}
