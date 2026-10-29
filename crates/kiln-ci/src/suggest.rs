//! Suggestions (docs/ci.md W26-R10), the pure part: a comment's `suggestion` blocks as
//! GitHub-flavoured markdown has them, with its old `suggestion` field read as one more; what
//! applying one does to a file (the lines its thread is on replaced, by the file's own numbers);
//! and the commit's `Co-authored-by` trailers, GitHub's way: whoever applies it commits it, and
//! whoever suggested it is a co-author. The server reads the comment and the file, follows the
//! thread's lines to the latest revision (or finds them outdated), and makes the commit.

use serde::Serialize;

/// One of a comment's suggestions (W26-R10): the lines it puts where its thread's lines are (none:
/// they go).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Suggestion {
    /// Its place among the comment's suggestions, from 0: its body's blocks in order, then its old
    /// `suggestion` field.
    pub index: u32,
    pub lines: Vec<String>,
    /// It's the comment's old `suggestion` field, not a block of its body.
    pub field: bool,
}

/// A fenced code block's opening line (CommonMark 4.5): up to three spaces, three or more
/// backticks or tildes, and its info string (a backtick fence's has no backtick in it). Its
/// indent, its character, its length and its info string.
fn opening(line: &str) -> Option<(usize, char, usize, &str)> {
    let rest = line.trim_start_matches(' ');
    let indent = line.len() - rest.len();
    let ch = rest.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let after = rest.trim_start_matches(ch);
    let (n, info) = (rest.len() - after.len(), after.trim());
    (indent <= 3 && n >= 3 && !(ch == '`' && info.contains('`'))).then_some((indent, ch, n, info))
}

/// Whether `line` closes a fence of `n` `ch`: up to three spaces, at least as long a run of the
/// same character, then only spaces or tabs.
fn closes(line: &str, ch: char, n: usize) -> bool {
    let rest = line.trim_start_matches(' ');
    let after = rest.trim_start_matches(ch);
    line.len() - rest.len() <= 3 && rest.len() - after.len() >= n && after.trim_matches([' ', '\t']).is_empty()
}

/// A line of a fenced block without the indent its opening fence had (up to `n` spaces).
fn dedent(line: &str, n: usize) -> &str {
    let spaces = line.len() - line.trim_start_matches(' ').len();
    line.get(spaces.min(n)..).unwrap_or(line)
}

/// A fence being read: its character, length and indent, and its lines when it's a suggestion.
struct Fence {
    ch: char,
    n: usize,
    indent: usize,
    lines: Option<Vec<String>>,
}

/// The `suggestion` blocks of a comment's markdown, in order, each its lines: the fenced code
/// blocks whose info string's first word is `suggestion` (GitHub's), fenced with backticks or
/// tildes, closed by a fence at least as long (so a block fenced with four backticks carries lines
/// of three) or else running to the comment's end, as CommonMark has them. What another fenced
/// block holds is its text, never a suggestion, and so is a quoted block (`> `): it's another
/// comment's suggestion, not this one's.
pub fn blocks(markdown: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let mut open: Option<Fence> = None;
    for line in markdown.lines() {
        match open.take() {
            Some(mut f) => {
                if closes(line, f.ch, f.n) {
                    out.extend(f.lines);
                } else {
                    if let Some(l) = f.lines.as_mut() {
                        l.push(dedent(line, f.indent).to_owned());
                    }
                    open = Some(f);
                }
            }
            None => {
                open = opening(line).map(|(indent, ch, n, info)| Fence { ch, n, indent, lines: (info.split_whitespace().next() == Some("suggestion")).then(Vec::new) });
            }
        }
    }
    out.extend(open.and_then(|f| f.lines));
    out
}

/// A comment's suggestions (W26-R10): its body's blocks, then its old `suggestion` field as one
/// more (its lines; empty, the lines go) unless a block of its body says the same.
pub fn suggestions(body: &str, field: Option<&str>) -> Vec<Suggestion> {
    let mut out: Vec<Suggestion> = blocks(body).into_iter().map(|lines| Suggestion { lines, ..Suggestion::default() }).collect();
    if let Some(f) = field {
        let lines: Vec<String> = f.lines().map(str::to_owned).collect();
        if !out.iter().any(|s| s.lines == lines) {
            out.push(Suggestion { lines, field: true, ..Suggestion::default() });
        }
    }
    for (i, s) in out.iter_mut().enumerate() {
        s.index = u32::try_from(i).unwrap_or(u32::MAX);
    }
    out
}

/// `text` with its lines `start..=end` (the file's own numbers, from 1) replaced by `lines` (none:
/// they go). The lines put in end as the file's do (`\r\n` in a file of them), and its last line
/// ends as it did; the rest is left byte for byte. `None` when the file has no such lines.
pub fn replace(text: &str, (start, end): (u32, u32), lines: &[String]) -> Option<String> {
    let pieces: Vec<&str> = text.split_inclusive('\n').collect();
    let (lo, hi) = (usize::try_from(start).ok()?, usize::try_from(end).ok()?);
    if lo == 0 || hi < lo || hi > pieces.len() {
        return None;
    }
    let eol = if pieces.first().is_some_and(|p| p.ends_with("\r\n")) { "\r\n" } else { "\n" };
    // The file's last line is among those replaced, and it has no line ending.
    let unended = hi == pieces.len() && !text.ends_with('\n');
    let mut out = String::with_capacity(text.len());
    out.extend(pieces.get(..lo - 1)?.iter().copied());
    for (i, l) in lines.iter().enumerate() {
        out.push_str(l);
        if !(unended && i + 1 == lines.len()) {
            out.push_str(eol);
        }
    }
    out.extend(pieces.get(hi..)?.iter().copied());
    Some(out)
}

/// `message` with a `Co-authored-by` trailer for each of `co_authors` (`Name <email>`), GitHub's
/// way of naming a commit's other authors (a suggestion's suggester, W26-R10): among its trailers,
/// or as their paragraph. None for its `author`, and none it has already.
pub fn with_co_authors(message: &str, author: &str, co_authors: &[String]) -> String {
    let body = message.trim_end();
    let has = crate::review::trailers(body);
    let mut add: Vec<&str> = Vec::new();
    for c in co_authors.iter().map(|c| c.trim()) {
        let known = c.eq_ignore_ascii_case(author.trim()) || add.iter().any(|a| a.eq_ignore_ascii_case(c)) || has.iter().any(|(k, v)| k.eq_ignore_ascii_case("Co-authored-by") && v.eq_ignore_ascii_case(c));
        if !c.is_empty() && !known {
            add.push(c);
        }
    }
    let mut out = String::from(body);
    if !add.is_empty() {
        out.push_str(if has.is_empty() { "\n\n" } else { "\n" });
        out.push_str(&add.iter().map(|c| format!("Co-authored-by: {c}")).collect::<Vec<_>>().join("\n"));
    }
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &[&str]) -> Vec<String> {
        text.iter().map(|l| (*l).to_owned()).collect()
    }

    #[test]
    fn w26_r10_a_suggestion_block_is_a_suggestion() {
        let one = "Return early: an empty matrix has no rows.\n\n```suggestion\nif m.is_empty() { return Ok(vec![]) }\n```\n";
        assert_eq!(blocks(one), [lines(&["if m.is_empty() { return Ok(vec![]) }"])]);
        // Tildes, words after `suggestion`, up to three spaces (taken off each line, as far as it goes).
        assert_eq!(blocks("~~~suggestion\na\n~~~"), [lines(&["a"])]);
        assert_eq!(blocks("```suggestion for line 42\na\n```"), [lines(&["a"])]);
        assert_eq!(blocks("   ```suggestion\n     indented\n  less\nnone\n   ```"), [lines(&["  indented", "less", "none"])]);
        // Lines that go, and a line made empty, aren't the same.
        assert_eq!(blocks("```suggestion\n```"), [Vec::<String>::new()]);
        assert_eq!(blocks("```suggestion\n\n```"), [lines(&[""])]);
        // Not suggestions: another language, no language, inline code, an indented code block, a
        // fence too short.
        for not in ["```rust\nfn f() {}\n```", "```\nplain\n```", "Use `suggestion` blocks: ```suggestion x```", "    ```suggestion\n    a\n    ```", "``suggestion\na\n``", "```Suggestion\na\n```"] {
            assert_eq!(blocks(not), Vec::<Vec<String>>::new(), "{not:?}");
        }
        // Lines end as the comment's do (a browser's `\r\n`).
        assert_eq!(blocks("```suggestion\r\nx = 1\r\ny = 2\r\n```\r\n"), [lines(&["x = 1", "y = 2"])]);
    }

    #[test]
    fn w26_r10_several_blocks_in_order() {
        let body = "Either\n\n```suggestion\nlet n = m.len();\n```\n\nor, keeping the loop:\n\n```rust\nfor r in m {}\n```\n\n```suggestion\nlet n = m.iter().count();\nlet _ = n;\n```\n";
        assert_eq!(blocks(body), [lines(&["let n = m.len();"]), lines(&["let n = m.iter().count();", "let _ = n;"])]);
        let s = suggestions(body, None);
        assert_eq!(s.iter().map(|s| (s.index, s.field)).collect::<Vec<_>>(), [(0, false), (1, false)]);
    }

    #[test]
    fn w26_r10_nested_backticks() {
        // A suggestion for a markdown file carries its fences, fenced longer than they are.
        let md = "````suggestion\nRun it:\n\n```sh\nkiln run\n```\n````\n";
        assert_eq!(blocks(md), [lines(&["Run it:", "", "```sh", "kiln run", "```"])]);
        // A suggestion's fence closes only with as long a fence of its own character, with nothing after it.
        assert_eq!(blocks("```suggestion\n~~~\n``\n```rust\n```"), [lines(&["~~~", "``", "```rust"])]);
        assert_eq!(blocks("~~~suggestion\n```\n~~~~  \t"), [lines(&["```"])]);
        assert_eq!(blocks("```suggestion\na\n   ``````\nafter\n"), [lines(&["a"])], "a longer closing fence, indented, closes it");
        // A suggestion shown inside another block is that block's text.
        let howto = "Write it so:\n\n````markdown\n```suggestion\nnew line\n```\n````\n";
        assert_eq!(blocks(howto), Vec::<Vec<String>>::new());
        assert_eq!(blocks("~~~\n```suggestion\nx\n```\n~~~"), Vec::<Vec<String>>::new());
        // A backtick fence's info string has no backtick: that line is prose, and the block after it counts.
        assert_eq!(blocks("```suggestion `x`\n```suggestion\ny\n```"), [lines(&["y"])]);
        // Inline code holding backticks is prose.
        assert_eq!(blocks("Use ``` `` ``` here, then\n```suggestion\nz\n```"), [lines(&["z"])]);
    }

    #[test]
    fn w26_r10_quoted_and_unclosed_blocks() {
        // Quoting someone's suggestion (the ⋯ menu's quote) suggests nothing.
        assert_eq!(blocks("> ```suggestion\n> TWO\n> ```\n\nAgreed."), Vec::<Vec<String>>::new());
        assert_eq!(blocks("> said\n\n```suggestion\nmine\n```"), [lines(&["mine"])]);
        // Unclosed, a block runs to the comment's end, as CommonMark has it.
        assert_eq!(blocks("```suggestion\na\nb\n"), [lines(&["a", "b"])]);
        assert_eq!(blocks("```suggestion"), [Vec::<String>::new()]);
    }

    #[test]
    fn w26_r10_the_old_field_is_one_block_more() {
        // Written in the old field alone, it's the comment's one suggestion, as a block would be.
        let old = suggestions("shout it", Some("TWO"));
        let new = suggestions("shout it\n\n```suggestion\nTWO\n```", None);
        assert_eq!(old, [Suggestion { index: 0, lines: lines(&["TWO"]), field: true }]);
        assert_eq!(new, [Suggestion { index: 0, lines: lines(&["TWO"]), field: false }]);
        assert_eq!(old.first().map(|s| &s.lines), new.first().map(|s| &s.lines), "they apply alike");
        // After the body's blocks, unless one says the same; its lines as the field has them.
        assert_eq!(suggestions("```suggestion\na\n```", Some("b\nc\n")).iter().map(|s| (s.index, s.lines.clone(), s.field)).collect::<Vec<_>>(), [(0, lines(&["a"]), false), (1, lines(&["b", "c"]), true)]);
        assert_eq!(suggestions("```suggestion\na\n```", Some("a\n")).len(), 1);
        assert_eq!(suggestions("text", Some("")), [Suggestion { index: 0, lines: Vec::new(), field: true }], "an empty field takes the lines out");
        assert_eq!(suggestions("text", None), Vec::<Suggestion>::new());
    }

    #[test]
    fn w26_r10_a_suggestion_replaces_its_lines() {
        let file = "one\ntwo\nthree\n";
        assert_eq!(replace(file, (2, 2), &lines(&["TWO"])).as_deref(), Some("one\nTWO\nthree\n"), "one line");
        assert_eq!(replace(file, (2, 3), &lines(&["2", "3", "3½"])).as_deref(), Some("one\n2\n3\n3½\n"), "several, more of them");
        assert_eq!(replace(file, (1, 2), &[]).as_deref(), Some("three\n"), "none: they go");
        assert_eq!(replace(file, (3, 3), &lines(&[""])).as_deref(), Some("one\ntwo\n\n"), "a line made empty");
        // The file's own line endings, and its last line's.
        assert_eq!(replace("a\r\nb\r\nc", (2, 2), &lines(&["B", "B2"])).as_deref(), Some("a\r\nB\r\nB2\r\nc"));
        assert_eq!(replace("a\nb", (2, 2), &lines(&["B", "C"])).as_deref(), Some("a\nB\nC"), "no line ending at the end, as before");
        assert_eq!(replace("a\nb", (1, 1), &lines(&["A"])).as_deref(), Some("A\nb"));
        // Lines the file hasn't.
        assert_eq!(replace(file, (3, 4), &lines(&["x"])), None);
        assert_eq!(replace(file, (0, 1), &lines(&["x"])), None);
        assert_eq!(replace(file, (3, 2), &lines(&["x"])), None);
        assert_eq!(replace("", (1, 1), &lines(&["x"])), None);
    }

    #[test]
    fn w26_r10_suggesters_are_co_authors() {
        const REVIEW: &str = "agent:review <agent-review@kiln>";
        let co = |m: &str, author: &str, who: &[&str]| with_co_authors(m, author, &lines(who));
        assert_eq!(co("Apply a suggestion from agent:review\n\nOn matrix.rs, line 42.\n", "ada <ada@kiln>", &[REVIEW]), format!("Apply a suggestion from agent:review\n\nOn matrix.rs, line 42.\n\nCo-authored-by: {REVIEW}\n"));
        // An amended diff's message keeps its trailers, the co-author among them.
        let amended = co("matrix: dynamic\n\nWhy.\n\nChange-Id: I1111111111111111111111111111111111111111\n", "sam <sam@kiln>", &[REVIEW]);
        assert_eq!(amended, format!("matrix: dynamic\n\nWhy.\n\nChange-Id: I1111111111111111111111111111111111111111\nCo-authored-by: {REVIEW}\n"));
        assert_eq!(crate::review::change_id(&amended).as_deref(), Some("I1111111111111111111111111111111111111111"));
        // Once each, and never the author.
        assert_eq!(co(&amended, "sam <sam@kiln>", &[REVIEW, "AGENT:review <agent-review@kiln>"]), amended);
        assert_eq!(co("fix\n", "sam <sam@kiln>", &["sam <sam@kiln>", "", "kim <kim@kiln>", "kim <kim@kiln>"]), "fix\n\nCo-authored-by: kim <kim@kiln>\n");
        assert_eq!(co("fix\n\n", "sam <sam@kiln>", &[]), "fix\n");
    }
}
