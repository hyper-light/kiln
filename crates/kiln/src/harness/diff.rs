//! Line diffs for previews: what an edit an agent asks to make would change, computed against the
//! file as it is now (harnesses report real patches only after the fact). Myers' O(ND) algorithm
//! on the lines between the common prefix and suffix, with a bound on the work: a change too big
//! to diff cheaply is shown as one replaced block.

use super::model::{FileDiff, Hunk};

/// Lines of unchanged context around each change.
pub const CONTEXT: usize = 3;
/// Most edit distance searched before falling back to "replace the block".
const MAX_D: usize = 2_000;
/// Files larger than this are not read for previews.
pub const MAX_FILE: u64 = 4 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Equal(usize, usize),
    Delete(usize),
    Insert(usize),
}

/// Unified hunks turning `old` into `new`.
pub fn hunks(old: &str, new: &str, context: usize) -> Vec<Hunk> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let ops = ops(&a, &b);
    group(&a, &b, &ops, context)
}

/// A whole file appearing.
pub fn created(path: &str, content: &str) -> FileDiff {
    let lines: Vec<String> = content.lines().map(|l| format!("+{l}")).collect();
    let n = u32::try_from(lines.len()).unwrap_or(u32::MAX);
    FileDiff {
        path: path.to_owned(),
        created: true,
        hunks: if lines.is_empty() { Vec::new() } else { vec![Hunk { old_start: 0, old_lines: 0, new_start: 1, new_lines: n, lines }] },
    }
}

fn ops(a: &[&str], b: &[&str]) -> Vec<Op> {
    let prefix = a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count();
    let rest_a = a.get(prefix..).unwrap_or_default();
    let rest_b = b.get(prefix..).unwrap_or_default();
    let suffix = rest_a.iter().rev().zip(rest_b.iter().rev()).take_while(|(x, y)| x == y).count();
    let mid_a = rest_a.get(..rest_a.len() - suffix).unwrap_or_default();
    let mid_b = rest_b.get(..rest_b.len() - suffix).unwrap_or_default();
    let mut out: Vec<Op> = (0..prefix).map(|i| Op::Equal(i, i)).collect();
    let middle = myers(mid_a, mid_b).unwrap_or_else(|| {
        // Too different to diff cheaply: delete the old block, insert the new one.
        (0..mid_a.len()).map(Op::Delete).chain((0..mid_b.len()).map(Op::Insert)).collect()
    });
    out.extend(middle.into_iter().map(|op| match op {
        Op::Equal(i, j) => Op::Equal(i + prefix, j + prefix),
        Op::Delete(i) => Op::Delete(i + prefix),
        Op::Insert(j) => Op::Insert(j + prefix),
    }));
    let (sa, sb) = (prefix + mid_a.len(), prefix + mid_b.len());
    out.extend((0..suffix).map(|i| Op::Equal(sa + i, sb + i)));
    out
}

/// Myers' greedy shortest edit script. `None` when the distance exceeds `MAX_D`.
fn myers(a: &[&str], b: &[&str]) -> Option<Vec<Op>> {
    let n = isize::try_from(a.len()).ok()?;
    let m = isize::try_from(b.len()).ok()?;
    if n == 0 || m == 0 {
        return Some((0..a.len()).map(Op::Delete).chain((0..b.len()).map(Op::Insert)).collect());
    }
    let max = usize::try_from(n + m).ok()?.min(MAX_D);
    // v[k] for diagonals -d..=d, stored at k + d; one snapshot per d for the walk back.
    let mut trace: Vec<Vec<isize>> = Vec::new();
    let mut v: Vec<isize> = vec![0; 3];
    for d in 0..=max {
        let di = isize::try_from(d).ok()?;
        let mut next = vec![0isize; 2 * d + 3];
        let at = |vec: &Vec<isize>, dd: isize, k: isize| -> isize {
            usize::try_from(k + dd + 1).ok().and_then(|i| vec.get(i).copied()).unwrap_or(-1)
        };
        let prev_d = di - 1;
        let mut k = -di;
        while k <= di {
            let down = k == -di || (k != di && at(&v, prev_d, k - 1) < at(&v, prev_d, k + 1));
            let mut x = if d == 0 { 0 } else if down { at(&v, prev_d, k + 1) } else { at(&v, prev_d, k - 1) + 1 };
            let mut y = x - k;
            while x < n && y < m && a.get(x as usize) == b.get(y as usize) {
                x += 1;
                y += 1;
            }
            if let Some(slot) = usize::try_from(k + di + 1).ok().and_then(|i| next.get_mut(i)) {
                *slot = x;
            }
            if x >= n && y >= m {
                trace.push(next);
                return Some(backtrack(&trace, a.len(), b.len()));
            }
            k += 2;
        }
        trace.push(next.clone());
        v = next;
    }
    None
}

fn backtrack(trace: &[Vec<isize>], n: usize, m: usize) -> Vec<Op> {
    let at = |vec: &Vec<isize>, dd: isize, k: isize| -> isize {
        usize::try_from(k + dd + 1).ok().and_then(|i| vec.get(i).copied()).unwrap_or(-1)
    };
    let mut ops = Vec::new();
    let (mut x, mut y) = (n as isize, m as isize);
    for d in (0..trace.len()).rev() {
        let di = d as isize;
        let k = x - y;
        if d == 0 {
            while x > 0 && y > 0 {
                x -= 1;
                y -= 1;
                ops.push(Op::Equal(x as usize, y as usize));
            }
            break;
        }
        let Some(prev) = trace.get(d - 1) else { break };
        let pd = di - 1;
        let down = k == -di || (k != di && at(prev, pd, k - 1) < at(prev, pd, k + 1));
        let prev_k = if down { k + 1 } else { k - 1 };
        let prev_x = at(prev, pd, prev_k);
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            x -= 1;
            y -= 1;
            ops.push(Op::Equal(x as usize, y as usize));
        }
        if down {
            y -= 1;
            ops.push(Op::Insert(y.max(0) as usize));
        } else {
            x -= 1;
            ops.push(Op::Delete(x.max(0) as usize));
        }
    }
    ops.reverse();
    ops
}

fn group(a: &[&str], b: &[&str], ops: &[Op], context: usize) -> Vec<Hunk> {
    let mut hunks = Vec::new();
    let changed: Vec<usize> = ops.iter().enumerate().filter(|(_, op)| !matches!(op, Op::Equal(..))).map(|(i, _)| i).collect();
    let Some(&first) = changed.first() else { return hunks };
    // Ranges of op indexes, each a run of changes padded with context, merged when they touch.
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let (mut start, mut end) = (first.saturating_sub(context), first);
    for &c in &changed {
        if c > end + 2 * context {
            ranges.push((start, (end + context).min(ops.len() - 1)));
            start = c.saturating_sub(context);
        }
        end = c;
    }
    ranges.push((start, (end + context).min(ops.len().saturating_sub(1))));
    for (s, e) in ranges {
        let slice = ops.get(s..=e).unwrap_or_default();
        let mut lines = Vec::with_capacity(slice.len());
        let (mut old_start, mut new_start) = (None, None);
        let (mut old_lines, mut new_lines) = (0u32, 0u32);
        // Where the hunk sits when one side is empty: just after the last line before it.
        let mut old_before = 0usize;
        let mut new_before = 0usize;
        for op in ops.get(..s).unwrap_or_default().iter().rev() {
            match *op {
                Op::Equal(i, j) => {
                    old_before = old_before.max(i + 1);
                    new_before = new_before.max(j + 1);
                    break;
                }
                Op::Delete(i) => old_before = old_before.max(i + 1),
                Op::Insert(j) => new_before = new_before.max(j + 1),
            }
        }
        for op in slice {
            match *op {
                Op::Equal(i, j) => {
                    old_start.get_or_insert(i + 1);
                    new_start.get_or_insert(j + 1);
                    old_lines += 1;
                    new_lines += 1;
                    lines.push(format!(" {}", a.get(i).copied().unwrap_or("")));
                }
                Op::Delete(i) => {
                    old_start.get_or_insert(i + 1);
                    old_lines += 1;
                    lines.push(format!("-{}", a.get(i).copied().unwrap_or("")));
                }
                Op::Insert(j) => {
                    new_start.get_or_insert(j + 1);
                    new_lines += 1;
                    lines.push(format!("+{}", b.get(j).copied().unwrap_or("")));
                }
            }
        }
        let to_u32 = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
        hunks.push(Hunk {
            old_start: to_u32(old_start.unwrap_or(old_before)),
            old_lines,
            new_start: to_u32(new_start.unwrap_or(new_before)),
            new_lines,
            lines,
        });
    }
    hunks
}

/// The hunks of a unified diff of one file (headers before the first `@@` are skipped).
pub fn parse_unified(diff: &str) -> Vec<Hunk> {
    let mut out: Vec<Hunk> = Vec::new();
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("@@ ") {
            // @@ -a,b +c,d @@ optional heading
            let mut nums = rest.split_whitespace();
            let parse = |s: Option<&str>, sign: char| -> (u32, u32) {
                let s = s.unwrap_or("").trim_start_matches(sign);
                let mut it = s.split(',');
                let start = it.next().and_then(|n| n.parse().ok()).unwrap_or(0);
                let len = it.next().and_then(|n| n.parse().ok()).unwrap_or(1);
                (start, len)
            };
            let (old_start, old_lines) = parse(nums.next(), '-');
            let (new_start, new_lines) = parse(nums.next(), '+');
            out.push(Hunk { old_start, old_lines, new_start, new_lines, lines: Vec::new() });
            continue;
        }
        let Some(h) = out.last_mut() else { continue };
        if line.starts_with('\\') {
            continue; // "\ No newline at end of file"
        }
        if line.starts_with(['+', '-', ' ']) {
            h.lines.push(line.to_owned());
        } else if line.is_empty() {
            h.lines.push(" ".to_owned());
        }
    }
    out
}

/// The text before `hunks` were applied to it, given the text after. None when they do not fit.
pub fn reverse_apply(now: &str, hunks: &[Hunk]) -> Option<String> {
    let lines: Vec<&str> = now.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut cursor = 0usize; // next line of `now` not yet copied (0-based)
    for h in hunks {
        // Where the hunk's new side begins in `now`.
        let start = if h.new_lines == 0 { h.new_start as usize } else { (h.new_start as usize).saturating_sub(1) };
        if start < cursor || start > lines.len() {
            return None;
        }
        out.extend(lines.get(cursor..start)?.iter().map(|l| (*l).to_owned()));
        cursor = start;
        for l in &h.lines {
            let (tag, text) = l.split_at(l.len().min(1));
            match tag {
                " " => {
                    if lines.get(cursor).copied()? != text {
                        return None;
                    }
                    out.push(text.to_owned());
                    cursor += 1;
                }
                "+" => {
                    if lines.get(cursor).copied()? != text {
                        return None;
                    }
                    cursor += 1;
                }
                "-" => out.push(text.to_owned()),
                _ => return None,
            }
        }
    }
    out.extend(lines.get(cursor..)?.iter().map(|l| (*l).to_owned()));
    let mut text = out.join("\n");
    if now.ends_with('\n') || (now.is_empty() && !text.is_empty()) {
        text.push('\n');
    }
    Some(text)
}

/// Lines of hunks kept per changed file (the counts always cover everything).
const MAX_CHANGE_LINES: usize = 4_000;

/// A file's cumulative change: its baseline against how it is on disk now. Blocking (reads the
/// file). Returns the change (None: no net change) and, when the baseline had to be worked out
/// from the file (`Baseline::Reverse`), the baseline found, to keep for later changes.
pub fn file_change(job: &super::core::ChangeJob) -> (Option<super::model::FileChange>, Option<super::core::Baseline>) {
    use super::core::Baseline;
    let path = std::path::Path::new(&job.abs);
    let meta = std::fs::metadata(path).ok().filter(|m| m.is_file());
    let now: Option<Result<String, ()>> = meta.map(|m| {
        if m.len() > MAX_FILE {
            return Err(());
        }
        std::fs::read(path).ok().and_then(|b| String::from_utf8(b).ok()).ok_or(())
    });
    if let Baseline::Reverse(hunks) = &job.baseline {
        let resolved = match &now {
            Some(Ok(text)) => reverse_apply(text, hunks).map_or(Baseline::TooLarge, Baseline::Text),
            Some(Err(())) => Baseline::TooLarge,
            // Gone already: what it was is what the patch removed.
            None => Baseline::Text(hunks.iter().flat_map(|h| h.lines.iter()).filter_map(|l| l.strip_prefix('-').or_else(|| l.strip_prefix(' '))).map(|l| format!("{l}\n")).collect()),
        };
        let job = super::core::ChangeJob { abs: job.abs.clone(), rel: job.rel.clone(), baseline: resolved.clone() };
        return (compare(&job, now), Some(resolved));
    }
    (compare(job, now), None)
}

fn compare(job: &super::core::ChangeJob, now: Option<Result<String, ()>>) -> Option<super::model::FileChange> {
    use super::core::Baseline;
    use super::model::FileChange;
    let at = crate::util::now_ms();
    let count = |hunks: &[Hunk]| -> (u32, u32) {
        let mut add = 0u32;
        let mut del = 0u32;
        for l in hunks.iter().flat_map(|h| h.lines.iter()) {
            match l.as_bytes().first() {
                Some(b'+') => add += 1,
                Some(b'-') => del += 1,
                _ => {}
            }
        }
        (add, del)
    };
    let shown = |mut hunks: Vec<Hunk>| -> (Vec<Hunk>, bool) {
        let mut total = 0usize;
        let mut keep = hunks.len();
        for (i, h) in hunks.iter().enumerate() {
            total += h.lines.len();
            if total > MAX_CHANGE_LINES {
                keep = i;
                break;
            }
        }
        let cut = keep < hunks.len();
        hunks.truncate(keep);
        (hunks, cut)
    };
    let blank = |rel: &str| FileChange { path: rel.to_owned(), created: false, deleted: false, added: 0, removed: 0, hunks: Vec::new(), truncated: true, at };
    match (&job.baseline, now) {
        (Baseline::Absent, None) => None,
        (Baseline::Absent, Some(Ok(text))) => {
            let d = created(&job.rel, &text);
            let (add, _) = count(&d.hunks);
            let (hunks, cut) = shown(d.hunks);
            Some(FileChange { path: job.rel.clone(), created: true, deleted: false, added: add, removed: 0, hunks, truncated: cut, at })
        }
        (Baseline::Text(before), None) => {
            let hunks = hunks(before, "", CONTEXT);
            let (_, del) = count(&hunks);
            let (hunks, cut) = shown(hunks);
            Some(FileChange { path: job.rel.clone(), created: false, deleted: true, added: 0, removed: del, hunks, truncated: cut, at })
        }
        (Baseline::Text(before), Some(Ok(text))) => {
            if *before == text {
                return None;
            }
            let all = hunks(before, &text, CONTEXT);
            if all.is_empty() {
                return None;
            }
            let (add, del) = count(&all);
            let (hunks, cut) = shown(all);
            Some(FileChange { path: job.rel.clone(), created: false, deleted: false, added: add, removed: del, hunks, truncated: cut, at })
        }
        (Baseline::TooLarge | Baseline::Reverse(_), _) | (_, Some(Err(()))) => Some(blank(&job.rel)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(old: &str, hunks: &[Hunk]) -> String {
        // Rebuild the new text from the hunks (context and additions) plus untouched old lines.
        let old_lines: Vec<&str> = old.lines().collect();
        let mut out: Vec<String> = Vec::new();
        let mut cursor = 0usize; // next old line (0-based) not yet copied
        for h in hunks {
            let start = (h.old_start as usize).saturating_sub(1);
            let start = if h.old_lines == 0 { h.old_start as usize } else { start };
            while cursor < start {
                out.push(old_lines.get(cursor).copied().unwrap_or("").to_owned());
                cursor += 1;
            }
            for l in &h.lines {
                let (tag, text) = l.split_at(1);
                match tag {
                    " " => {
                        out.push(text.to_owned());
                        cursor += 1;
                    }
                    "-" => cursor += 1,
                    _ => out.push(text.to_owned()),
                }
            }
        }
        while cursor < old_lines.len() {
            out.push(old_lines.get(cursor).copied().unwrap_or("").to_owned());
            cursor += 1;
        }
        out.join("\n")
    }

    #[test]
    fn small_edit_in_a_long_file() {
        let old: String = (1..=40).map(|i| format!("line {i}\n")).collect();
        let new = old.replace("line 20\n", "line twenty\nline 20b\n");
        let h = hunks(&old, &new, CONTEXT);
        assert_eq!(h.len(), 1);
        let h0 = h.first().cloned().unwrap_or(Hunk { old_start: 0, old_lines: 0, new_start: 0, new_lines: 0, lines: vec![] });
        assert_eq!((h0.old_start, h0.old_lines, h0.new_start, h0.new_lines), (17, 7, 17, 8));
        assert_eq!(h0.lines.iter().filter(|l| l.starts_with('-')).count(), 1);
        assert_eq!(h0.lines.iter().filter(|l| l.starts_with('+')).count(), 2);
        assert_eq!(apply(&old, &h), new.trim_end());
    }

    #[test]
    fn separate_changes_make_separate_hunks_and_round_trip() {
        let old: String = (1..=60).map(|i| format!("L{i}\n")).collect();
        let new: String = (1..=60)
            .filter_map(|i| match i {
                5 => Some("five\n".to_owned()),
                50 => None,
                33 => Some("L33\nextra\n".to_owned()),
                _ => Some(format!("L{i}\n")),
            })
            .collect();
        let h = hunks(&old, &new, CONTEXT);
        assert_eq!(h.len(), 3, "{h:?}");
        assert_eq!(apply(&old, &h), new.trim_end());
        // Identical texts: nothing.
        assert!(hunks(&old, &old, CONTEXT).is_empty());
        // From empty, and to empty.
        let all = hunks("", "a\nb\n", CONTEXT);
        assert_eq!(all.first().map(|h| h.lines.clone()), Some(vec!["+a".to_owned(), "+b".to_owned()]));
        let none = hunks("a\nb\n", "", CONTEXT);
        assert_eq!(none.first().map(|h| (h.old_lines, h.new_lines)), Some((2, 0)));
    }

    #[test]
    fn interleaved_changes_round_trip() {
        let old = "a\nb\nc\nd\ne\nf\ng\n";
        let new = "a\nB\nc\nD\ne\nf\nG\nh\n";
        let h = hunks(old, new, 1);
        assert_eq!(apply(old, &h), new.trim_end());
        let created = created("x.rs", "fn main() {}\n");
        assert!(created.created);
        assert_eq!(created.hunks.first().map(|h| h.new_lines), Some(1));
    }

    #[test]
    fn unified_diffs_parse_and_reverse() {
        let d = "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1,3 +1,4 @@ fn x\n a\n-b\n+B\n+B2\n c\n@@ -10 +11,0 @@\n-z\n";
        let h = parse_unified(d);
        assert_eq!(h.len(), 2);
        assert_eq!(h.first().map(|h| (h.old_start, h.old_lines, h.new_start, h.new_lines)), Some((1, 3, 1, 4)));
        assert_eq!(h.get(1).map(|h| (h.old_start, h.old_lines, h.new_start, h.new_lines)), Some((10, 1, 11, 0)));
        let before = "a\nb\nc\n";
        let after = "a\nB\nB2\nc\n";
        let hh = hunks(before, after, CONTEXT);
        assert_eq!(reverse_apply(after, &hh).as_deref(), Some(before));
        // A patch that does not fit the text.
        assert!(reverse_apply("q\nr\n", &hh).is_none());
    }

    #[test]
    fn cumulative_file_changes() -> anyhow::Result<()> {
        use super::super::core::{Baseline, ChangeJob};
        let dir = tempfile::tempdir()?;
        let f = dir.path().join("a.txt");
        let abs = f.to_string_lossy().into_owned();
        std::fs::write(&f, "one\ntwo\nthree\n")?;
        let job = |b: Baseline| ChangeJob { abs: abs.clone(), rel: "a.txt".into(), baseline: b };
        // Edited twice since the baseline: one change, counted once.
        let c = file_change(&job(Baseline::Text("one\n2\nthree\n".into()))).0.ok_or_else(|| anyhow::anyhow!("no change"))?;
        assert_eq!((c.added, c.removed, c.created, c.deleted), (1, 1, false, false));
        // Back to how it was: nothing.
        assert!(file_change(&job(Baseline::Text("one\ntwo\nthree\n".into()))).0.is_none());
        // A baseline worked out by undoing a patch on the file as it is now.
        let patch = parse_unified("--- a/a.txt\n+++ b/a.txt\n@@ -1,3 +1,3 @@\n one\n-2\n+two\n three\n");
        let (c, resolved) = file_change(&job(Baseline::Reverse(patch)));
        assert_eq!(resolved, Some(Baseline::Text("one\n2\nthree\n".into())));
        assert_eq!(c.map(|c| (c.added, c.removed)), Some((1, 1)));
        // New file.
        let c = file_change(&job(Baseline::Absent)).0.ok_or_else(|| anyhow::anyhow!("no change"))?;
        assert!(c.created && c.added == 3);
        // Deleted.
        std::fs::remove_file(&f)?;
        let c = file_change(&job(Baseline::Text("x\ny\n".into()))).0.ok_or_else(|| anyhow::anyhow!("no change"))?;
        assert!(c.deleted && c.removed == 2);
        assert!(file_change(&job(Baseline::Absent)).0.is_none());
        Ok(())
    }
}
