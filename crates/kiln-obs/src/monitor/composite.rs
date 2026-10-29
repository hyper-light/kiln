//! Composite monitors (`docs/observe.md` §9): Datadog's, a boolean expression over other monitors'
//! states, group by group (docs.datadoghq.com, "Composite Monitor").
//!
//! ```text
//! expr := and ("||" and)*      and := not ("&&" not)*      not := "!" not | atom
//! atom := id | "(" expr ")"    id  := a monitor's id, [A-Za-z0-9_-]+
//! ```
//!
//! Ten monitors at the most, none of them a composite. A state is weighed by its severity, ALERT
//! over WARN over NO DATA over OK, the first three alert-worthy: `a && b` is the less severe of the
//! two (OK unless both are alert-worthy), `a || b` the more severe, `!a` OK where `a` is
//! alert-worthy, ALERT where it is OK, NO DATA where it is NO DATA (Datadog's rules, and its table
//! of two monitors, row for row: the tests). Datadog states no precedence: `!` binds tightest, then
//! `&&`, then `||`, as in C and the languages after it.
//!
//! Groups: a composite looks only at what its monitors have in common. A group is known by its
//! tags' values, not their keys (`host:web04` is `service:web04`'s). The composite's groups are those
//! of its most finely grouped monitor for which each other monitor grouped has a group whose values
//! it holds: `host` on web01–05 and `host` on web04–09 share web04 and web05; `device,host` holds
//! `host`; `host,url` and `device,host` share nothing. A monitor not grouped stands for every group.

use super::{MonitorError, State};

/// Monitors a composite reads at the most (Datadog's: "up to 10").
pub const MAX_MONITORS: usize = 10;

/// A step of the expression, in postfix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// A monitor's state, by its place in [`Composite::ids`].
    Id(u8),
    Not,
    And,
    Or,
}

/// A composite's expression, parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Composite {
    /// The monitors it reads, as they first appear.
    pub ids: Vec<String>,
    code: Vec<Step>,
}

fn syntax(why: impl Into<String>) -> MonitorError {
    MonitorError::Syntax(why.into())
}

/// A state's severity: ALERT 4, WARN 3, NO DATA 2, OK 1.
pub fn severity(s: State) -> u8 {
    match s {
        State::Alert => 4,
        State::Warn => 3,
        State::NoData => 2,
        State::Ok => 1,
    }
}

fn alert_worthy(s: State) -> bool {
    s != State::Ok
}

/// Parses an expression (`m1 && (m2 || !m3)`): shunting-yard into postfix, no recursion.
pub fn parse(q: &str) -> Result<Composite, MonitorError> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Pending {
        Open,
        Not,
        And,
        Or,
    }
    let prec = |p: Pending| match p {
        Pending::Not => 3,
        Pending::And => 2,
        Pending::Or => 1,
        Pending::Open => 0,
    };
    let emit = |p: Pending| match p {
        Pending::Not => Some(Step::Not),
        Pending::And => Some(Step::And),
        Pending::Or => Some(Step::Or),
        Pending::Open => None,
    };
    let mut ids: Vec<String> = Vec::new();
    let mut code: Vec<Step> = Vec::new();
    let mut ops: Vec<Pending> = Vec::new();
    // Whether an operand is wanted next (at the start, after an operator or an opening parenthesis).
    let mut operand = true;
    let b = q.as_bytes();
    let mut i = 0;
    while let Some(&c) = b.get(i) {
        match c {
            b' ' | b'\t' | b'\n' | b'\r' => i += 1,
            b'(' if operand => {
                ops.push(Pending::Open);
                i += 1;
            }
            b')' if !operand => {
                loop {
                    match ops.pop() {
                        Some(Pending::Open) => break,
                        Some(p) => code.extend(emit(p)),
                        None => return Err(syntax(format!("a ')' at {i} without its '('"))),
                    }
                }
                i += 1;
            }
            b'!' if operand => {
                ops.push(Pending::Not);
                i += 1;
            }
            b'&' | b'|' if !operand => {
                if b.get(i + 1) != Some(&c) {
                    return Err(syntax(format!("at {i}: `{}` (`&&` and `||`)", char::from(c))));
                }
                let op = if c == b'&' { Pending::And } else { Pending::Or };
                // Left-associative: what binds as tightly or tighter goes first.
                while let Some(&top) = ops.last() {
                    if top == Pending::Open || prec(top) < prec(op) {
                        break;
                    }
                    ops.pop();
                    code.extend(emit(top));
                }
                ops.push(op);
                operand = true;
                i += 2;
            }
            c if operand && (c.is_ascii_alphanumeric() || c == b'_' || c == b'-') => {
                let start = i;
                while b.get(i).is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'-') {
                    i += 1;
                }
                let id = q.get(start..i).unwrap_or("");
                let at = match ids.iter().position(|x| x == id) {
                    Some(at) => at,
                    None => {
                        if ids.len() == MAX_MONITORS {
                            return Err(syntax(format!("more than {MAX_MONITORS} monitors")));
                        }
                        ids.push(id.to_owned());
                        ids.len() - 1
                    }
                };
                code.push(Step::Id(u8::try_from(at).map_err(|_| syntax("too many monitors"))?));
                // A `!` waiting binds to this operand.
                while ops.last() == Some(&Pending::Not) {
                    ops.pop();
                    code.push(Step::Not);
                }
                operand = false;
            }
            _ if operand => return Err(syntax(format!("at {i}: a monitor's id, `!` or `(`"))),
            _ => return Err(syntax(format!("at {i}: `&&`, `||` or `)`"))),
        }
        // A `)` closes an operand: a `!` waiting before its `(` applies to it.
        if c == b')' {
            while ops.last() == Some(&Pending::Not) {
                ops.pop();
                code.push(Step::Not);
            }
        }
    }
    if operand {
        return Err(syntax(if code.is_empty() { "an expression of monitors".to_owned() } else { "a monitor after the last operator".to_owned() }));
    }
    while let Some(p) = ops.pop() {
        match p {
            Pending::Open => return Err(syntax("a '(' without its ')'")),
            p => code.extend(emit(p)),
        }
    }
    Ok(Composite { ids, code })
}

impl Composite {
    /// The composite's state, its monitors' states in [`Composite::ids`]' order; `stack` a buffer
    /// kept across calls.
    pub fn eval(&self, states: &[State], stack: &mut Vec<State>) -> State {
        stack.clear();
        for step in &self.code {
            let s = match step {
                Step::Id(i) => states.get(usize::from(*i)).copied().unwrap_or(State::NoData),
                Step::Not => match stack.pop().unwrap_or(State::NoData) {
                    State::NoData => State::NoData,
                    s if alert_worthy(s) => State::Ok,
                    _ => State::Alert,
                },
                Step::And | Step::Or => {
                    let b = stack.pop().unwrap_or(State::NoData);
                    let a = stack.pop().unwrap_or(State::NoData);
                    let (less, more) = if severity(a) <= severity(b) { (a, b) } else { (b, a) };
                    if *step == Step::And { less } else { more }
                }
            };
            stack.push(s);
        }
        stack.pop().unwrap_or(State::NoData)
    }

    /// The expression with each monitor named by `name` (its id where `name` says nothing).
    pub fn show(&self, name: impl Fn(&str) -> Option<String>) -> String {
        let mut parts: Vec<(String, u8)> = Vec::new();
        for step in &self.code {
            match step {
                Step::Id(i) => {
                    let id = self.ids.get(usize::from(*i)).map(String::as_str).unwrap_or("?");
                    parts.push((name(id).unwrap_or_else(|| id.to_owned()), 4));
                }
                Step::Not => {
                    let (s, p) = parts.pop().unwrap_or_default();
                    parts.push((if p < 3 { format!("!({s})") } else { format!("!{s}") }, 3));
                }
                Step::And | Step::Or => {
                    let (b, pb) = parts.pop().unwrap_or_default();
                    let (a, pa) = parts.pop().unwrap_or_default();
                    let (op, p) = if *step == Step::And { ("&&", 2) } else { ("||", 1) };
                    let wrap = |s: String, ps: u8| if ps < p { format!("({s})") } else { s };
                    parts.push((format!("{} {op} {}", wrap(a, pa), wrap(b, pb)), p));
                }
            }
        }
        parts.pop().map(|(s, _)| s).unwrap_or_default()
    }
}

/// A group: its tags (sorted, as a monitor's groups are).
pub type Group = Vec<(String, String)>;

/// The composite's groups and each one's state, from its monitors' groups and states
/// (`monitors[i]` [`Composite::ids`]`[i]`'s; a monitor not grouped has one group, of no tags).
/// None while a monitor has no state at all.
pub fn evaluate(c: &Composite, monitors: &[Vec<(Group, State)>]) -> Option<Vec<(Group, State)>> {
    if monitors.len() != c.ids.len() || monitors.iter().any(Vec::is_empty) {
        return None;
    }
    let grouped = |m: &Vec<(Group, State)>| m.iter().any(|(g, _)| !g.is_empty());
    let mut stack = Vec::new();
    let mut states: Vec<State> = monitors.iter().map(|m| m.first().map_or(State::NoData, |(_, s)| *s)).collect();
    // The most finely grouped monitor's groups are the composite's candidates.
    let finest = monitors.iter().enumerate().filter(|(_, m)| grouped(m)).max_by(|(i, a), (j, b)| {
        let tags = |m: &Vec<(Group, State)>| m.iter().map(|(g, _)| g.len()).max().unwrap_or(0);
        // The first of the finest, as written.
        tags(a).cmp(&tags(b)).then(j.cmp(i))
    });
    let Some((fi, fm)) = finest else {
        return Some(vec![(Vec::new(), c.eval(&states, &mut stack))]);
    };
    // Each other grouped monitor's groups by their values, sorted.
    let index: Vec<Option<std::collections::HashMap<Vec<&str>, State>>> = monitors
        .iter()
        .enumerate()
        .map(|(i, m)| {
            (i != fi && grouped(m)).then(|| {
                m.iter()
                    .map(|(g, s)| {
                        let mut v: Vec<&str> = g.iter().map(|(_, v)| v.as_str()).collect();
                        v.sort_unstable();
                        (v, *s)
                    })
                    .collect()
            })
        })
        .collect();
    let mut out = Vec::new();
    'groups: for (g, s) in fm {
        let mut values: Vec<&str> = g.iter().map(|(_, v)| v.as_str()).collect();
        values.sort_unstable();
        values.dedup();
        if let Some(slot) = states.get_mut(fi) {
            *slot = *s;
        }
        for (i, idx) in index.iter().enumerate() {
            let Some(idx) = idx else { continue };
            // A group of this monitor whose values the candidate holds: one of the candidate's
            // values' subsets of that monitor's size (a few: groups have a few tags).
            let k = monitors.get(i).and_then(|m| m.first()).map_or(0, |(g, _)| g.len());
            let Some(found) = subset_state(&values, k, idx) else { continue 'groups };
            if let Some(slot) = states.get_mut(i) {
                *slot = found;
            }
        }
        out.push((g.clone(), c.eval(&states, &mut stack)));
    }
    Some(out)
}

/// The state of the group in `idx` whose (sorted) values are a `k`-subset of `values`, if any.
fn subset_state(values: &[&str], k: usize, idx: &std::collections::HashMap<Vec<&str>, State>) -> Option<State> {
    if k == 0 || k > values.len() {
        return None;
    }
    // Each k-subset, in order: `pick` the places chosen.
    let n = values.len();
    let mut pick: Vec<usize> = (0..k).collect();
    let mut key: Vec<&str> = Vec::with_capacity(k);
    loop {
        key.clear();
        key.extend(pick.iter().filter_map(|&p| values.get(p).copied()));
        if let Some(s) = idx.get(&key) {
            return Some(*s);
        }
        // The next subset: the last place that can move, moved, those after it just after.
        let mut j = k;
        loop {
            if j == 0 {
                return None;
            }
            j -= 1;
            if pick.get(j).is_some_and(|&p| p < n - k + j) {
                break;
            }
        }
        let base = pick.get(j).copied().unwrap_or(0) + 1;
        for (o, p) in pick.iter_mut().skip(j).enumerate() {
            *p = base + o;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(s: &str) -> State {
        match s {
            "Alert" => State::Alert,
            "Warn" => State::Warn,
            "No Data" => State::NoData,
            "OK" => State::Ok,
            _ => panic!("{s}"),
        }
    }

    fn eval(q: &str, states: &[State]) -> State {
        parse(q).unwrap().eval(states, &mut Vec::new())
    }

    /// Datadog's table of two monitors, row by row (the rows with notify-no-data off and a NO DATA
    /// answer, "last known", are the monitor's to keep: here the answer is NO DATA).
    #[test]
    fn two_monitors_combine_as_datadog_s_table_says() {
        let rows = [
            ("Alert", "Warn", "&&", "Warn"),
            ("Alert", "Warn", "||", "Alert"),
            ("Alert", "OK", "&&", "OK"),
            ("Alert", "OK", "||", "Alert"),
            ("Warn", "OK", "&&", "OK"),
            ("Warn", "OK", "||", "Warn"),
            ("No Data", "Warn", "&&", "No Data"),
            ("No Data", "Warn", "||", "Warn"),
            ("No Data", "OK", "&&", "OK"),
            ("No Data", "OK", "||", "No Data"),
            ("No Data", "No Data", "&&", "No Data"),
            ("No Data", "No Data", "||", "No Data"),
        ];
        for (a, b, op, want) in rows {
            assert_eq!(eval(&format!("ma {op} mb"), &[st(a), st(b)]), st(want), "{a} {op} {b}");
            // Either way round.
            assert_eq!(eval(&format!("mb {op} ma"), &[st(b), st(a)]), st(want), "{b} {op} {a}");
        }
    }

    #[test]
    fn not_is_ok_for_trouble_alert_for_ok_and_no_data_for_no_data() {
        for (a, want) in [("Alert", "OK"), ("Warn", "OK"), ("OK", "Alert"), ("No Data", "No Data")] {
            assert_eq!(eval("!m1", &[st(a)]), st(want), "!{a}");
        }
        assert_eq!(eval("!!m1", &[State::Warn]), State::Alert);
    }

    #[test]
    fn not_binds_tightest_then_and_then_or() {
        // a || b && !c: a || (b && (!c)).
        let (a, b, c) = (State::Ok, State::Alert, State::Ok);
        assert_eq!(eval("a || b && !c", &[a, b, c]), State::Alert);
        assert_eq!(eval("(a || b) && !c", &[a, b, c]), State::Alert);
        assert_eq!(eval("a || b && !c", &[State::Ok, State::Alert, State::Alert]), State::Ok);
        // !(a && b) against !a && b.
        assert_eq!(eval("!(a && b)", &[State::Alert, State::Ok]), State::Alert);
        assert_eq!(eval("!a && b", &[State::Alert, State::Ok]), State::Ok);
        let q = parse("(a || b) && (c || d)").unwrap();
        assert_eq!(q.ids, ["a", "b", "c", "d"]);
        assert_eq!(q.show(|id| Some(id.to_uppercase())), "(A || B) && (C || D)");
        assert_eq!(parse("a || b && !c").unwrap().show(|_| None), "a || b && !c");
        assert_eq!(parse("!(a && b)").unwrap().show(|_| None), "!(a && b)");
    }

    #[test]
    fn mistakes_are_errors_that_say_where() {
        for q in ["", "a &&", "&& a", "a & b", "a | b", "(a || b", "a || b)", "a b", "!", "a && ()", "a ^ b"] {
            assert!(parse(q).is_err(), "{q:?} parsed");
        }
        let eleven = (0..11).map(|i| format!("m{i}")).collect::<Vec<_>>().join(" && ");
        assert!(parse(&eleven).is_err());
        let ten = (0..10).map(|i| format!("m{i}")).collect::<Vec<_>>().join(" || ");
        assert_eq!(parse(&ten).unwrap().ids.len(), 10);
        // A monitor named twice is one monitor.
        assert_eq!(parse("m1 && !m1").unwrap().ids, ["m1"]);
    }

    fn groups(tag: &str, values: &[&str], s: State) -> Vec<(Group, State)> {
        values.iter().map(|v| (vec![(tag.to_owned(), (*v).to_owned())], s)).collect()
    }

    fn names(out: &[(Group, State)]) -> Vec<String> {
        out.iter().map(|(g, s)| format!("{}={}", g.iter().map(|(k, v)| format!("{k}:{v}")).collect::<Vec<_>>().join(","), s.as_str())).collect()
    }

    /// Datadog's examples: only the groups every monitor has count (A on web01–05, B on web04–09:
    /// web04 and web05); a group is its values (`service:web04` is `host:web04`'s); a finer
    /// grouping holds a coarser one; `host,url` and `device,host` share nothing.
    #[test]
    fn groups_are_those_the_monitors_share_by_their_tags_values() {
        let c = parse("a && b").unwrap();
        let a = groups("host", &["web01", "web02", "web03", "web04", "web05"], State::Alert);
        let b = groups("host", &["web04", "web05", "web06", "web07", "web08", "web09"], State::Alert);
        assert_eq!(names(&evaluate(&c, &[a.clone(), b]).unwrap()), ["host:web04=ALERT", "host:web05=ALERT"]);
        let svc = groups("service", &["web04"], State::Warn);
        let three = parse("a && b && c").unwrap();
        let b = groups("host", &["web04", "web05"], State::Alert);
        assert_eq!(names(&evaluate(&three, &[a.clone(), b, svc]).unwrap()), ["host:web04=WARN"]);
        // device,host holds host.
        let dh: Vec<(Group, State)> = [("sda", "web01", State::Alert), ("sdb", "web01", State::Ok), ("sda", "web02", State::Alert)]
            .iter()
            .map(|(d, h, s)| (vec![("device".to_owned(), (*d).to_owned()), ("host".to_owned(), (*h).to_owned())], *s))
            .collect();
        let h = groups("host", &["web01", "web02"], State::Alert);
        assert_eq!(names(&evaluate(&c, &[h, dh.clone()]).unwrap()), ["device:sda,host:web01=ALERT", "device:sdb,host:web01=OK", "device:sda,host:web02=ALERT"]);
        // host,url and device,host: nothing in common.
        let hu: Vec<(Group, State)> = vec![(vec![("host".to_owned(), "web01".to_owned()), ("url".to_owned(), "/a".to_owned())], State::Alert)];
        assert!(evaluate(&c, &[hu, dh]).unwrap().is_empty());
        // A monitor not grouped stands for every group.
        let simple = vec![(Vec::new(), State::Warn)];
        assert_eq!(names(&evaluate(&c, &[simple.clone(), a]).unwrap()).len(), 5);
        assert_eq!(names(&evaluate(&c, &[simple.clone(), simple.clone()]).unwrap()), ["=WARN"]);
        // A monitor with no state yet: no answer.
        assert_eq!(evaluate(&c, &[simple, Vec::new()]), None);
    }

    #[test]
    fn every_k_subset_is_tried() {
        let idx: std::collections::HashMap<Vec<&str>, State> = [(vec!["b", "d"], State::Warn)].into_iter().collect();
        assert_eq!(subset_state(&["a", "b", "c", "d"], 2, &idx), Some(State::Warn));
        assert_eq!(subset_state(&["a", "b", "c"], 2, &idx), None);
        assert_eq!(subset_state(&["b"], 2, &idx), None);
    }
}
