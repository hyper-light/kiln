//! Log patterns (`docs/observe.md` §6, §12.13): messages grouped by their template with Drain (He,
//! Zhu, Zheng and Lyu, "Drain: An Online Log Parsing Approach with Fixed Depth Tree", ICWS 2017),
//! ported from the reference (`logparser/Drain/Drain.py`) to the letter, so its benchmark's
//! accuracies come out here as published (`benches/loghub.rs`).
//!
//! A message's tokens (split at whitespace, its variables masked) go down a tree by their count and
//! their first tokens to a few candidate templates; the most similar (the share of the template's
//! tokens the message has at their places) takes it if at least `similarity` alike, its differing
//! tokens becoming `<*>`; else it starts a template of its own.
//!
//! Masked before the tree sees them, as Drain3 does without rules of a dataset's: a number (its
//! digits and the `.`, `:`, `,` and `-` between them: an address, a time, a date, a version), `0x`
//! hex, a run of 8 or more hex digits with a digit among them (an id, a hash, a UUID's parts) — each
//! where it stands between characters that are not letters or digits, its sign with it, so
//! `blk_-1608999687919862906` and `blk_42` are both `blk_<*>` (and `x86_64` `x86_<*>`, as Drain3 has
//! it) while `user123` stays itself.

use std::borrow::Cow;
use std::collections::HashMap;

/// A template's token for anything.
pub const WILDCARD: &str = "<*>";

/// Values at a place that make it a variable's, for [`Params::merge`].
pub const MERGE_VALUES: usize = 3;

/// Drain's parameters: the tree's depth (the reference's, counting its root and length layers), the
/// similarity a message needs to join a template, a node's children at the most; and kiln's two ways
/// with masked messages (the reference's own are both off):
/// - `masked_variable`: a token with a part masked goes the tree's wildcard way, as one with digits
///   does (masking `www.example.com:443` to `www.example.com:<*>` leaves no digit, and each host
///   would be a branch of its own);
/// - `masked_alike`: a masked variable of the message where the template has a wildcard is alike
///   (the reference counts no wildcard as alike, so a message with four variables of eleven tokens
///   can be at most 7/11 like any template, however it is built).
/// - `merge` (off: measured on Loghub, it costs more than it mends, §12.13): after the messages,
///   templates the tree kept apart are joined where a variable among the first tokens it branched
///   on sent them down branches of their own (`user ada signed in`, `user sam signed in`):
///   templates alike but at one of those places, where they hold at least [`MERGE_VALUES`] values
///   (IPLoM's reading of a position's values, Makanju et al., KDD 2009). Without it, as Drain has
///   it, a variable among a message's first two tokens makes a pattern of each of its values.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Params {
    pub depth: usize,
    pub similarity: f64,
    pub max_children: usize,
    pub masked_variable: bool,
    pub masked_alike: bool,
    pub merge: bool,
}

impl Params {
    /// The reference's: Drain as published, its benchmark's settings given.
    pub fn reference(depth: usize, similarity: f64) -> Self {
        Self { depth, similarity, max_children: 100, masked_variable: false, masked_alike: false, merge: false }
    }
}

impl Default for Params {
    /// What the Loghub study (§12.13) found best across its 16 systems, masking as above.
    fn default() -> Self {
        Self { depth: 5, similarity: 0.7, max_children: 100, masked_variable: true, masked_alike: true, merge: false }
    }
}

#[derive(Debug, Default)]
struct Node {
    children: HashMap<Box<str>, usize>,
    /// A leaf's templates.
    clusters: Vec<usize>,
}

/// A template and how many messages it took.
#[derive(Debug, Clone, PartialEq)]
pub struct Cluster {
    pub template: Vec<Box<str>>,
    pub size: u64,
}

impl Cluster {
    pub fn text(&self) -> String {
        self.template.join(" ")
    }
}

/// A Drain parser: its tree and its templates.
#[derive(Debug, Default)]
pub struct Drain {
    p: Params,
    nodes: Vec<Node>,
    /// The tree's first layer: a node a message length.
    by_length: HashMap<usize, usize>,
    clusters: Vec<Cluster>,
}

fn has_numbers(s: &str) -> bool {
    s.bytes().any(|b| b.is_ascii_digit())
}

impl Drain {
    pub fn new(p: Params) -> Self {
        Self { p, nodes: Vec::new(), by_length: HashMap::new(), clusters: Vec::new() }
    }

    pub fn clusters(&self) -> &[Cluster] {
        &self.clusters
    }

    /// The reference's `self.depth`: the layers of first tokens below the length layer.
    fn leaf(&self) -> usize {
        self.p.depth.saturating_sub(2)
    }

    fn node(&mut self) -> usize {
        self.nodes.push(Node::default());
        self.nodes.len() - 1
    }

    fn child(&self, node: usize, token: &str) -> Option<usize> {
        self.nodes.get(node).and_then(|n| n.children.get(token).copied())
    }

    /// Takes a message's tokens: the template it joined or started.
    pub fn add<S: AsRef<str>>(&mut self, tokens: &[S]) -> usize {
        match self.search(tokens) {
            Some(c) => {
                if let Some(cl) = self.clusters.get_mut(c) {
                    for (t, m) in cl.template.iter_mut().zip(tokens) {
                        if &**t != m.as_ref() && &**t != WILDCARD {
                            *t = WILDCARD.into();
                        }
                    }
                    cl.size += 1;
                }
                c
            }
            None => {
                let id = self.clusters.len();
                self.clusters.push(Cluster { template: tokens.iter().map(|t| t.as_ref().into()).collect(), size: 1 });
                self.insert(id);
                id
            }
        }
    }

    /// `treeSearch`: down by length and first tokens (a token's node, else the wildcard's), then the
    /// leaf's most similar template, if similar enough.
    fn search<S: AsRef<str>>(&self, seq: &[S]) -> Option<usize> {
        let mut node = *self.by_length.get(&seq.len())?;
        let mut reached = false;
        for (depth, token) in (1..).zip(seq) {
            if depth >= self.leaf() || depth > seq.len() {
                reached = true;
                break;
            }
            node = self.child(node, token.as_ref()).or_else(|| self.child(node, WILDCARD))?;
        }
        // A leaf is only where the walk stopped at its depth: a shorter message's walk ends inside
        // the tree, where no template is (the reference's, to the letter).
        let leaf = if reached { self.nodes.get(node)?.clusters.as_slice() } else { &[] };
        self.best(leaf, seq)
    }

    /// How alike a message (or a template) is to a template: the share of the template's tokens it
    /// has at their places (a wildcard of the template alike only to a masked variable, and only
    /// with `masked_alike`), and the template's wildcards.
    fn alike<S: AsRef<str>>(&self, template: &[Box<str>], seq: &[S]) -> (f64, usize) {
        let (mut alike, mut wild) = (0usize, 0usize);
        for (a, b) in template.iter().zip(seq) {
            if &**a == WILDCARD {
                wild += 1;
                if self.p.masked_alike && b.as_ref() == WILDCARD {
                    alike += 1;
                }
            } else if &**a == b.as_ref() {
                alike += 1;
            }
        }
        (if template.is_empty() { 0.0 } else { alike as f64 / template.len() as f64 }, wild)
    }

    /// The templates the tree kept apart, joined (`Params::merge`): of each length and each place the
    /// tree branched on, those alike everywhere else, when they hold at least [`MERGE_VALUES`]
    /// values there, into the largest (that place a wildcard). What each template became, by its
    /// place.
    pub fn merge(&mut self) -> Vec<usize> {
        let n = self.clusters.len();
        let mut into: Vec<usize> = (0..n).collect();
        let branched = self.leaf().saturating_sub(1);
        for place in 0..branched {
            // The templates still their own, by length and their tokens but at `place`.
            let mut sets: HashMap<(usize, Vec<Box<str>>), Vec<usize>> = HashMap::new();
            for (i, c) in self.clusters.iter().enumerate() {
                if into.get(i) != Some(&i) || c.template.len() <= place {
                    continue;
                }
                let mut key = c.template.clone();
                if let Some(t) = key.get_mut(place) {
                    *t = "".into();
                }
                sets.entry((c.template.len(), key)).or_default().push(i);
            }
            let mut sets: Vec<Vec<usize>> = sets.into_values().filter(|s| s.len() >= MERGE_VALUES).collect();
            sets.sort_unstable();
            for mut set in sets {
                set.sort_by_key(|&i| (std::cmp::Reverse(self.clusters.get(i).map_or(0, |c| c.size)), i));
                let Some((&keep, rest)) = set.split_first() else { continue };
                let size: u64 = rest.iter().filter_map(|&i| self.clusters.get(i)).map(|c| c.size).sum();
                if let Some(c) = self.clusters.get_mut(keep) {
                    if let Some(t) = c.template.get_mut(place) {
                        *t = WILDCARD.into();
                    }
                    c.size += size;
                }
                for &i in rest {
                    for x in into.iter_mut().filter(|x| **x == i) {
                        *x = keep;
                    }
                }
            }
        }
        into
    }

    /// `fastMatch`: the most similar template (more wildcards break a tie), if `similarity` alike.
    fn best<S: AsRef<str>>(&self, leaf: &[usize], seq: &[S]) -> Option<usize> {
        let mut best: Option<(usize, f64, usize)> = None;
        for &c in leaf {
            let Some(t) = self.clusters.get(c) else { continue };
            let (sim, wild) = self.alike(&t.template, seq);
            if best.is_none_or(|(_, s, w)| sim > s || (sim == s && wild > w)) {
                best = Some((c, sim, wild));
            }
        }
        best.filter(|(_, s, _)| *s >= self.p.similarity).map(|(c, ..)| c)
    }

    /// `addSeqToPrefixTree`: a new template's place, down by its first tokens; a token with digits,
    /// or one past a node's children, goes the wildcard's way.
    fn insert(&mut self, id: usize) {
        let Some(template) = self.clusters.get(id).map(|c| c.template.clone()) else { return };
        let len = template.len();
        let mut node = match self.by_length.get(&len) {
            Some(&n) => n,
            None => {
                let n = self.node();
                self.by_length.insert(len, n);
                n
            }
        };
        let (leaf, max) = (self.leaf(), self.p.max_children);
        for (depth, token) in (1..).zip(&template) {
            if depth >= leaf || depth > len {
                if let Some(n) = self.nodes.get_mut(node) {
                    n.clusters.push(id);
                }
                break;
            }
            node = match self.child(node, token) {
                Some(c) => c,
                None => {
                    let count = self.nodes.get(node).map_or(0, |n| n.children.len());
                    let wild = self.child(node, WILDCARD);
                    let variable = has_numbers(token) || (self.p.masked_variable && token.contains(WILDCARD));
                    let new_key: Option<&str> = if !variable {
                        match wild {
                            Some(_) if count < max => Some(token),
                            Some(_) => None,
                            None if count + 1 < max => Some(token),
                            None => Some(WILDCARD),
                        }
                    } else if wild.is_none() {
                        Some(WILDCARD)
                    } else {
                        None
                    };
                    match (new_key, wild) {
                        (Some(k), _) => {
                            let c = self.node();
                            if let Some(n) = self.nodes.get_mut(node) {
                                n.children.insert(k.into(), c);
                            }
                            c
                        }
                        (None, Some(w)) => w,
                        (None, None) => node,
                    }
                }
            };
        }
    }
}

/// A message's tokens, its variables masked (see the module's notes).
pub fn tokens(message: &str) -> Vec<Cow<'_, str>> {
    message.split_whitespace().map(mask).collect()
}

/// A token, its variables masked: numbers (digits with `.`, `:`, `,`, `-` between), `0x` hex, runs of
/// 8+ hex digits with a digit, each between non-alphanumerics.
pub fn mask(token: &str) -> Cow<'_, str> {
    let b = token.as_bytes();
    let n = b.len();
    let alnum = |i: usize| b.get(i).is_some_and(u8::is_ascii_alphanumeric);
    let mut out: Option<String> = None;
    let mut i = 0;
    let mut last = 0;
    while i < n {
        let start_ok = i == 0 || !alnum(i - 1);
        let mut end = None;
        if start_ok {
            // 0x hex.
            if b.get(i) == Some(&b'0') && matches!(b.get(i + 1), Some(b'x' | b'X')) && b.get(i + 2).is_some_and(u8::is_ascii_hexdigit) {
                let mut j = i + 2;
                while b.get(j).is_some_and(u8::is_ascii_hexdigit) {
                    j += 1;
                }
                end = Some(j);
            }
            // A run of hex digits (and `-` between): an id, a hash, a UUID.
            if end.is_none() && b.get(i).is_some_and(u8::is_ascii_hexdigit) {
                let mut j = i;
                let mut digits = 0;
                let mut hex = 0;
                while let Some(&c) = b.get(j) {
                    if c.is_ascii_hexdigit() {
                        hex += 1;
                        digits += usize::from(c.is_ascii_digit());
                    } else if !(c == b'-' && b.get(j + 1).is_some_and(u8::is_ascii_hexdigit)) {
                        break;
                    }
                    j += 1;
                }
                if hex >= 8 && digits > 0 && !alnum(j) {
                    end = Some(j);
                }
            }
            // A number, its sign with it (`blk_-123` is `blk_<*>`, as `blk_123` is): digits, and `.`,
            // `:`, `,`, `-` between digits.
            let signed = matches!(b.get(i), Some(b'-' | b'+')) && b.get(i + 1).is_some_and(u8::is_ascii_digit);
            if end.is_none() && (signed || b.get(i).is_some_and(u8::is_ascii_digit)) {
                let mut j = if signed { i + 1 } else { i };
                while let Some(&c) = b.get(j) {
                    if c.is_ascii_digit() || (matches!(c, b'.' | b':' | b',' | b'-') && b.get(j + 1).is_some_and(u8::is_ascii_digit)) {
                        j += 1;
                    } else {
                        break;
                    }
                }
                if !alnum(j) {
                    end = Some(j);
                }
            }
        }
        match end {
            Some(j) => {
                let o = out.get_or_insert_with(|| String::with_capacity(n));
                o.push_str(token.get(last..i).unwrap_or(""));
                o.push_str(WILDCARD);
                i = j;
                last = j;
            }
            None => i += 1,
        }
    }
    match out {
        Some(mut o) => {
            o.push_str(token.get(last..).unwrap_or(""));
            Cow::Owned(o)
        }
        None => Cow::Borrowed(token),
    }
}

/// Each line's group by its template's text (two templates alike are one group: the reference's
/// event ids are its templates' hashes), and the templates.
pub fn parse<S: AsRef<str>>(lines: impl IntoIterator<Item = Vec<S>>, p: Params) -> (Vec<usize>, Drain) {
    let mut d = Drain::new(p);
    let mut ids: Vec<usize> = lines.into_iter().map(|t| d.add(&t)).collect();
    if p.merge {
        let into = d.merge();
        for i in &mut ids {
            *i = into.get(*i).copied().unwrap_or(*i);
        }
    }
    // Numbered over the templates lines are in (those joined into others are none's).
    let mut by_text: HashMap<String, usize> = HashMap::new();
    let mut group: HashMap<usize, usize> = HashMap::new();
    for &i in &ids {
        if let (std::collections::hash_map::Entry::Vacant(e), Some(c)) = (group.entry(i), d.clusters.get(i)) {
            let n = by_text.len();
            e.insert(*by_text.entry(c.text()).or_insert(n));
        }
    }
    (ids.iter().map(|i| group.get(i).copied().unwrap_or(0)).collect(), d)
}

/// The share of lines grouped right (Zhu et al., "Tools and Benchmarks for Automated Log Parsing",
/// ICSE-SEIP 2019): a line is, when its group holds exactly the lines of its true group.
pub fn grouping_accuracy(truth: &[usize], parsed: &[usize]) -> f64 {
    let n = truth.len().min(parsed.len());
    if n == 0 {
        return 0.0;
    }
    let mut true_sizes: HashMap<usize, usize> = HashMap::new();
    for &t in truth.iter().take(n) {
        *true_sizes.entry(t).or_default() += 1;
    }
    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..n {
        if let Some(&p) = parsed.get(i) {
            groups.entry(p).or_default().push(i);
        }
    }
    let mut right = 0usize;
    for lines in groups.values() {
        let first = lines.first().and_then(|&i| truth.get(i)).copied();
        if let Some(t) = first
            && lines.iter().all(|&i| truth.get(i) == Some(&t))
            && true_sizes.get(&t).copied() == Some(lines.len())
        {
            right += lines.len();
        }
    }
    right as f64 / n as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variables_are_masked_where_they_stand_apart() {
        for (from, to) in [
            ("blk_-1608999687919862906", "blk_<*>"),
            ("blk_1608999687919862906", "blk_<*>"),
            ("-5", "<*>"),
            ("a-1", "a-<*>"),
            ("10.251.42.84:50010", "<*>"),
            ("size=1024", "size=<*>"),
            ("0x7f3a9c", "<*>"),
            ("x86_64", "x86_<*>"),
            ("user123", "user123"),
            ("2024-01-01", "<*>"),
            ("12:34:56,789", "<*>"),
            ("550e8400-e29b-41d4-a716-446655440000", "<*>"),
            ("deadbeefcafe", "deadbeefcafe"),
            ("(42)", "(<*>)"),
            ("v1.2.3", "v1.<*>"),
            ("plain", "plain"),
        ] {
            assert_eq!(mask(from), to, "{from}");
        }
    }

    /// A variable second token sends lines of one template down branches of their own: Drain keeps
    /// them apart (the default); merged, three values or more at that place are one pattern, two
    /// (a verb's, say) stay two.
    #[test]
    fn templates_the_tree_kept_apart_are_joined_only_when_asked_and_their_place_varies() {
        let lines = ["user ada signed in", "user sam signed in", "user lin signed in", "job 41 queued", "GET /api/runs 200 in 12 ms", "GET /login 200 in 7 ms"];
        let tok = |l: &&str| tokens(l).into_iter().map(|t| t.into_owned()).collect::<Vec<_>>();
        let (apart, _) = parse(lines.iter().map(tok), Params::default());
        assert_eq!(apart, vec![0, 1, 2, 3, 4, 5]);
        let (groups, d) = parse(lines.iter().map(tok), Params { merge: true, ..Params::default() });
        assert_eq!(groups, vec![0, 0, 0, 1, 2, 3], "{:?}", d.clusters().iter().map(Cluster::text).collect::<Vec<_>>());
    }

    /// Drain's own example: three messages of two templates; the third joins the first's, which
    /// takes a wildcard where they differ.
    #[test]
    fn messages_join_the_most_similar_template_and_differences_become_wildcards() {
        let lines = ["Receiving block blk_1 src: /10.0.0.1:50010", "Receiving block blk_2 src: /10.0.0.2:50010", "Deleting block blk_3 file /mnt/data/blk_3"];
        let (groups, d) = parse(lines.iter().map(|l| l.split_whitespace().collect::<Vec<_>>()), Params::reference(4, 0.5));
        assert_eq!(groups, vec![0, 0, 1]);
        assert_eq!(d.clusters().first().map(Cluster::text).as_deref(), Some("Receiving block <*> src: <*>"));
        assert_eq!(grouping_accuracy(&[7, 7, 9], &groups), 1.0);
        assert_eq!(grouping_accuracy(&[7, 8, 9], &groups), 1.0 / 3.0);
    }
}
