//! A structural merge: the conflict ladder's first rung (docs/ci.md W26-R17). Where git's line merge
//! left a hunk conflicted, this merges it on syntax trees, with the tree-sitter grammars kiln
//! vendors through vorpal, and keeps the markers wherever it isn't sure. Mergiraf's way in spirit
//! (Mergiraf itself is GPL-3.0-only; kiln is MIT), and more cautious: pure, text in and text out.
//!
//! - One hunk at a time ([`resolve_hunk`]): the file's other hunks take main's side in all three
//!   versions, so the three differ only at this hunk; base, main and this are parsed, and merged.
//! - Two nodes merge when one side left the node as the base had it (the other's is taken), both
//!   made it the same, or they're the same kind with children: their children merge as a three-way
//!   merge of sibling sequences. Siblings are matched across the three by kind and identity: a
//!   definition's name (its `name` or `key` field), anything else by its text. A stretch of
//!   siblings only one side changed takes that side's; one both changed alike takes it once; one
//!   both changed otherwise merges sibling by sibling where each side has the same kinds there,
//!   and is otherwise kept ([`Kept::SamePlace`], [`Kept::SameNode`]).
//! - Kept besides: a file without a grammar, or a version that doesn't parse; a name defined twice
//!   among siblings in any version (which one is which is ambiguous) or in the merge (Mergiraf's
//!   duplicate-signature check); a merge that doesn't parse; and one that changed the file outside
//!   the hunk.

use std::collections::BTreeMap;

use vorpal_core::Node;
use vorpal_core::tree_sitter::StrDoc;
use vorpal_language::{Language as _, LanguageExt as _, SupportLang};

type N<'r> = Node<'r, StrDoc<SupportLang>>;

/// The most siblings' pairs matched at one level (base's by a side's), and the deepest nesting
/// followed: past them it's kept.
const MAX_PAIRS: usize = 1_000_000;
const MAX_DEPTH: usize = 256;

/// Why a structural merge kept a hunk's markers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kept {
    /// No grammar kiln takes reads this file.
    NoGrammar,
    /// A version of the file doesn't parse cleanly.
    DoesNotParse,
    /// Both sides changed the same node, otherwise.
    SameNode,
    /// Both sides put something different in the same place.
    SamePlace,
    /// A name is defined twice among the same siblings, so which is which is unclear.
    Ambiguous,
    /// The merge would define the same name twice among the same siblings.
    DuplicateSignature,
    /// The merge doesn't parse cleanly.
    MergeDoesNotParse,
    /// The merge changed the file outside the hunk.
    OutsideHunk,
    /// Too large or too deep to merge here.
    TooLarge,
}

impl Kept {
    pub fn as_str(self) -> &'static str {
        match self {
            Kept::NoGrammar => "no_grammar",
            Kept::DoesNotParse => "does_not_parse",
            Kept::SameNode => "same_node",
            Kept::SamePlace => "same_place",
            Kept::Ambiguous => "ambiguous",
            Kept::DuplicateSignature => "duplicate_signature",
            Kept::MergeDoesNotParse => "merge_does_not_parse",
            Kept::OutsideHunk => "outside_hunk",
            Kept::TooLarge => "too_large",
        }
    }

    /// In words, for the change page.
    pub fn why(self) -> &'static str {
        match self {
            Kept::NoGrammar => "no grammar kiln has reads this file",
            Kept::DoesNotParse => "a side of it doesn't parse",
            Kept::SameNode => "both sides changed the same code",
            Kept::SamePlace => "both sides added different code in the same place",
            Kept::Ambiguous => "a name is defined twice there, so which is which is unclear",
            Kept::DuplicateSignature => "merging would define the same name twice",
            Kept::MergeDoesNotParse => "the merge wouldn't parse",
            Kept::OutsideHunk => "the merge would change the file outside the conflict",
            Kept::TooLarge => "it's too large to merge structurally",
        }
    }
}

/// What a structural merge made of one hunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The hunk's merged text, in place of its markers.
    Resolved(String),
    Kept(Kept),
}

/// One conflicted hunk of the file at `path`, merged: `prefix` and `suffix` are the file around it
/// (the same in each version), `base`, `main` and `this` the hunk's three sides (each its whole
/// lines, with their line endings).
pub fn resolve_hunk(path: &str, prefix: &str, base: &str, main: &str, this: &str, suffix: &str) -> Outcome {
    let Some(lang) = SupportLang::from_path(path) else { return Outcome::Kept(Kept::NoGrammar) };
    let whole = |side: &str| format!("{prefix}{side}{suffix}");
    match merge(lang, &whole(base), &whole(main), &whole(this)) {
        Ok(text) => {
            let inner = text.strip_prefix(prefix).and_then(|t| t.strip_suffix(suffix));
            match inner {
                Some(hunk) => Outcome::Resolved(hunk.to_owned()),
                None => Outcome::Kept(Kept::OutsideHunk),
            }
        }
        Err(k) => Outcome::Kept(k),
    }
}

/// Three whole versions of a file merged structurally.
pub fn merge_files(path: &str, base: &str, main: &str, this: &str) -> Outcome {
    let Some(lang) = SupportLang::from_path(path) else { return Outcome::Kept(Kept::NoGrammar) };
    match merge(lang, base, main, this) {
        Ok(text) => Outcome::Resolved(text),
        Err(k) => Outcome::Kept(k),
    }
}

fn merge(lang: SupportLang, base: &str, main: &str, this: &str) -> Result<String, Kept> {
    if main == this {
        return Ok(main.to_owned());
    }
    if base == main {
        return Ok(this.to_owned());
    }
    if base == this {
        return Ok(main.to_owned());
    }
    // tree-sitter is C, and the grammars are generated: whatever goes wrong in them is a kept hunk.
    std::panic::catch_unwind(|| {
        let (gb, gm, gt) = (lang.grep(base), lang.grep(main), lang.grep(this));
        let (rb, rm, rt) = (gb.root(), gm.root(), gt.root());
        if rb.has_error() || rm.has_error() || rt.has_error() {
            return Err(Kept::DoesNotParse);
        }
        finish(lang, Merger.node(&rb, &rm, &rt, 0)?)
    })
    .unwrap_or(Err(Kept::DoesNotParse))
}

/// A sibling of the merge: its text, the space before it in its own version (none for a first
/// sibling), and its name where it defines one.
struct Piece {
    text: String,
    gap: Option<String>,
    name: Option<String>,
}

struct Kids<'r> {
    src: &'r str,
    nodes: Vec<N<'r>>,
    keys: Vec<String>,
    names: Vec<Option<String>>,
}

impl<'r> Kids<'r> {
    fn of(n: &N<'r>) -> Kids<'r> {
        let src: &'r str = n.get_doc().src.as_str();
        let nodes: Vec<N<'r>> = n.children().collect();
        let names: Vec<Option<String>> = nodes.iter().map(name_of).collect();
        let keys = nodes.iter().zip(&names).map(|(c, name)| match name {
            Some(name) => format!("{}#{name}", c.kind()),
            None => format!("{}={}", c.kind(), c.text()),
        });
        Kids { src, keys: keys.collect(), nodes, names }
    }

    /// Whether a name is defined twice among them.
    fn ambiguous(&self) -> bool {
        let mut seen: BTreeMap<&str, ()> = BTreeMap::new();
        self.keys.iter().zip(&self.names).filter(|(_, n)| n.is_some()).any(|(k, _)| seen.insert(k.as_str(), ()).is_some())
    }

    /// The text between sibling `at` and the one before it.
    fn gap(&self, at: usize) -> Option<String> {
        let (prev, this) = (self.nodes.get(at.checked_sub(1)?)?, self.nodes.get(at)?);
        self.src.get(prev.range().end..this.range().start).map(str::to_owned)
    }

    /// Sibling `at` as a piece of the merge, its text `text`.
    fn piece(&self, at: usize, text: String) -> Piece {
        Piece { text, gap: self.gap(at), name: self.names.get(at).cloned().flatten() }
    }

    fn texts(&self, from: usize, to: usize) -> Vec<String> {
        self.nodes.get(from..to).unwrap_or_default().iter().map(|n| n.text().into_owned()).collect()
    }
}

/// The name a node defines: its `name` field's text, or its `key` field's (a JSON, YAML or TOML
/// pair's).
fn name_of(n: &N<'_>) -> Option<String> {
    n.field("name").or_else(|| n.field("key")).map(|f| f.text().into_owned())
}

/// A merge as it's given: only one that parses cleanly in its grammar.
fn finish(lang: SupportLang, merged: String) -> Result<String, Kept> {
    if lang.grep(&merged).root().has_error() {
        return Err(Kept::MergeDoesNotParse);
    }
    Ok(merged)
}

struct Merger;

impl Merger {
    fn node(&self, b: &N<'_>, m: &N<'_>, t: &N<'_>, depth: usize) -> Result<String, Kept> {
        let (tb, tm, tt) = (b.text(), m.text(), t.text());
        if tm == tt || tb == tt {
            return Ok(tm.into_owned());
        }
        if tb == tm {
            return Ok(tt.into_owned());
        }
        if depth > MAX_DEPTH {
            return Err(Kept::TooLarge);
        }
        if b.kind() != m.kind() || b.kind() != t.kind() {
            return Err(Kept::SameNode);
        }
        let (kb, km, kt) = (Kids::of(b), Kids::of(m), Kids::of(t));
        if kb.nodes.is_empty() || km.nodes.is_empty() || kt.nodes.is_empty() {
            return Err(Kept::SameNode);
        }
        if kb.ambiguous() || km.ambiguous() || kt.ambiguous() {
            return Err(Kept::Ambiguous);
        }
        let to_main = matched(&kb.keys, &km.keys)?;
        let to_this = matched(&kb.keys, &kt.keys)?;
        let mut pieces: Vec<Piece> = Vec::new();
        let (mut pb, mut pm, mut pt) = (0, 0, 0);
        for i in 0..=kb.nodes.len() {
            let anchor = if i == kb.nodes.len() {
                Some((i, km.nodes.len(), kt.nodes.len()))
            } else {
                match (to_main.get(i).copied().flatten(), to_this.get(i).copied().flatten()) {
                    (Some(x), Some(y)) if x >= pm && y >= pt => Some((i, x, y)),
                    _ => None,
                }
            };
            let Some((ai, am, at)) = anchor else { continue };
            self.stretch(&kb, &km, &kt, (pb, ai), (pm, am), (pt, at), depth, &mut pieces)?;
            if let (Some(nb), Some(nm), Some(nt)) = (kb.nodes.get(ai), km.nodes.get(am), kt.nodes.get(at)) {
                let text = self.node(nb, nm, nt, depth + 1)?;
                pieces.push(km.piece(am, text));
            }
            (pb, pm, pt) = (ai + 1, am + 1, at + 1);
        }
        // Mergiraf's duplicate-signature check: no name may now be defined twice here.
        let mut seen: BTreeMap<String, ()> = BTreeMap::new();
        for p in &pieces {
            if let Some(name) = &p.name
                && seen.insert(name.clone(), ()).is_some()
            {
                return Err(Kept::DuplicateSignature);
            }
        }
        assemble(m, &km, &pieces)
    }

    /// The siblings between two anchors: base's `b`, main's `m` and this's `t` (from, to).
    #[allow(clippy::too_many_arguments)]
    fn stretch(&self, kb: &Kids<'_>, km: &Kids<'_>, kt: &Kids<'_>, b: (usize, usize), m: (usize, usize), t: (usize, usize), depth: usize, out: &mut Vec<Piece>) -> Result<(), Kept> {
        let (sb, sm, st) = (kb.texts(b.0, b.1), km.texts(m.0, m.1), kt.texts(t.0, t.1));
        fn take(k: &Kids<'_>, (from, to): (usize, usize), out: &mut Vec<Piece>) {
            for at in from..to {
                if let Some(n) = k.nodes.get(at) {
                    out.push(k.piece(at, n.text().into_owned()));
                }
            }
        }
        if sm == st || sb == st {
            take(km, m, out);
            return Ok(());
        }
        if sb == sm {
            take(kt, t, out);
            return Ok(());
        }
        // Both changed this stretch: sibling by sibling where each has the same kinds there.
        let kinds = |k: &Kids<'_>, (from, to): (usize, usize)| k.nodes.get(from..to).unwrap_or_default().iter().map(|n| n.kind().into_owned()).collect::<Vec<_>>();
        let (kinds_b, kinds_m, kinds_t) = (kinds(kb, b), kinds(km, m), kinds(kt, t));
        if kinds_b.is_empty() || kinds_b != kinds_m || kinds_b != kinds_t {
            return Err(if kinds_b.is_empty() { Kept::SamePlace } else { Kept::SameNode });
        }
        for k in 0..kinds_b.len() {
            let (Some(nb), Some(nm), Some(nt)) = (kb.nodes.get(b.0 + k), km.nodes.get(m.0 + k), kt.nodes.get(t.0 + k)) else { return Err(Kept::SameNode) };
            let text = self.node(nb, nm, nt, depth + 1)?;
            out.push(km.piece(m.0 + k, text));
        }
        Ok(())
    }
}

/// The merged node's text: main's text before its first child and after its last, and before
/// each piece the space its own version had before it (main's between its first two, for a piece
/// that was first in its version).
fn assemble(m: &N<'_>, km: &Kids<'_>, pieces: &[Piece]) -> Result<String, Kept> {
    let src_m = km.src;
    let (first, last) = match (km.nodes.first(), km.nodes.last()) {
        (Some(f), Some(l)) => (f.range().start, l.range().end),
        _ => return Err(Kept::SameNode),
    };
    let fallback = km.gap(1).unwrap_or_default();
    let mut out = String::new();
    out.push_str(src_m.get(m.range().start..first).ok_or(Kept::SameNode)?);
    for (i, p) in pieces.iter().enumerate() {
        if i > 0 {
            out.push_str(p.gap.as_deref().unwrap_or(&fallback));
        }
        out.push_str(&p.text);
    }
    out.push_str(src_m.get(last..m.range().end).ok_or(Kept::SameNode)?);
    Ok(out)
}

/// Which of `b`'s keys each is matched to in `s`: a longest common subsequence of the two.
fn matched(b: &[String], s: &[String]) -> Result<Vec<Option<usize>>, Kept> {
    let (n, m) = (b.len(), s.len());
    if n.saturating_mul(m) > MAX_PAIRS {
        return Err(Kept::TooLarge);
    }
    let w = m + 1;
    // lcs[i * w + j]: the longest common subsequence of b[i..] and s[j..].
    let mut lcs = vec![0u32; (n + 1) * w];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            let v = if b.get(i) == s.get(j) { lcs.get((i + 1) * w + j + 1).copied().unwrap_or(0) + 1 } else { lcs.get((i + 1) * w + j).copied().unwrap_or(0).max(lcs.get(i * w + j + 1).copied().unwrap_or(0)) };
            if let Some(cell) = lcs.get_mut(i * w + j) {
                *cell = v;
            }
        }
    }
    let mut out = vec![None; n];
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if b.get(i) == s.get(j) {
            if let Some(o) = out.get_mut(i) {
                *o = Some(j);
            }
            i += 1;
            j += 1;
        } else if lcs.get((i + 1) * w + j).copied().unwrap_or(0) >= lcs.get(i * w + j + 1).copied().unwrap_or(0) {
            i += 1;
        } else {
            j += 1;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
