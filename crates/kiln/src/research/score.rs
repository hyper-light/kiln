//! How confident kiln is in a finding (docs/research.md, "Confidence"). The agent says what its
//! evidence is: each source's kind, the words it quotes and where they are, the counterclaims it
//! found. kiln weighs it, the same way for every finding, so a grade means the same thing whoever
//! researched it:
//!
//! - each source is taken at its word as far as its kind of thing goes ([`Type::reliability`]),
//!   less when the agent says it's out of date;
//! - each citation counts as far as it can be followed ([`citation`]): a quote kiln found where it
//!   says it is counts in full, a quote with where it is counts most of the way, a bare link half,
//!   nothing to follow little, and a quote kiln looked for and didn't find, or a dead link, hardly;
//! - sources from one origin (one site, one file, one record) count once, the strongest of them,
//!   and independent ones add up ([`confidence`]: one minus what each leaves unsupported);
//! - counterclaims weigh against it the same way, by their own sources (or, with none, by how
//!   strong the agent says they are);
//! - a claim the agent infers rather than reads in its sources counts for less.
//!
//! Its score is support × (1 − 0.8 × what weighs against it), out of 100, and its grade: A (85 and
//! up: high), B (70: good), C (50: moderate), D (30: low), F (very low).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{Check, CheckState, Cite, Counter, Finding, Source, Strength};

/// A source's kind of thing, which says how far it's taken at its word.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Type {
    // On the machines and systems the agent works in.
    /// A command it ran, and what it printed.
    Measurement,
    Code,
    /// Data: a dataset, a table, a spreadsheet.
    Data,
    /// Logs, traces, metrics.
    Log,
    /// A document kept there: a README, a design, notes.
    Doc,
    /// Pictures, video, sound.
    Media,
    /// kiln's own records: conversations, plans, runs, deployments.
    Record,
    // Elsewhere.
    /// A standard or a specification (an RFC, ISO, W3C).
    Standard,
    /// A peer-reviewed paper.
    Paper,
    /// A product's or a project's own documentation.
    Docs,
    Book,
    Preprint,
    News,
    Blog,
    Wiki,
    /// A forum, a Q&A site, an issue tracker, a mailing list.
    Forum,
    /// What a vendor says of its own product: marketing, sales.
    Vendor,
    Social,
    /// Any other web page.
    #[default]
    Web,
}

impl Type {
    /// Whether a source of this type is on the machines and systems the agent works in.
    pub fn internal(self) -> bool {
        matches!(self, Self::Measurement | Self::Code | Self::Data | Self::Log | Self::Doc | Self::Media | Self::Record)
    }

    /// How far a source of this type is taken at its word, from 0 to 1.
    pub fn reliability(self) -> f64 {
        match self {
            Self::Measurement => 0.95,
            Self::Code | Self::Standard => 0.9,
            Self::Paper | Self::Docs => 0.85,
            Self::Data | Self::Log => 0.8,
            Self::Book => 0.75,
            Self::Doc => 0.7,
            Self::Record => 0.65,
            Self::Preprint | Self::News | Self::Media => 0.6,
            Self::Blog | Self::Wiki => 0.5,
            Self::Forum => 0.4,
            Self::Vendor => 0.35,
            Self::Web => 0.3,
            Self::Social => 0.25,
        }
    }

    /// As words.
    pub fn words(self) -> &'static str {
        match self {
            Self::Measurement => "a measurement",
            Self::Code => "code",
            Self::Data => "data",
            Self::Log => "logs",
            Self::Doc => "a document",
            Self::Media => "media",
            Self::Record => "kiln's records",
            Self::Standard => "a standard",
            Self::Paper => "a peer-reviewed paper",
            Self::Docs => "official docs",
            Self::Book => "a book",
            Self::Preprint => "a preprint",
            Self::News => "news",
            Self::Blog => "a blog",
            Self::Wiki => "a wiki",
            Self::Forum => "a forum",
            Self::Vendor => "a vendor",
            Self::Social => "social media",
            Self::Web => "a web page",
        }
    }
}

/// A source said to be out of date counts this much of its kind.
const DATED: f64 = 0.75;
/// A claim the agent infers from its sources, rather than reads in them, counts this much.
const INFERRED: f64 = 0.8;
/// How much of what weighs against a claim comes off it.
const AGAINST: f64 = 0.8;

/// How far citation `cite` of `source` can be followed, from 0 to 1.
pub fn citation(cite: &Cite, source: &Source) -> f64 {
    match cite.check.as_ref().map(|c| c.state) {
        Some(CheckState::Found) => 1.0,
        // There (a picture, a file that isn't text), its content not words to look for.
        Some(CheckState::Exists) => 0.7,
        // Looked for where it says it is, and not there.
        Some(CheckState::NotFound) => 0.3,
        // A dead link, a file that isn't there.
        Some(CheckState::Gone) => 0.1,
        Some(CheckState::Pending | CheckState::Unchecked) | None => {
            let quoted = cite.quote.as_deref().is_some_and(|q| !q.trim().is_empty());
            let placed = cite.at.as_deref().is_some_and(|a| !a.trim().is_empty()) || pinpointed(&source.locator);
            match (quoted, placed, source.locator.trim().is_empty()) {
                (true, true, _) => 0.75,
                (true, false, _) => 0.65,
                (false, _, false) => 0.5,
                (false, _, true) => 0.2,
            }
        }
    }
}

/// Whether a locator says where in its source: a line (`file.rs:42`), an anchor (`#section`), a
/// page or a time.
fn pinpointed(locator: &str) -> bool {
    let l = locator.trim();
    l.contains('#')
        || l.rsplit_once(':')
            .is_some_and(|(head, tail)| !head.is_empty() && !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit() || c == '-') && !head.ends_with("http") && !head.ends_with("https"))
}

/// Where a source comes from: its origin as the agent gave it, else its site (a URL's host), its
/// DOI, or its file; sources of one origin count once.
pub fn origin(source: &Source) -> String {
    if let Some(o) = source.origin.as_deref().map(str::trim).filter(|o| !o.is_empty()) {
        return o.to_lowercase();
    }
    let l = source.locator.trim();
    if let Some(rest) = l.strip_prefix("https://").or_else(|| l.strip_prefix("http://")) {
        let host = rest.split(['/', '?', '#']).next().unwrap_or(rest).to_lowercase();
        let host = host.rsplit_once('@').map_or(host.as_str(), |(_, h)| h).trim_start_matches("www.").to_owned();
        // A DOI resolver names the work, not a site.
        if host == "doi.org" || host == "dx.doi.org" {
            return format!("doi:{}", rest.split_once('/').map_or("", |(_, d)| d).to_lowercase());
        }
        return host;
    }
    if let Some(doi) = l.strip_prefix("doi:") {
        return format!("doi:{}", doi.trim().to_lowercase());
    }
    // A file, without the line it points to.
    let file = l.rsplit_once(':').filter(|(_, tail)| tail.chars().all(|c| c.is_ascii_digit() || c == '-')).map_or(l, |(head, _)| head);
    format!("{}:{}", source.machine.as_deref().unwrap_or(""), file)
}

/// How confident kiln is in a finding, and why.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Confidence {
    /// Out of 100.
    pub score: u8,
    /// A to F.
    pub grade: &'static str,
    /// "high", "good", "moderate", "low", "very low".
    pub words: &'static str,
    /// What supports it, 0 to 1.
    pub support: f64,
    /// What weighs against it, 0 to 1.
    pub against: f64,
    /// Its sources' kinds, the best of them, 0 to 1.
    pub reliability: f64,
    /// How far its citations can be followed, on average, 0 to 1.
    pub citation: f64,
    /// Origins its sources come from: independent ones.
    pub independent: usize,
    /// Its citations, and of them those kiln found the quote of where they say it is.
    pub cites: usize,
    pub found: usize,
    /// Its citations kiln looked for and didn't find, or found dead.
    pub failed: usize,
    /// Why, in a line.
    pub why: String,
}

/// The grade a score has, and its words.
pub fn grade(score: u8) -> (&'static str, &'static str) {
    match score {
        85.. => ("A", "high"),
        70..=84 => ("B", "good"),
        50..=69 => ("C", "moderate"),
        30..=49 => ("D", "low"),
        _ => ("F", "very low"),
    }
}

/// What citations `cites` lend, one origin counting once (its strongest): one minus what each
/// origin leaves unsupported; and the origins.
fn lend(cites: &[Cite], sources: &BTreeMap<&str, &Source>) -> (f64, usize) {
    let mut best: BTreeMap<String, f64> = BTreeMap::new();
    for c in cites {
        let Some(s) = sources.get(c.source.as_str()) else { continue };
        let weight = s.kind.reliability() * if s.dated { DATED } else { 1.0 } * citation(c, s);
        let e = best.entry(origin(s)).or_insert(0.0);
        *e = e.max(weight);
    }
    let left: f64 = best.values().map(|w| 1.0 - w).product();
    (1.0 - left, best.len())
}

/// What one counterclaim weighs: its sources', or, with none, how strong the agent says it is.
fn weighs(c: &Counter, sources: &BTreeMap<&str, &Source>) -> f64 {
    let (lent, origins) = lend(&c.cites, sources);
    if origins > 0 {
        return lent;
    }
    match c.strength.unwrap_or(Strength::Moderate) {
        Strength::Strong => 0.6,
        Strength::Moderate => 0.35,
        Strength::Weak => 0.15,
    }
}

/// What counterclaim `c` weighs against its finding, 0 to 1, its sources being `sources`.
pub fn against(c: &Counter, sources: &[Source]) -> f64 {
    let by_id: BTreeMap<&str, &Source> = sources.iter().map(|s| (s.id.as_str(), s)).collect();
    weighs(c, &by_id)
}

/// How confident kiln is in finding `f`, its sources being `sources`.
pub fn confidence(f: &Finding, sources: &[Source]) -> Confidence {
    let by_id: BTreeMap<&str, &Source> = sources.iter().map(|s| (s.id.as_str(), s)).collect();
    let (lent, independent) = lend(&f.cites, &by_id);
    let support = lent * if f.inferred { INFERRED } else { 1.0 };
    let against = 1.0 - f.against.iter().map(|c| 1.0 - weighs(c, &by_id)).product::<f64>();
    let raw = support * (1.0 - AGAINST * against);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let score = (raw.clamp(0.0, 1.0) * 100.0).round() as u8;
    let (grade, words) = grade(score);
    let cited: Vec<(&Cite, &Source)> = f.cites.iter().filter_map(|c| by_id.get(c.source.as_str()).map(|s| (c, *s))).collect();
    let reliability = cited.iter().map(|(_, s)| s.kind.reliability()).fold(0.0, f64::max);
    #[allow(clippy::cast_precision_loss)]
    let citation_avg = if cited.is_empty() { 0.0 } else { cited.iter().map(|(c, s)| citation(c, s)).sum::<f64>() / cited.len() as f64 };
    let state = |c: &Cite| c.check.as_ref().map(|k| k.state);
    let found = f.cites.iter().filter(|c| state(c) == Some(CheckState::Found)).count();
    let failed = f.cites.iter().filter(|c| matches!(state(c), Some(CheckState::NotFound | CheckState::Gone))).count();
    let why = why_of(f, &cited, independent, found, failed, &by_id);
    Confidence { score, grade, words, support, against, reliability, citation: citation_avg, independent, cites: f.cites.len(), found, failed, why }
}

/// A finding's confidence in a line: its sources (how many independent, of what kinds), how its
/// citations held up, and what weighs against it.
fn why_of(f: &Finding, cited: &[(&Cite, &Source)], independent: usize, found: usize, failed: usize, sources: &BTreeMap<&str, &Source>) -> String {
    if cited.is_empty() {
        return "no sources".to_owned();
    }
    let mut kinds: Vec<&str> = Vec::new();
    for (_, s) in cited {
        if !kinds.contains(&s.kind.words()) {
            kinds.push(s.kind.words());
        }
    }
    let mut out = format!("{independent} independent source{} ({})", if independent == 1 { "" } else { "s" }, kinds.join(", "));
    let checked = cited.iter().filter(|(c, _)| c.check.as_ref().is_some_and(|k| !matches!(k.state, CheckState::Pending | CheckState::Unchecked))).count();
    if checked > 0 {
        out.push_str(&format!("; {found} of {} quotes found where cited", cited.len()));
        if failed > 0 {
            out.push_str(&format!(", {failed} not"));
        }
    } else if cited.iter().any(|(c, _)| c.quote.as_deref().is_some_and(|q| !q.trim().is_empty())) {
        out.push_str("; quoted, not yet checked");
    } else {
        out.push_str("; cited without quotes");
    }
    if cited.iter().any(|(_, s)| s.dated) {
        out.push_str("; some out of date");
    }
    if f.inferred {
        out.push_str("; inferred from them");
    }
    match f.against.len() {
        0 => {}
        n => {
            let strongest = f.against.iter().map(|c| weighs(c, sources)).fold(0.0, f64::max);
            let how = if strongest >= 0.5 {
                "strong"
            } else if strongest >= 0.25 {
                "moderate"
            } else {
                "weak"
            };
            out.push_str(&format!("; {n} counterclaim{}, the strongest {how}", if n == 1 { "" } else { "s" }));
        }
    }
    out
}

/// A check as kiln keeps it: `state`, now.
pub fn checked(state: CheckState, said: Option<String>, at: i64) -> Check {
    Check { state, at, said }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(id: &str, kind: Type, locator: &str) -> Source {
        Source { id: id.into(), kind, title: id.into(), locator: locator.into(), ..Source::default() }
    }

    fn cite(source: &str, quote: Option<&str>, check: Option<CheckState>) -> Cite {
        Cite { source: source.into(), quote: quote.map(str::to_owned), at: None, check: check.map(|state| Check { state, at: 1, said: None }) }
    }

    fn finding(cites: Vec<Cite>) -> Finding {
        Finding { id: "f1".into(), claim: "x".into(), cites, ..Finding::default() }
    }

    #[test]
    fn a_source_is_taken_at_its_word_as_far_as_its_kind_goes() {
        assert!(Type::Measurement.reliability() > Type::Code.reliability());
        assert!(Type::Docs.reliability() > Type::Blog.reliability() && Type::Blog.reliability() > Type::Forum.reliability());
        assert!(Type::Forum.reliability() > Type::Vendor.reliability() && Type::Vendor.reliability() > Type::Social.reliability());
        assert!(Type::Code.internal() && Type::Record.internal() && !Type::Paper.internal() && !Type::Web.internal());
    }

    #[test]
    fn a_citation_counts_as_far_as_it_can_be_followed() {
        let s = src("s1", Type::Docs, "https://sqlite.org/fts5.html");
        let pinned = src("s2", Type::Code, "crates/kiln/src/api.rs:3713");
        assert!((citation(&cite("s1", Some("q"), Some(CheckState::Found)), &s) - 1.0).abs() < 1e-9);
        assert!((citation(&cite("s2", Some("q"), None), &pinned) - 0.75).abs() < 1e-9, "a quote and the line it's on");
        assert!((citation(&cite("s1", Some("q"), None), &s) - 0.65).abs() < 1e-9, "a quote, not where in the page");
        assert!((citation(&cite("s1", None, None), &s) - 0.5).abs() < 1e-9, "a bare link");
        assert!((citation(&cite("s0", None, None), &src("s0", Type::Web, "")) - 0.2).abs() < 1e-9, "nothing to follow");
        assert!(citation(&cite("s1", Some("q"), Some(CheckState::NotFound)), &s) < 0.5, "looked for, not there");
        assert!(citation(&cite("s1", Some("q"), Some(CheckState::Gone)), &s) < citation(&cite("s1", Some("q"), Some(CheckState::NotFound)), &s), "a dead link least");
        assert!(pinpointed("a.rs:12-30") && pinpointed("https://x.org/a#b") && !pinpointed("https://x.org/a") && !pinpointed("README.md"));
    }

    #[test]
    fn sources_of_one_origin_count_once_and_independent_ones_add_up() {
        let docs = src("s1", Type::Docs, "https://www.sqlite.org/fts5.html");
        let docs_again = src("s2", Type::Docs, "https://sqlite.org/lang.html");
        let blog = src("s3", Type::Blog, "https://example.dev/post");
        assert_eq!(origin(&docs), "sqlite.org");
        assert_eq!(origin(&docs), origin(&docs_again), "one site");
        assert_eq!(origin(&src("s4", Type::Paper, "https://doi.org/10.1145/X")), "doi:10.1145/x");
        assert_eq!(origin(&src("s5", Type::Code, "a/b.rs:10-20")), origin(&src("s6", Type::Code, "a/b.rs:40")), "one file");
        let sources = [docs, docs_again, blog];
        let one = confidence(&finding(vec![cite("s1", Some("q"), Some(CheckState::Found))]), &sources);
        let same_site = confidence(&finding(vec![cite("s1", Some("q"), Some(CheckState::Found)), cite("s2", Some("q"), Some(CheckState::Found))]), &sources);
        let two = confidence(&finding(vec![cite("s1", Some("q"), Some(CheckState::Found)), cite("s3", Some("q"), Some(CheckState::Found))]), &sources);
        assert_eq!((one.score, one.independent), (85, 1));
        assert_eq!((same_site.score, same_site.independent), (85, 1), "a second page of one site adds nothing");
        assert!(two.score > one.score && two.independent == 2, "{two:?}");
        assert_eq!((one.grade, one.words), ("A", "high"));
    }

    #[test]
    fn counterclaims_weigh_against_a_claim_by_their_own_sources() {
        let sources = [src("s1", Type::Code, "x.rs:1"), src("s2", Type::Forum, "https://forum.example/t/1"), src("s3", Type::Paper, "https://doi.org/10.1/a")];
        let base = finding(vec![cite("s1", Some("q"), Some(CheckState::Found))]);
        let clean = confidence(&base, &sources);
        let weak = confidence(&Finding { against: vec![Counter { claim: "no".into(), cites: vec![], strength: Some(Strength::Weak) }], ..base.clone() }, &sources);
        let forum = confidence(&Finding { against: vec![Counter { claim: "no".into(), cites: vec![cite("s2", None, None)], strength: None }], ..base.clone() }, &sources);
        let paper = confidence(&Finding { against: vec![Counter { claim: "no".into(), cites: vec![cite("s3", Some("q"), Some(CheckState::Found))], strength: None }], ..base.clone() }, &sources);
        assert!(clean.score > weak.score && weak.score > forum.score && forum.score > paper.score, "{} {} {} {}", clean.score, weak.score, forum.score, paper.score);
        assert!(paper.why.contains("1 counterclaim, the strongest strong"), "{}", paper.why);
        // Inferred, it counts for less; out of date too.
        assert!(confidence(&Finding { inferred: true, ..base.clone() }, &sources).score < clean.score);
        let dated = [Source { dated: true, ..sources[0].clone() }];
        assert!(confidence(&base, &dated).score < clean.score);
    }

    #[test]
    fn a_claim_without_sources_is_graded_f_and_says_so() {
        let c = confidence(&finding(vec![]), &[]);
        assert_eq!((c.score, c.grade, c.why.as_str()), (0, "F", "no sources"));
        let weak = confidence(&finding(vec![cite("s1", None, None)]), &[src("s1", Type::Blog, "https://example.dev")]);
        assert_eq!(weak.grade, "F", "a lone blog post, linked: {weak:?}");
        assert_eq!(grade(85).0, "A");
        assert_eq!(grade(84).0, "B");
        assert_eq!(grade(50).0, "C");
        assert_eq!(grade(30).0, "D");
        assert_eq!(grade(29).0, "F");
    }
}
