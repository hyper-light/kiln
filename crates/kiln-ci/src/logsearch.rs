//! U05/W19-R04: deterministic literal log search; the host stores only candidate postings.
use std::collections::BTreeSet;

/// Unique Unicode trigrams of the same lowercase form used by actual line matching.
/// A posting is only a candidate: it never substitutes for checking the whole literal phrase.
pub fn trigrams(text: &str) -> BTreeSet<i64> {
    // Three Unicode scalar values fit losslessly in 63 bits (21 each). Compact integer keys
    // avoid allocating a String for every overlapping trigram in multi-megabyte log lines.
    let mut out = BTreeSet::new();
    let lower = text.to_lowercase();
    let mut chars = lower.chars().map(|c| i64::from(u32::from(c)));
    let (Some(mut a), Some(mut b)) = (chars.next(), chars.next()) else { return out };
    for c in chars {
        out.insert((a << 42) | (b << 21) | c);
        a = b;
        b = c;
    }
    out
}

/// Preserve the existing line/snippet/byte-offset contract, including lowercase byte offsets.
pub fn lines_with(body: &str, first: usize, query: &str) -> Vec<(usize, String, usize)> {
    let needle = query.to_lowercase();
    body.split('\n').enumerate().filter_map(|(i, line)| {
        let at = line.to_lowercase().find(&needle)?;
        let mut end = line.len().min(2000);
        while !line.is_char_boundary(end) { end = end.saturating_sub(1); }
        Some((first.saturating_add(i), line.get(..end).unwrap_or_default().to_owned(), at))
    }).collect()
}
