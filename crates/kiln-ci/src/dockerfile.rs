//! Dockerfiles, as kiln reads and checks them (docs/ci.md W13-R11, W21-R01): the instructions,
//! with their line continuations, heredocs (`RUN <<EOF`) and parser directives (`# escape=`),
//! then lints under hadolint's codes, which people know, for the mistakes that cost: an untagged
//! or `latest` base, `apt-get` without `-y`, `cd` in `RUN`, `ADD` for local files, a second `CMD`,
//! root as the last user, and the rest below. Structure errors are kiln's own (`KDF1xxx`).

use crate::wf::diag::{Diag, Pos, Range, suggest};

/// Every instruction Docker knows.
pub const INSTRUCTIONS: &[&str] = &["FROM", "RUN", "CMD", "LABEL", "MAINTAINER", "EXPOSE", "ENV", "ADD", "COPY", "ENTRYPOINT", "VOLUME", "USER", "WORKDIR", "ARG", "ONBUILD", "STOPSIGNAL", "HEALTHCHECK", "SHELL"];

/// One instruction: its keyword (upper case), its arguments with continuations joined (each
/// continued line's text after a space) and heredocs' bodies kept apart, and where it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instruction {
    pub keyword: String,
    pub args: String,
    /// Its heredocs' bodies, in order.
    pub heredocs: Vec<String>,
    /// Lines, from 1.
    pub line: u32,
    pub end_line: u32,
}

impl Instruction {
    /// The arguments as an exec form's list (`["a", "b"]`), when they are one.
    pub fn exec_form(&self) -> Option<Vec<String>> {
        let t = self.args.trim();
        if !t.starts_with('[') {
            return None;
        }
        serde_json::from_str(t).ok()
    }

    fn range(&self) -> Range {
        Range { start: Pos { line: self.line, col: 1, offset: 0 }, end: Pos { line: self.end_line, col: 1, offset: 0 } }
    }
}

/// A Dockerfile read: its instructions, and what's wrong with its shape.
#[derive(Debug, Clone, Default)]
pub struct Parsed {
    pub instructions: Vec<Instruction>,
    pub diagnostics: Vec<Diag>,
}

fn at(line: u32) -> Range {
    Range { start: Pos { line, col: 1, offset: 0 }, end: Pos { line, col: 1, offset: 0 } }
}

/// A heredoc's opener in an instruction's text: `<<EOF`, `<<-EOF`, `<<"EOF"`, `<<'EOF'`: its word
/// and whether its body's leading tabs go.
fn heredocs_in(text: &str) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find("<<") {
        let after = rest.get(i + 2..).unwrap_or_default();
        let (strip, after) = match after.strip_prefix('-') {
            Some(a) => (true, a),
            None => (false, after),
        };
        let quoted = after.starts_with(['"', '\'']);
        let word: String = after.trim_start_matches(['"', '\'']).chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
        if !word.is_empty() && (quoted || word.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')) {
            out.push((word.clone(), strip));
        }
        rest = after.get(word.len()..).unwrap_or_default();
    }
    out
}

/// Reads `src`.
pub fn parse(src: &str) -> Parsed {
    let lines: Vec<&str> = src.lines().collect();
    let mut p = Parsed::default();
    let mut escape = '\\';
    // Parser directives: `# name=value` lines at the very top.
    let mut i = 0usize;
    while let Some(l) = lines.get(i) {
        let Some(body) = l.trim_start().strip_prefix('#') else { break };
        let Some((k, v)) = body.split_once('=') else { break };
        let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
        if k.is_empty() || k.contains(char::is_whitespace) {
            break;
        }
        if k == "escape" {
            match v {
                "\\" => escape = '\\',
                "`" => escape = '`',
                other => p.diagnostics.push(Diag::error("KDF1007", format!("`# escape={other}`: the escape character is \\ or `"), at(u32::try_from(i + 1).unwrap_or(u32::MAX)))),
            }
        }
        i += 1;
    }
    while i < lines.len() {
        let start = i;
        let Some(first) = lines.get(i) else { break };
        let trimmed = first.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            i += 1;
            continue;
        }
        // The instruction's text: continued lines joined, comment lines inside it dropped.
        let mut text = String::new();
        let mut line = (*first).to_owned();
        loop {
            let continued = line.trim_end().ends_with(escape);
            let part = if continued { line.trim_end().strip_suffix(escape).unwrap_or_default().to_owned() } else { line.clone() };
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(part.trim_start());
            if !continued {
                break;
            }
            i += 1;
            // Comment lines inside a continuation are dropped; an empty one is, too.
            while lines.get(i).is_some_and(|l| l.trim_start().starts_with('#')) {
                i += 1;
            }
            match lines.get(i) {
                Some(next) => line = (*next).to_owned(),
                None => {
                    p.diagnostics.push(Diag::error("KDF1004", "the last line continues (ends with the escape character) into nothing", at(u32::try_from(i).unwrap_or(u32::MAX))));
                    break;
                }
            }
        }
        let (keyword, args) = match text.trim().split_once(char::is_whitespace) {
            Some((k, a)) => (k.to_ascii_uppercase(), a.trim().to_owned()),
            None => (text.trim().to_ascii_uppercase(), String::new()),
        };
        // Heredocs: their bodies follow the instruction, each up to its word.
        let mut heredocs = Vec::new();
        if matches!(keyword.as_str(), "RUN" | "COPY" | "ADD") {
            for (word, strip) in heredocs_in(&args) {
                let mut body = String::new();
                let mut closed = false;
                i += 1;
                while let Some(l) = lines.get(i) {
                    let probe = if strip { l.trim_start_matches('\t') } else { l };
                    if probe == word {
                        closed = true;
                        break;
                    }
                    body.push_str(probe);
                    body.push('\n');
                    i += 1;
                }
                if !closed {
                    p.diagnostics.push(Diag::error("KDF1008", format!("the heredoc `<<{word}` never ends: no line `{word}`"), at(u32::try_from(start + 1).unwrap_or(u32::MAX))));
                }
                heredocs.push(body);
            }
        }
        let ins = Instruction { keyword, args, heredocs, line: u32::try_from(start + 1).unwrap_or(u32::MAX), end_line: u32::try_from(i.min(lines.len().saturating_sub(1)) + 1).unwrap_or(u32::MAX) };
        if !INSTRUCTIONS.contains(&ins.keyword.as_str()) {
            let hint = suggest(&ins.keyword, INSTRUCTIONS.iter().copied()).map(|s| format!(" (did you mean {s}?)")).unwrap_or_default();
            p.diagnostics.push(Diag::error("KDF1001", format!("`{}` isn't a Dockerfile instruction{hint}", ins.keyword), ins.range()));
        } else if ins.args.is_empty() && ins.heredocs.is_empty() {
            p.diagnostics.push(Diag::error("KDF1003", format!("{} needs arguments", ins.keyword), ins.range()));
        } else if matches!(ins.keyword.as_str(), "CMD" | "ENTRYPOINT" | "SHELL") && ins.args.trim_start().starts_with('[') && ins.exec_form().is_none() {
            // (A `RUN [ -f x ]` is a shell test, not a list.)
            p.diagnostics.push(Diag::error("KDF1005", format!("{}'s exec form isn't a JSON list of strings (`[\"a\", \"b\"]`, double quotes)", ins.keyword), ins.range()));
        }
        p.instructions.push(ins);
        i += 1;
    }
    // FROM first: only ARGs may come before it.
    match p.instructions.iter().find(|x| x.keyword != "ARG") {
        None => p.diagnostics.push(Diag::error("KDF1006", "a Dockerfile needs a FROM", at(1))),
        Some(x) if x.keyword != "FROM" => p.diagnostics.push(Diag::error("KDF1002", format!("{} comes before any FROM: only ARG may", x.keyword), x.range())),
        Some(_) => {}
    }
    p
}

/// The words of a shell command, split at `&&`, `||`, `;` and `|`, each command's words apart.
fn commands(script: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    for part in script.split(['\n', ';', '|', '&']) {
        let words: Vec<String> = part.split_whitespace().map(str::to_owned).collect();
        if !words.is_empty() {
            out.push(words);
        }
    }
    out
}

/// What a `RUN` runs: its arguments (shell or exec form, after its `--mount`-like flags) and its
/// heredocs.
fn script(x: &Instruction) -> String {
    let mut s = match x.exec_form() {
        Some(list) => list.join(" "),
        None => x.args.split_whitespace().skip_while(|w| w.starts_with("--")).collect::<Vec<_>>().join(" "),
    };
    for h in &x.heredocs {
        s.push('\n');
        s.push_str(h);
    }
    s
}

/// The lints on `p`'s instructions.
fn lint(p: &Parsed) -> Vec<Diag> {
    let mut d = Vec::new();
    let mut stages: Vec<String> = Vec::new();
    // Per stage: its CMDs, ENTRYPOINTs, HEALTHCHECKs and last USER.
    let mut counts: [(usize, &str); 3] = [(0, "CMD"), (0, "ENTRYPOINT"), (0, "HEALTHCHECK")];
    let mut last_user: Option<(String, Range)> = None;
    let end_stage = |d: &mut Vec<Diag>, last_user: &Option<(String, Range)>, is_last: bool| {
        if let (true, Some((u, r))) = (is_last, last_user)
            && (u == "root" || u == "0" || u.starts_with("root:") || u.starts_with("0:"))
        {
            d.push(Diag::warning("DL3002", "the last USER is root: run as someone else", *r));
        }
    };
    let froms = p.instructions.iter().filter(|x| x.keyword == "FROM").count();
    let mut seen_froms = 0usize;
    for x in &p.instructions {
        let r = x.range();
        // An instruction without arguments is a structure error already.
        if x.args.trim().is_empty() && x.heredocs.is_empty() {
            continue;
        }
        match x.keyword.as_str() {
            "FROM" => {
                if seen_froms > 0 {
                    end_stage(&mut d, &last_user, false);
                }
                seen_froms += 1;
                counts = [(0, "CMD"), (0, "ENTRYPOINT"), (0, "HEALTHCHECK")];
                last_user = None;
                let words: Vec<&str> = x.args.split_whitespace().filter(|w| !w.starts_with("--")).collect();
                let image = words.first().copied().unwrap_or_default();
                if let (Some(a), Some(name)) = (words.get(1), words.get(2))
                    && a.eq_ignore_ascii_case("as")
                {
                    if stages.iter().any(|s| s.eq_ignore_ascii_case(name)) {
                        d.push(Diag::error("DL3024", format!("the stage name `{name}` is used twice"), r));
                    }
                    stages.push((*name).to_owned());
                }
                let named_stage = stages.iter().any(|s| s.eq_ignore_ascii_case(image));
                // A digest pins it; a tag of `latest` doesn't.
                if !named_stage && image != "scratch" && !image.contains('$') && !image.contains('@') {
                    match image.rsplit('/').next().unwrap_or(image).split_once(':') {
                        Some((_, "latest")) => d.push(Diag::warning("DL3007", format!("`{image}` moves: pin a version (`latest` is whatever was pushed last)"), r)),
                        Some(_) => {}
                        None => d.push(Diag::warning("DL3006", format!("`{image}` has no tag: pin a version"), r)),
                    }
                }
            }
            "MAINTAINER" => d.push(Diag::error("DL4000", "MAINTAINER is deprecated: use `LABEL maintainer=\"…\"`", r)),
            "CMD" | "ENTRYPOINT" | "HEALTHCHECK" => {
                if let Some(c) = counts.iter_mut().find(|(_, k)| *k == x.keyword) {
                    c.0 += 1;
                    if c.0 == 2 {
                        let code = match x.keyword.as_str() {
                            "CMD" => "DL4003",
                            "ENTRYPOINT" => "DL4004",
                            _ => "DL3012",
                        };
                        d.push(Diag::warning(code, format!("a second {} in one stage: only the last one counts", x.keyword), r));
                    }
                }
                if x.keyword != "HEALTHCHECK" && x.exec_form().is_none() {
                    d.push(Diag::warning("DL3025", format!("{} in shell form: use the exec form (`[\"cmd\", \"arg\"]`), so signals reach the process", x.keyword), r));
                }
            }
            "USER" => last_user = Some((x.args.trim().to_owned(), r)),
            "WORKDIR" => {
                let dir = x.args.trim().trim_matches('"');
                if !dir.starts_with('/') && !dir.starts_with('$') && !dir.contains(":\\") {
                    d.push(Diag::error("DL3000", format!("WORKDIR `{dir}` is relative: use an absolute path"), r));
                }
            }
            "ADD" => {
                let srcs: Vec<&str> = x.args.split_whitespace().filter(|w| !w.starts_with("--")).collect();
                let sources = srcs.get(..srcs.len().saturating_sub(1)).unwrap_or_default();
                let fetched_or_unpacked = sources.iter().any(|s| s.contains("://") || s.starts_with("git@") || [".tar", ".tar.gz", ".tgz", ".tar.bz2", ".tbz2", ".tar.xz", ".txz", ".tar.zst"].iter().any(|e| s.ends_with(e)));
                if !sources.is_empty() && !fetched_or_unpacked && x.heredocs.is_empty() {
                    d.push(Diag::error("DL3020", "ADD copies local files: use COPY (ADD is for URLs and archives it unpacks)", r));
                }
            }
            "COPY" => {
                let words: Vec<&str> = x.args.split_whitespace().filter(|w| !w.starts_with("--")).collect();
                if x.exec_form().is_none() && x.heredocs.is_empty() && words.len() > 2 && !words.last().is_some_and(|w| w.ends_with('/') || w.contains('$')) {
                    d.push(Diag::error("DL3021", "COPY with more than one source needs a destination that ends with /", r));
                }
            }
            "EXPOSE" => {
                for w in x.args.split_whitespace() {
                    let port = w.split('/').next().unwrap_or(w);
                    let ok = port.contains('$') || port.split('-').all(|n| n.parse::<u32>().is_ok_and(|n| n <= 65_535));
                    if !ok {
                        d.push(Diag::error("DL3011", format!("`{w}` isn't a port (0 to 65535)"), r));
                    }
                }
            }
            "RUN" => {
                let s = script(x);
                for words in commands(&s) {
                    let (cmd, rest) = match words.split_first() {
                        Some((c, rest)) => (c.as_str(), rest),
                        None => continue,
                    };
                    // A flag, long or among short ones (`-yq`).
                    let flag = |long: &[&str], short: char| rest.iter().any(|w| long.contains(&w.as_str()) || (w.starts_with('-') && !w.starts_with("--") && w.contains(short)));
                    match cmd {
                        "cd" => d.push(Diag::warning("DL3003", "`cd` in RUN: use WORKDIR to change directory", r)),
                        "sudo" => d.push(Diag::error("DL3004", "`sudo` in RUN: the build runs as root already (or say USER)", r)),
                        "apt" if rest.first().is_some_and(|w| w == "install" || w == "update") => d.push(Diag::warning("DL3027", "`apt` is for people: use `apt-get` (or `apt-cache`) in scripts", r)),
                        "apt-get" if rest.iter().any(|w| w == "install") => {
                            // (`-qq` says yes too.)
                            if !flag(&["--yes", "--assume-yes", "-qq"], 'y') {
                                d.push(Diag::warning("DL3014", "`apt-get install` without `-y` waits for an answer that never comes", r));
                            }
                            if !rest.iter().any(|w| w == "--no-install-recommends") {
                                d.push(Diag::info("DL3015", "`apt-get install` without `--no-install-recommends` pulls in packages nobody asked for", r));
                            }
                            if !s.contains("/var/lib/apt/lists") && !x.args.contains("type=cache") {
                                d.push(Diag::info("DL3009", "the apt lists stay in the image: `rm -rf /var/lib/apt/lists/*` in the same RUN", r));
                            }
                        }
                        "pip" | "pip3" if rest.iter().any(|w| w == "install") && !rest.iter().any(|w| w == "--no-cache-dir") && !x.args.contains("type=cache") => {
                            d.push(Diag::warning("DL3042", "`pip install` keeps its cache in the image: add `--no-cache-dir`", r));
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    if froms > 0 {
        end_stage(&mut d, &last_user, true);
    }
    d
}

/// `src` read and linted: what's wrong with its shape, then what to change, in order.
pub fn check(src: &str) -> Vec<Diag> {
    let p = parse(src);
    let mut out = p.diagnostics.clone();
    out.extend(lint(&p));
    out.sort_by_key(|d| (d.range.start.line, d.code));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codes(src: &str) -> Vec<&'static str> {
        check(src).iter().map(|d| d.code).collect()
    }

    #[test]
    fn w13_r11_reads_instructions() {
        let p = parse("# syntax=docker/dockerfile:1\n# escape=\\\nARG BASE=ubuntu:24.04\nFROM $BASE AS build\n# a comment\nRUN apt-get update \\\n    # inside\n    && apt-get install -y --no-install-recommends curl \\\n    && rm -rf /var/lib/apt/lists/*\nCOPY <<EOF /etc/motd\nhello\nEOF\nCMD [\"bash\"]\n");
        let kw: Vec<&str> = p.instructions.iter().map(|x| x.keyword.as_str()).collect();
        assert_eq!(kw, ["ARG", "FROM", "RUN", "COPY", "CMD"], "{p:?}");
        let run = p.instructions.get(2).cloned().unwrap_or_else(|| Instruction { keyword: String::new(), args: String::new(), heredocs: Vec::new(), line: 0, end_line: 0 });
        assert_eq!((run.line, run.end_line), (6, 9));
        assert!(run.args.contains("apt-get install -y") && !run.args.contains("inside"), "{}", run.args);
        assert_eq!(p.instructions.get(3).map(|x| x.heredocs.clone()), Some(vec!["hello\n".to_owned()]));
        assert_eq!(p.instructions.get(4).and_then(Instruction::exec_form), Some(vec!["bash".to_owned()]));
        assert!(p.diagnostics.is_empty(), "{:?}", p.diagnostics);
    }

    #[test]
    fn w13_r11_structure_errors() {
        assert_eq!(codes("RUN echo\nFROM alpine:3.20\n"), ["KDF1002"]);
        assert_eq!(codes("FROM alpine:3.20\nRUNN echo\n"), ["KDF1001"]);
        assert!(check("FROM alpine:3.20\nRUNN echo\n").first().is_some_and(|d| d.message.contains("did you mean RUN")));
        assert_eq!(codes("FROM alpine:3.20\nRUN echo \\\n"), ["KDF1004"]);
        assert_eq!(codes("FROM alpine:3.20\nCMD ['sh']\n"), ["DL3025", "KDF1005"]);
        assert!(codes("FROM alpine:3.20\nRUN [ -f /etc/os-release ] && echo ok\n").is_empty(), "a shell test, not a list");
        assert_eq!(codes("FROM alpine:3.20\nRUN <<EOF\necho\n"), ["KDF1008"]);
        assert_eq!(codes("# just a comment\n"), ["KDF1006"]);
        assert_eq!(codes("FROM alpine:3.20\nWORKDIR\n"), ["KDF1003"]);
        // The backtick escape, as on Windows.
        assert!(codes("# escape=`\nFROM mcr.microsoft.com/windows/servercore:ltsc2022\nRUN dir `\n  C:\\\n").is_empty());
    }

    #[test]
    fn w13_r11_lints() {
        assert_eq!(codes("FROM ubuntu\n"), ["DL3006"]);
        assert_eq!(codes("FROM ubuntu:latest\n"), ["DL3007"]);
        assert!(codes("FROM ubuntu@sha256:abc\nFROM scratch\nFROM ubuntu:24.04 AS a\nFROM a\n").is_empty(), "digests, scratch and stages are fine");
        assert_eq!(codes("FROM alpine:3.20 AS a\nFROM alpine:3.20 AS a\n"), ["DL3024"]);
        assert_eq!(codes("FROM ubuntu:24.04\nRUN apt-get update && apt-get install curl\n"), ["DL3009", "DL3014", "DL3015"]);
        assert_eq!(codes("FROM ubuntu:24.04\nRUN cd /src && make\n"), ["DL3003"]);
        assert_eq!(codes("FROM ubuntu:24.04\nRUN sudo make install\n"), ["DL3004"]);
        assert_eq!(codes("FROM ubuntu:24.04\nRUN apt install -y curl\n"), ["DL3027"]);
        assert_eq!(codes("FROM python:3.12\nRUN pip install requests\n"), ["DL3042"]);
        assert!(codes("FROM python:3.12\nRUN --mount=type=cache,target=/root/.cache pip install requests\n").is_empty(), "a cache mount is fine");
        assert_eq!(codes("FROM alpine:3.20\nADD app.py /app/\n"), ["DL3020"]);
        assert!(codes("FROM alpine:3.20\nADD https://example.com/a.tar.gz /opt/\nADD rootfs.tar.xz /\n").is_empty());
        assert_eq!(codes("FROM alpine:3.20\nCOPY a b /dest\n"), ["DL3021"]);
        assert_eq!(codes("FROM alpine:3.20\nWORKDIR app\n"), ["DL3000"]);
        assert_eq!(codes("FROM alpine:3.20\nEXPOSE 80 70000\n"), ["DL3011"]);
        assert_eq!(codes("FROM alpine:3.20\nCMD [\"a\"]\nCMD [\"b\"]\n"), ["DL4003"]);
        assert_eq!(codes("FROM alpine:3.20\nENTRYPOINT sh\n"), ["DL3025"]);
        assert_eq!(codes("FROM alpine:3.20\nMAINTAINER ada\n"), ["DL4000"]);
        assert_eq!(codes("FROM alpine:3.20\nUSER app\nUSER root\n"), ["DL3002"]);
        assert!(codes("FROM alpine:3.20 AS build\nUSER root\nFROM alpine:3.20\nUSER app\n").is_empty(), "only the last stage's user counts");
        // A good one says nothing.
        assert!(codes("FROM ubuntu:24.04\nRUN apt-get update \\\n && apt-get install -y --no-install-recommends curl \\\n && rm -rf /var/lib/apt/lists/*\nWORKDIR /app\nCOPY . .\nUSER app\nCMD [\"./run\"]\n").is_empty());
    }
}
