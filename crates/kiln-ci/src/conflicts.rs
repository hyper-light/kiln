//! Conflicts in the diff (docs/ci.md W26-R12), the pure part. The server replays a change's latest
//! revision onto its trunk with `git merge-tree --write-tree -z` (with `merge.conflictStyle=zdiff3`,
//! so git's markers carry the base); this reads what git printed ([`merged`]): the tree it wrote,
//! each conflicted path's stages and git's messages; names each conflicted path's kind
//! ([`kind_of`]); and, for a text file, cuts its merged text into what merged cleanly and each
//! conflict where it falls ([`Text`]), then makes the file each resolution gives (taking main, this,
//! both, an agent's resolution or an edit). A binary file, a file one side deleted, and anything
//! else git can't merge by lines is resolved by taking a side: it's named, never shown as markers.

use serde::{Deserialize, Serialize};

use crate::review;

/// One stage of a conflicted path, as git's conflicted-file info gives it: 1 the merge base, 2
/// main's (what the change is replayed onto), 3 this change's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stage {
    pub stage: u8,
    pub mode: String,
    pub oid: String,
}

/// One of git's messages about a merge: the paths it's about, its kind (`CONFLICT (binary)`,
/// `Auto-merging`) and its words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub paths: Vec<String>,
    pub kind: String,
    pub text: String,
}

/// What `git merge-tree --write-tree -z` printed: the tree it wrote (conflict markers in the text
/// files that conflict), each conflicted path with its stages in git's order, and git's messages.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Merged {
    pub tree: String,
    pub paths: Vec<(String, Vec<Stage>)>,
    pub messages: Vec<Message>,
}

impl Merged {
    /// Whether git found `path` binary (a `CONFLICT (binary)` message names it).
    pub fn said_binary(&self, path: &str) -> bool {
        self.messages.iter().any(|m| m.kind.to_ascii_lowercase().contains("binary") && m.paths.iter().any(|p| p == path))
    }

    /// git's words on the conflicts at `path`, in its order.
    pub fn said(&self, path: &str) -> Vec<&str> {
        self.messages.iter().filter(|m| m.kind.starts_with("CONFLICT") && m.paths.iter().any(|p| p == path)).map(|m| m.text.as_str()).collect()
    }
}

fn is_oid(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The most paths one of git's messages names that's believed.
const MAX_MESSAGE_PATHS: usize = 4096;

/// What `git merge-tree --write-tree -z` printed. Its exit status says whether the merge was
/// clean (git's own advice: an empty list of conflicted paths isn't a clean merge); refused,
/// saying why, when the output isn't merge-tree's.
pub fn merged(out: &[u8]) -> Result<Merged, String> {
    let text = String::from_utf8_lossy(out);
    let mut fields = text.split('\0');
    let tree = fields.next().unwrap_or_default().trim().to_owned();
    if !is_oid(&tree) {
        return Err(format!("git merge-tree named no tree: {:?}", tree.chars().take(80).collect::<String>()));
    }
    let mut m = Merged { tree, ..Merged::default() };
    // Each conflicted path's stages (`<mode> <oid> <stage>\t<path>`), until an empty field.
    for f in fields.by_ref() {
        if f.is_empty() {
            break;
        }
        let bad = || format!("git merge-tree's conflicted-file info doesn't read: {:?}", f.chars().take(120).collect::<String>());
        let (head, path) = f.split_once('\t').ok_or_else(bad)?;
        let mut w = head.split(' ');
        let (Some(mode), Some(oid), Some(stage), None) = (w.next(), w.next(), w.next(), w.next()) else { return Err(bad()) };
        let stage: u8 = stage.parse().map_err(|_| bad())?;
        if !(1..=3).contains(&stage) || !is_oid(oid) || mode.is_empty() || path.is_empty() {
            return Err(bad());
        }
        let st = Stage { stage, mode: mode.to_owned(), oid: oid.to_owned() };
        match m.paths.last_mut() {
            Some((p, stages)) if p == path => stages.push(st),
            _ => m.paths.push((path.to_owned(), vec![st])),
        }
    }
    // Messages: how many paths, the paths, the kind, the words.
    while let Some(n) = fields.next() {
        if n.is_empty() {
            break;
        }
        let n: usize = n.parse().map_err(|_| format!("git merge-tree's messages don't read at {:?}", n.chars().take(40).collect::<String>()))?;
        if n > MAX_MESSAGE_PATHS {
            return Err(format!("git merge-tree's message names {n} paths"));
        }
        let mut paths = Vec::with_capacity(n);
        for _ in 0..n {
            paths.push(fields.next().ok_or("git merge-tree's message ends early")?.to_owned());
        }
        let kind = fields.next().ok_or("git merge-tree's message has no kind")?.to_owned();
        let words = fields.next().ok_or("git merge-tree's message has no words")?;
        m.messages.push(Message { paths, kind, text: words.trim_end().to_owned() });
    }
    Ok(m)
}

/// What kind of conflict a path is, which says how it's resolved (W26-R12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Both sides changed the same lines of a text file: each conflict sits where it falls in the
    /// file's merged text, between git's markers, and is resolved there.
    Text,
    /// A binary file both sides changed.
    Binary,
    /// Main changed a file this change deletes.
    ModifyDelete,
    /// Main deleted a file this change changes.
    DeleteModify,
    /// Anything else git can't merge by lines: a symbolic link or a submodule both sides changed, a
    /// rename on one side and a delete on the other, a file where the other side has a directory.
    Other,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Text => "text",
            Kind::Binary => "binary",
            Kind::ModifyDelete => "modify_delete",
            Kind::DeleteModify => "delete_modify",
            Kind::Other => "other",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        Some(match s {
            "text" => Kind::Text,
            "binary" => Kind::Binary,
            "modify_delete" => Kind::ModifyDelete,
            "delete_modify" => Kind::DeleteModify,
            "other" => Kind::Other,
            _ => return None,
        })
    }

    /// Whether it's resolved line by line where it falls (main, this, both, the agent's or an
    /// edit), rather than by taking one side's file.
    pub fn by_lines(self) -> bool {
        self == Kind::Text
    }

    /// What it is, in words, for a conflict resolved by taking a side: what each side did.
    pub fn named(self, file: &str) -> String {
        let f = file.rsplit('/').next().unwrap_or(file);
        match self {
            Kind::Text => format!("{f}: both sides changed the same lines"),
            Kind::Binary => format!("{f}: a binary file both main and this change changed: take main's or this change's"),
            Kind::ModifyDelete => format!("{f}: main changed it and this change deletes it: keep main's, or delete it"),
            Kind::DeleteModify => format!("{f}: main deleted it and this change changes it: delete it, or keep this change's"),
            Kind::Other => format!("{f}: git can't merge it by lines: take main's or this change's"),
        }
    }
}

/// Whether bytes are binary as git judges a blob: a NUL in its first 8000.
pub fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8000).any(|b| *b == 0)
}

/// A conflicted path's kind, from its stages and whether its content is binary (git said so, or a
/// side's bytes are). Only regular files both sides kept can conflict line by line.
pub fn kind_of(stages: &[Stage], binary: bool) -> Kind {
    let has = |n: u8| stages.iter().any(|s| s.stage == n);
    let regular = |n: u8| stages.iter().filter(|s| s.stage == n).all(|s| s.mode.starts_with("100"));
    match (has(1), has(2), has(3)) {
        (_, true, true) if !(regular(2) && regular(3)) => Kind::Other,
        (_, true, true) if binary => Kind::Binary,
        (_, true, true) => Kind::Text,
        (true, true, false) => Kind::ModifyDelete,
        (true, false, true) => Kind::DeleteModify,
        _ => Kind::Other,
    }
}

/// How a conflict is resolved (W26-R12): main's side, this change's, both (main's then this
/// change's), an agent's resolution, or an edit. A conflict that isn't text takes a side (or the
/// agent's choice of one).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Take {
    Main,
    This,
    Both,
    Agent,
    Edit,
}

impl Take {
    pub fn as_str(self) -> &'static str {
        match self {
            Take::Main => "main",
            Take::This => "this",
            Take::Both => "both",
            Take::Agent => "agent",
            Take::Edit => "edit",
        }
    }

    pub fn parse(s: &str) -> Option<Take> {
        Some(match s.trim() {
            "main" | "theirs" => Take::Main,
            "this" | "ours" => Take::This,
            "both" => Take::Both,
            "agent" => Take::Agent,
            "edit" => Take::Edit,
            _ => return None,
        })
    }

    /// What resolves a conflict of `kind`: all five for text, else a side (or the agent's choice).
    pub fn takes(kind: Kind) -> &'static [Take] {
        if kind.by_lines() { &[Take::Main, Take::This, Take::Both, Take::Agent, Take::Edit] } else { &[Take::Main, Take::This, Take::Agent] }
    }
}

/// A conflict in a text file's merged text, as git's markers show it: main's lines, this change's,
/// and the base's where the markers carry them (zdiff3 or diff3). Lines are as the file has them
/// (a CRLF file's keep their `\r`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hunk {
    pub main: Vec<String>,
    pub this: Vec<String>,
    pub base: Option<Vec<String>>,
}

/// Lines as they read, without a CRLF file's `\r`.
pub fn plain(lines: &[String]) -> Vec<String> {
    lines.iter().map(|l| l.strip_suffix('\r').unwrap_or(l).to_owned()).collect()
}

/// A side of a conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Main,
    This,
    Base,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    Clean(Vec<String>),
    Conflict(Hunk),
}

/// A text file as git merged it: what merged cleanly, and each conflict where it falls.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Text {
    pieces: Vec<Piece>,
    ends_with_newline: bool,
    crlf: bool,
}

/// What a line is, among git's conflict markers of `size` characters: `<<<<<<< main`,
/// `||||||| base`, `=======`, `>>>>>>> this`.
fn marker(line: &str, c: char, size: usize, labelled: bool) -> bool {
    let line = line.strip_suffix('\r').unwrap_or(line);
    let n = line.chars().take_while(|x| *x == c).count();
    if n != size {
        return false;
    }
    let rest = line.get(n..).unwrap_or_default();
    if labelled { rest.is_empty() || rest.starts_with(' ') } else { rest.is_empty() }
}

/// The size of the `<<<<<<<` marker a line opens a conflict with (7 unless the file's
/// `conflict-marker-size` says otherwise).
fn opening(line: &str) -> Option<usize> {
    let line = line.strip_suffix('\r').unwrap_or(line);
    let n = line.chars().take_while(|x| *x == '<').count();
    (n >= 7 && marker(line, '<', n, true)).then_some(n)
}

impl Text {
    /// A merged text cut at git's markers; refused when there are none or they don't close (the
    /// path is then resolved by taking a side).
    pub fn parse(merged: &str) -> Result<Text, String> {
        let ends_with_newline = merged.ends_with('\n');
        let mut lines: Vec<&str> = merged.split('\n').collect();
        if ends_with_newline {
            lines.pop();
        }
        let (mut pieces, mut clean) = (Vec::new(), Vec::new());
        let mut crlf = false;
        let mut it = lines.into_iter().enumerate();
        while let Some((at, line)) = it.next() {
            let Some(size) = opening(line) else {
                clean.push(line.to_owned());
                continue;
            };
            crlf |= line.ends_with('\r');
            let mut h = Hunk::default();
            let mut part = Side::Main;
            let mut closed = false;
            for (_, line) in it.by_ref() {
                match part {
                    Side::Main if marker(line, '|', size, true) => {
                        part = Side::Base;
                        h.base = Some(Vec::new());
                    }
                    Side::Main | Side::Base if marker(line, '=', size, false) => part = Side::This,
                    Side::This if marker(line, '>', size, true) => {
                        closed = true;
                        break;
                    }
                    _ if opening(line) == Some(size) => return Err(format!("a conflict opened inside another, at line {}", at + 1)),
                    Side::Main => h.main.push(line.to_owned()),
                    Side::Base => h.base.get_or_insert_with(Vec::new).push(line.to_owned()),
                    Side::This => h.this.push(line.to_owned()),
                }
            }
            if !closed {
                return Err(format!("the conflict at line {} doesn't close", at + 1));
            }
            if !clean.is_empty() {
                pieces.push(Piece::Clean(std::mem::take(&mut clean)));
            }
            pieces.push(Piece::Conflict(h));
        }
        if !clean.is_empty() {
            pieces.push(Piece::Clean(clean));
        }
        if !pieces.iter().any(|p| matches!(p, Piece::Conflict(_))) {
            return Err("no conflict markers".into());
        }
        Ok(Text { pieces, ends_with_newline, crlf })
    }

    /// Its conflicts, in order.
    pub fn hunks(&self) -> Vec<&Hunk> {
        self.pieces
            .iter()
            .filter_map(|p| match p {
                Piece::Conflict(h) => Some(h),
                Piece::Clean(_) => None,
            })
            .collect()
    }

    /// Whether its last lines are a conflict's (nothing merged cleanly after it).
    pub fn ends_in_conflict(&self) -> bool {
        matches!(self.pieces.last(), Some(Piece::Conflict(_)))
    }

    fn join(&self, lines: Vec<&str>) -> String {
        let mut out = lines.join("\n");
        if self.ends_with_newline && !lines.is_empty() {
            out.push('\n');
        }
        out
    }

    /// The text with every conflict taking `side` (the base's: none where the markers carry no
    /// base), and where each conflict's lines are in it: its first line (from 1) and how many.
    pub fn with_side(&self, side: Side) -> (String, Vec<(u32, u32)>) {
        let (mut lines, mut at) = (Vec::new(), Vec::new());
        for p in &self.pieces {
            match p {
                Piece::Clean(c) => lines.extend(c.iter().map(String::as_str)),
                Piece::Conflict(h) => {
                    let mine: &[String] = match side {
                        Side::Main => &h.main,
                        Side::This => &h.this,
                        Side::Base => h.base.as_deref().unwrap_or_default(),
                    };
                    at.push((u32::try_from(lines.len() + 1).unwrap_or(u32::MAX), u32::try_from(mine.len()).unwrap_or(u32::MAX)));
                    lines.extend(mine.iter().map(String::as_str));
                }
            }
        }
        (self.join(lines), at)
    }

    /// The lines conflict `index` resolves to by `take`: main's, this change's, both (main's then
    /// this change's), or `given` (an agent's resolution, an edit), given lines taking the file's
    /// line endings.
    pub fn pick(&self, index: usize, take: Take, given: Option<&[String]>) -> Result<Vec<String>, String> {
        let h = self.hunks().get(index).copied().ok_or_else(|| format!("no conflict {}", index + 1))?;
        Ok(match take {
            Take::Main => h.main.clone(),
            Take::This => h.this.clone(),
            Take::Both => h.main.iter().chain(&h.this).cloned().collect(),
            Take::Agent | Take::Edit => {
                let given = given.ok_or_else(|| format!("{} needs its lines", take.as_str()))?;
                given.iter().map(|l| if self.crlf && !l.ends_with('\r') { format!("{l}\r") } else { l.clone() }).collect()
            }
        })
    }

    /// The file once each conflict is resolved: `picks` its lines, in order, one for each.
    pub fn resolved(&self, picks: &[Vec<String>]) -> Result<String, String> {
        let n = self.hunks().len();
        if picks.len() != n {
            return Err(format!("{} resolutions for {n} conflicts", picks.len()));
        }
        let mut picks = picks.iter();
        let mut lines = Vec::new();
        for p in &self.pieces {
            match p {
                Piece::Clean(c) => lines.extend(c.iter().map(String::as_str)),
                Piece::Conflict(_) => lines.extend(picks.next().into_iter().flatten().map(String::as_str)),
            }
        }
        Ok(self.join(lines))
    }
}

/// Where a conflict's lines are in its side's own file: `start` (from 1) and `len` lines in the
/// text taking that side ([`Text::with_side`]), carried by `hunks` (`git diff -U0` from that text to
/// the side's file, which differ only where the other side's changes merged cleanly). A side with
/// no lines of its own falls after the line before it. `None` when a change runs through them.
pub fn place(hunks: &[review::Hunk], start: u32, len: u32) -> Option<u32> {
    if len == 0 {
        if start <= 1 {
            return Some(1);
        }
        return review::follow(hunks, start - 1, start - 1).map(|(s, _)| s + 1);
    }
    review::follow(hunks, start, start + len - 1).map(|(s, _)| s)
}

/// Whether an edit still holds conflict markers (an opening or a closing one): it isn't a
/// resolution yet.
pub fn has_markers(lines: &[String]) -> bool {
    let closing = |l: &str| {
        let n = l.chars().take_while(|c| *c == '>').count();
        n >= 7 && marker(l, '>', n, true)
    };
    lines.iter().any(|l| opening(l).is_some() || closing(l))
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: &str = "5bb67d809fd151fd7b0e90ce1c4737ae59a6be7b";

    fn lines(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|x| (*x).to_owned()).collect()
    }

    /// git 2.56's output for a binary file, a text file and a modify/delete (main changed it, this
    /// change deleted it), `-z`.
    fn three() -> Vec<u8> {
        let mut out = format!("{T}\0").into_bytes();
        for (mode, oid, stage, path) in [
            ("100644", "88768efdf77ec78c9a995f94881793be6a41752b", 1, "b.bin"),
            ("100644", "e73ddff3b53b01da79aaa663b0873ef58e52aa26", 2, "b.bin"),
            ("100644", "768fa1c12959d5c7780b14e41075d377dd9b4f0d", 3, "b.bin"),
            ("100644", "f9d9a0195c5b9c01ef64e2a69d8b9a624f42b8c8", 1, "f.txt"),
            ("100644", "5f41f6e026e6ffbdf76dac840789d3591a427117", 2, "f.txt"),
            ("100644", "905053ff8d31adca86549f11469f2e3dbab8bc5e", 3, "f.txt"),
            ("100644", "2fa992c0b8b5c6acd2bdd4fa31de29d29799bdd5", 1, "gone.txt"),
            ("100644", "82952c55bb3028247347e3e7a3b910da45859fe6", 2, "gone.txt"),
        ] {
            out.extend(format!("{mode} {oid} {stage}\t{path}\0").into_bytes());
        }
        out.push(0);
        for (path, kind, words) in [
            ("b.bin", "CONFLICT (binary)", "warning: Cannot merge binary files: b.bin (master vs. this)\n"),
            ("b.bin", "Auto-merging", "Auto-merging b.bin\n"),
            ("b.bin", "CONFLICT (contents)", "CONFLICT (content): Merge conflict in b.bin\n"),
            ("f.txt", "Auto-merging", "Auto-merging f.txt\n"),
            ("f.txt", "CONFLICT (contents)", "CONFLICT (content): Merge conflict in f.txt\n"),
            ("gone.txt", "CONFLICT (modify/delete)", "CONFLICT (modify/delete): gone.txt deleted in this and modified in master.  Version master of gone.txt left in tree.\n"),
        ] {
            out.extend(format!("1\0{path}\0{kind}\0{words}\0").into_bytes());
        }
        out
    }

    #[test]
    fn w26_r12_reads_git_merge_trees_output() -> Result<(), String> {
        let m = merged(&three())?;
        assert_eq!(m.tree, T);
        assert_eq!(m.paths.iter().map(|(p, s)| (p.as_str(), s.iter().map(|x| x.stage).collect::<Vec<_>>())).collect::<Vec<_>>(), [("b.bin", vec![1, 2, 3]), ("f.txt", vec![1, 2, 3]), ("gone.txt", vec![1, 2])]);
        assert!(m.said_binary("b.bin") && !m.said_binary("f.txt"));
        assert_eq!(m.said("gone.txt"), ["CONFLICT (modify/delete): gone.txt deleted in this and modified in master.  Version master of gone.txt left in tree."]);
        assert_eq!(m.messages.len(), 6);
        // A clean merge is its tree alone; what isn't merge-tree's is refused.
        assert_eq!(merged(format!("{T}\0").as_bytes())?, Merged { tree: T.into(), ..Merged::default() });
        assert!(merged(b"fatal: not a git repository\n").is_err());
        assert!(merged(format!("{T}\0100644 xyz 2\tf.txt\0").as_bytes()).is_err(), "a stage's blob is an oid");
        assert!(merged(format!("{T}\0100644 {T} 4\tf.txt\0").as_bytes()).is_err(), "stages are 1 to 3");
        assert!(merged(format!("{T}\0\x002\0a\0").as_bytes()).is_err(), "a message ending early");
        // A path with a tab in it keeps it.
        let odd = merged(format!("{T}\0100644 {T} 2\tdir/a\tb.txt\0").as_bytes())?;
        assert_eq!(odd.paths.first().map(|(p, _)| p.as_str()), Some("dir/a\tb.txt"));
        Ok(())
    }

    #[test]
    fn w26_r12_each_kind_from_its_stages() {
        let st = |n: u8, mode: &str| Stage { stage: n, mode: mode.into(), oid: T.into() };
        let file = |ns: &[u8]| ns.iter().map(|n| st(*n, "100644")).collect::<Vec<_>>();
        assert_eq!(kind_of(&file(&[1, 2, 3]), false), Kind::Text);
        assert_eq!(kind_of(&file(&[2, 3]), false), Kind::Text, "both added it");
        assert_eq!(kind_of(&file(&[1, 2, 3]), true), Kind::Binary);
        assert_eq!(kind_of(&file(&[1, 2]), false), Kind::ModifyDelete);
        assert_eq!(kind_of(&file(&[1, 3]), true), Kind::DeleteModify, "a deleted side wins over binary: it's a choice either way");
        assert_eq!(kind_of(&file(&[3]), false), Kind::Other);
        assert_eq!(kind_of(&[st(1, "100644"), st(2, "120000"), st(3, "100644")], false), Kind::Other, "a symbolic link");
        assert_eq!(kind_of(&[st(2, "160000"), st(3, "160000")], false), Kind::Other, "a submodule");
        assert_eq!(kind_of(&[st(1, "100644"), st(2, "100755"), st(3, "100644")], false), Kind::Text, "an executable file is text");
        assert!(looks_binary(b"\x00\x01bin") && !looks_binary(b"text\n"));
        for k in [Kind::Text, Kind::Binary, Kind::ModifyDelete, Kind::DeleteModify, Kind::Other] {
            assert_eq!(Kind::parse(k.as_str()), Some(k));
        }
        assert_eq!(Take::takes(Kind::Binary), [Take::Main, Take::This, Take::Agent], "a binary file takes a side");
        assert_eq!(Take::takes(Kind::Text).len(), 5);
        assert_eq!(Kind::ModifyDelete.named("a/gone.txt"), "gone.txt: main changed it and this change deletes it: keep main's, or delete it");
    }

    /// D41 replayed onto main, as git 2.56 writes it with `merge.conflictStyle=zdiff3`.
    const MERGED: &str = "fn expand() {\n<<<<<<< main\n    let m = 1;\n    let n = 2;\n||||||| 5d0e7b2\n    let m = 0;\n=======\n    let m = 3;\n>>>>>>> D41\n    go();\n<<<<<<< main\nmain end\n||||||| 5d0e7b2\nbase end\n=======\nthis end\n>>>>>>> D41\n";

    #[test]
    fn w26_r12_conflicts_where_they_fall() -> Result<(), String> {
        let t = Text::parse(MERGED)?;
        let h = t.hunks();
        assert_eq!(h.len(), 2);
        assert_eq!(h.first().copied(), Some(&Hunk { main: lines(&["    let m = 1;", "    let n = 2;"]), this: lines(&["    let m = 3;"]), base: Some(lines(&["    let m = 0;"])) }));
        assert!(t.ends_in_conflict());
        // Where each conflict's lines are with every conflict taking a side.
        let (main, at) = t.with_side(Side::Main);
        assert_eq!((main.as_str(), at), ("fn expand() {\n    let m = 1;\n    let n = 2;\n    go();\nmain end\n", vec![(2, 2), (5, 1)]));
        let (this, at) = t.with_side(Side::This);
        assert_eq!((this.as_str(), at), ("fn expand() {\n    let m = 3;\n    go();\nthis end\n", vec![(2, 1), (4, 1)]));
        // Each of the five ways.
        let edit = lines(&["    let m = n.max(1);"]);
        let picks = [t.pick(0, Take::Both, None)?, t.pick(1, Take::Edit, Some(edit.as_slice()))?];
        assert_eq!(t.resolved(&picks)?, "fn expand() {\n    let m = 1;\n    let n = 2;\n    let m = 3;\n    go();\n    let m = n.max(1);\n");
        assert_eq!(t.resolved(&[t.pick(0, Take::Main, None)?, t.pick(1, Take::This, None)?])?, "fn expand() {\n    let m = 1;\n    let n = 2;\n    go();\nthis end\n");
        let none: Vec<String> = Vec::new();
        assert_eq!(t.resolved(&[t.pick(0, Take::Agent, Some(none.as_slice()))?, Vec::new()])?, "fn expand() {\n    go();\n", "a resolution may be no lines");
        assert!(t.pick(0, Take::Edit, None).is_err() && t.pick(2, Take::Main, None).is_err());
        assert!(t.resolved(&[Vec::new()]).is_err(), "one resolution for each conflict");
        Ok(())
    }

    #[test]
    fn w26_r12_markers_as_git_writes_them() -> Result<(), String> {
        // Without a base (merge style), with an empty base (both added it), CRLF, a wider marker.
        let t = Text::parse("a\n<<<<<<< HEAD\nx\n=======\ny\n>>>>>>> D1\nb\n")?;
        assert_eq!(t.hunks().first().map(|h| h.base.clone()), Some(None));
        let added = Text::parse("<<<<<<< main\nnew main\n||||||| 4335e92\n=======\nnew this\n>>>>>>> this\n")?;
        assert_eq!(added.hunks().first().map(|h| h.base.clone()), Some(Some(Vec::new())));
        let crlf = Text::parse("a\r\n<<<<<<< master\r\nMAIN\r\n||||||| master~1\r\nb\r\n=======\r\nTHIS\r\n>>>>>>> this\r\nc\r\n")?;
        assert_eq!(crlf.hunks().first().map(|h| plain(&h.this)), Some(lines(&["THIS"])));
        let edit = lines(&["EDIT"]);
        assert_eq!(crlf.resolved(&[crlf.pick(0, Take::Edit, Some(edit.as_slice()))?])?, "a\r\nEDIT\r\nc\r\n", "an edit takes the file's line endings");
        assert_eq!(crlf.resolved(&[crlf.pick(0, Take::This, None)?])?, "a\r\nTHIS\r\nc\r\n");
        let wide = Text::parse("<<<<<<<<< main\n=======\n<<<<<<< not a marker\n=========\nthis\n>>>>>>>>> D1\n")?;
        assert_eq!(wide.hunks().first().map(|h| (h.main.clone(), h.this.clone())), Some((lines(&["=======", "<<<<<<< not a marker"]), lines(&["this"]))), "a wider marker's file keeps 7-character ones as lines");
        // Refused: no markers, one that doesn't close, one opened inside another.
        assert!(Text::parse("plain\n").is_err());
        assert!(Text::parse("<<<<<<< main\nx\n=======\n").is_err());
        assert!(Text::parse("<<<<<<< main\n<<<<<<< again\n=======\n>>>>>>> x\n").is_err());
        // `<<<<<<<x` isn't a marker; a file without a final newline keeps none.
        assert!(Text::parse("<<<<<<<x\n").is_err());
        let open = Text::parse("<<<<<<< main\nm\n=======\nt\n>>>>>>> D1\nend")?;
        assert_eq!(open.resolved(&[lines(&["r"])])?, "r\nend");
        Ok(())
    }

    #[test]
    fn w26_r12_places_in_each_sides_file() {
        // The text taking this side has main's three new lines at the top that this change's file
        // doesn't: its conflict at line 5 is at line 2 there.
        let h = review::hunks("@@ -1,3 +0,0 @@\n-a\n-b\n-c\n");
        assert_eq!(place(&h, 5, 2), Some(2));
        assert_eq!(place(&h, 5, 0), Some(2), "no lines of its own: after the line before it");
        assert_eq!(place(&[], 1, 0), Some(1));
        assert_eq!(place(&h, 2, 1), None, "a change through it");
    }

    #[test]
    fn w26_r12_markers_left_in_an_edit() {
        assert!(has_markers(&lines(&["x", "<<<<<<< main"])) && has_markers(&lines(&[">>>>>>> D1"])));
        assert!(!has_markers(&lines(&["=======", "a <<<<<<< b", "<<<<<<<<x"])), "a rule or markers mid-line aren't markers");
    }
}
