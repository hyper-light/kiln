//! What `kiln setup` draws in the terminal, and how it asks. Where the terminal shows colour it
//! has hyperlight's look: the prism (hyperlight-site's `--prism`, the UI's too), an eyebrow over
//! each part (the site's spectrum rule beside a quiet capital label), its marks, and the shards
//! mark opening the setup ([`super::art`]). Where it doesn't (output that isn't a terminal,
//! `NO_COLOR`, `TERM=dumb`, a Windows console without VT), plain text.
//!
//! Questions: a list chosen with the arrow keys (by number where keys don't come one at a time:
//! Windows, or input that isn't a terminal), yes or no, a line of text, a password. Motion (off
//! with `KILN_NO_MOTION`) is the opening and the line a wait shows: setup redraws only what it
//! owns the screen for, never lines something else wrote.

use std::future::Future;
use std::io::{IsTerminal as _, Write as _};
use std::time::{Duration, Instant};

use futures::FutureExt as _;

use super::art::{self, Cell, Rgb};

/// hyperlight's prism: `--prism`'s stops (hyperlight-site `app/globals.css`, `ui/src/index.css`).
pub const PRISM: [Rgb; 5] = [(0xb8, 0xd9, 0xd2), (0xa6, 0xc4, 0xed), (0xb8, 0xa6, 0xd5), (0xd6, 0xa2, 0xaa), (0xd8, 0xcb, 0xb0)];
/// The UI's quieter greys and its status colours (`ui/src/index.css`).
const SECONDARY: Rgb = (0xa1, 0xa4, 0xad);
const MUTED: Rgb = (0x97, 0x99, 0x9f);
const FAINT: Rgb = (0x6d, 0x70, 0x78);
const OK: Rgb = (0xa8, 0xc9, 0xbc);
const WARN: Rgb = (0xcb, 0xb6, 0x9a);
const FAIL: Rgb = (0xd1, 0xaa, 0xad);
const RESET: &str = "\x1b[0m";
/// A frame of motion: 30 a second, as the site's studies move.
const FRAME: Duration = Duration::from_millis(33);
/// The shards mark's size in pixels (as many columns, half as many rows).
pub const MARK: usize = 22;
/// How long the shards take to rise to their places.
const RISE: f32 = 1.1;
/// A shard turning, for a line that waits.
const TURNING: [&str; 4] = ["◥", "◢", "◣", "◤"];

/// How much colour the terminal shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Color {
    None,
    /// The 256 of xterm's palette.
    Palette,
    True,
}

/// The terminal setup draws on (standard error) and takes answers from (standard input).
#[derive(Clone, Copy, Debug)]
pub struct Term {
    pub color: Color,
    /// Frames redrawn in place: a terminal with colour, tall enough, `KILN_NO_MOTION` unset.
    pub motion: bool,
    /// Keys come one at a time (Unix, input a terminal): lists are chosen with the arrow keys.
    pub keys: bool,
    pub width: usize,
    /// A light background (`COLORFGBG`): every colour a shade darker.
    pub light: bool,
}

/// The person stopped setup (Ctrl-C).
#[derive(Debug)]
pub struct Interrupted;

impl std::fmt::Display for Interrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("setup was stopped")
    }
}

impl std::error::Error for Interrupted {}

/// The tone a line of text is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Plain,
    Muted,
}

/// A mark before a line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    /// Done, or nothing to do.
    Done,
    /// To do.
    Todo,
    /// Left for a person, or not as it should be.
    Warn,
    Fail,
    /// Not done, on purpose.
    Skip,
}

/// The prism's colour at `at` (one turn of it from 0 to 1, going round).
pub fn prism_at(at: f32) -> Rgb {
    let n = PRISM.len() as f32;
    let p = at.rem_euclid(1.0) * n;
    let i = p.floor() as usize % PRISM.len();
    let from = PRISM.get(i).copied().unwrap_or(MUTED);
    let to = PRISM.get((i + 1) % PRISM.len()).copied().unwrap_or(MUTED);
    art::mix(from, to, p - p.floor())
}

/// The nearest colour of xterm's 256: its 6×6×6 cube or its greys.
fn palette(c: Rgb) -> u8 {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let near = |v: u8| LEVELS.iter().enumerate().min_by_key(|(_, l)| l.abs_diff(v)).map_or(0, |(i, _)| i);
    let level = |i: usize| LEVELS.get(i).copied().unwrap_or(0);
    let (r, g, b) = (near(c.0), near(c.1), near(c.2));
    let dist = |x: Rgb| [(x.0, c.0), (x.1, c.1), (x.2, c.2)].iter().map(|(a, b)| u32::from(a.abs_diff(*b)).pow(2)).sum::<u32>();
    let grey = ((u16::from(c.0) + u16::from(c.1) + u16::from(c.2)) / 3).saturating_sub(3) / 10;
    let grey = u8::try_from(grey.min(23)).unwrap_or(23);
    let g8 = 8 + 10 * grey;
    if dist((g8, g8, g8)) < dist((level(r), level(g), level(b))) {
        232 + grey
    } else {
        u8::try_from(16 + 36 * r + 6 * g + b).unwrap_or(15)
    }
}

/// `text` in lines of at most `width` characters, broken between words (a word longer than that
/// keeps a line of its own); a line break in it starts a new line.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut lines = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        for word in para.split_whitespace() {
            let (had, adds) = (line.chars().count(), word.chars().count());
            if had > 0 && had + 1 + adds > width {
                lines.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        lines.push(line);
    }
    lines
}

/// `s`, cut to `max` characters (the last of them `…`) when it's longer.
pub fn fit(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Write `s` to standard error at once.
pub fn write(s: &str) {
    let mut e = std::io::stderr().lock();
    let _ = e.write_all(s.as_bytes());
    let _ = e.flush();
}

/// This terminal's columns and rows.
fn size() -> (usize, usize) {
    #[cfg(unix)]
    if let Ok(w) = rustix::termios::tcgetwinsize(std::io::stderr())
        && w.ws_col > 0
        && w.ws_row > 0
    {
        return (usize::from(w.ws_col), usize::from(w.ws_row));
    }
    let env = |k: &str, d: usize| std::env::var(k).ok().and_then(|v| v.trim().parse().ok()).filter(|n| *n > 0).unwrap_or(d);
    (env("COLUMNS", 80), env("LINES", 24))
}

impl Term {
    /// This process's terminal; `asking`: setup may ask questions (not `--unattended`).
    pub fn detect(asking: bool) -> Term {
        let out = std::io::stderr().is_terminal() && crate::os::console_takes_escapes();
        Term::from_env(out, asking && std::io::stdin().is_terminal(), size(), &|k| std::env::var(k).ok())
    }

    /// [`Term::detect`] from what it looks at: whether standard error is a terminal that takes
    /// escapes, whether answers come from one, its size, and the environment.
    pub fn from_env(out: bool, input: bool, (width, height): (usize, usize), env: &dyn Fn(&str) -> Option<String>) -> Term {
        let set = |k: &str| env(k).is_some_and(|v| !v.is_empty());
        let term = env("TERM").unwrap_or_default();
        let true_color = env("COLORTERM").is_some_and(|c| c == "truecolor" || c == "24bit")
            || set("WT_SESSION")
            || env("TERM_PROGRAM").is_some_and(|p| ["iTerm.app", "WezTerm", "vscode", "ghostty", "Hyper", "Tabby", "rio", "zed"].contains(&p.as_str()))
            || ["kitty", "alacritty", "foot", "ghostty", "wezterm", "direct"].iter().any(|t| term.contains(t));
        let color = if !out || set("NO_COLOR") || term == "dumb" {
            Color::None
        } else if true_color {
            Color::True
        } else {
            Color::Palette
        };
        // `COLORFGBG` is "foreground;background" in the 16 colours: 7 and 15 are light.
        let light = env("COLORFGBG").and_then(|v| v.rsplit(';').next().and_then(|b| b.trim().parse::<u8>().ok())).is_some_and(|bg| bg == 7 || bg == 15);
        Term { color, motion: color != Color::None && !set("KILN_NO_MOTION") && height >= 16, keys: cfg!(unix) && input, width: width.clamp(40, 200), light }
    }

    /// Plain text: no colour, no motion, answers as lines.
    #[cfg(test)]
    pub fn plain() -> Term {
        Term { color: Color::None, motion: false, keys: false, width: 80, light: false }
    }

    /// How wide a paragraph is.
    pub fn text_width(&self) -> usize {
        self.width.saturating_sub(6).clamp(34, 84)
    }

    fn escape(&self, c: Rgb, background: bool) -> String {
        let c = if self.light { art::mix(c, (0, 0, 0), 0.4) } else { c };
        let layer = if background { 48 } else { 38 };
        match self.color {
            Color::None => String::new(),
            Color::True => format!("\x1b[{layer};2;{};{};{}m", c.0, c.1, c.2),
            Color::Palette => format!("\x1b[{layer};5;{}m", palette(c)),
        }
    }

    /// `s` in colour `c`.
    pub fn paint(&self, c: Rgb, s: &str) -> String {
        if self.color == Color::None || s.is_empty() { s.to_owned() } else { format!("{}{s}{RESET}", self.escape(c, false)) }
    }

    pub fn bold(&self, s: &str) -> String {
        if self.color == Color::None { s.to_owned() } else { format!("\x1b[1m{s}{RESET}") }
    }

    pub fn secondary(&self, s: &str) -> String {
        self.paint(SECONDARY, s)
    }

    pub fn muted(&self, s: &str) -> String {
        self.paint(MUTED, s)
    }

    pub fn faint(&self, s: &str) -> String {
        self.paint(FAINT, s)
    }

    pub fn ok(&self, s: &str) -> String {
        self.paint(OK, s)
    }

    pub fn warn(&self, s: &str) -> String {
        self.paint(WARN, s)
    }

    pub fn fail(&self, s: &str) -> String {
        self.paint(FAIL, s)
    }

    fn tone(&self, tone: Tone, s: &str) -> String {
        match tone {
            Tone::Plain => s.to_owned(),
            Tone::Muted => self.muted(s),
        }
    }

    /// `s` with the prism through it, from `phase` (spaces left as they are).
    pub fn prism(&self, s: &str, phase: f32) -> String {
        if self.color == Color::None {
            return s.to_owned();
        }
        let span = (s.chars().count().max(6) as f32) * 1.3;
        let mut out = String::new();
        for (i, ch) in s.chars().enumerate() {
            if ch != ' ' {
                out.push_str(&self.escape(prism_at(phase + i as f32 / span), false));
            }
            out.push(ch);
        }
        out.push_str(RESET);
        out
    }

    /// A part's eyebrow, as the site sets one: the spectrum rule, then the label in capitals.
    pub fn eyebrow(&self, label: &str) -> String {
        format!("  {}  {}", self.prism("━━━━", 0.0), self.secondary(&label.to_uppercase()))
    }

    pub fn mark(&self, m: Mark) -> String {
        match m {
            Mark::Done => self.ok("✓"),
            Mark::Todo => self.paint(prism_at(0.45), "◆"),
            Mark::Warn => self.warn("!"),
            Mark::Fail => self.fail("✗"),
            Mark::Skip => self.faint("·"),
        }
    }

    pub fn line(&self, s: &str) {
        write(&format!("{s}\n"));
    }

    /// A part begins: a blank line, its eyebrow, a blank line.
    pub fn part(&self, label: &str) {
        self.line("");
        self.line(&self.eyebrow(label));
        self.line("");
    }

    /// `text`, wrapped to the terminal, each line indented by `indent`.
    pub fn para(&self, indent: usize, tone: Tone, text: &str) {
        let pad = " ".repeat(indent);
        for l in wrap(text, self.text_width().saturating_sub(indent.saturating_sub(2))) {
            self.line(&format!("{pad}{}", self.tone(tone, &l)));
        }
    }

    /// Commands for a person or for sudo, set apart: one a line, indented.
    pub fn commands(&self, commands: &[String]) {
        for c in commands {
            self.line(&format!("      {}", self.secondary(c)));
        }
    }

    /// One line of a list: its mark, what it is, and (quieter, beside it when there's room, else
    /// under it) more about it.
    pub fn item(&self, m: Mark, label: &str, detail: &str) {
        self.item_in(m, label, detail, 34);
    }

    /// [`Term::item`], its detail from `column` on (a list's own, so its details line up).
    pub fn item_in(&self, m: Mark, label: &str, detail: &str, column: usize) {
        let used = label.chars().count();
        let pad = column.saturating_sub(used).max(2);
        let room = self.width.saturating_sub(4 + used + pad + 1);
        let mark = self.mark(m);
        if detail.is_empty() {
            self.line(&format!("  {mark} {label}"));
        } else if detail.chars().count() <= room {
            self.line(&format!("  {mark} {label}{}{}", " ".repeat(pad), self.faint(detail)));
        } else {
            self.line(&format!("  {mark} {label}"));
            for l in wrap(detail, self.width.saturating_sub(8)) {
                self.line(&format!("    {}", self.faint(&l)));
            }
        }
    }

    /// A drawing's rows of cells as this terminal shows them (nothing without colour).
    pub fn art(&self, rows: &[Vec<Cell>]) -> Vec<String> {
        if self.color == Color::None {
            return Vec::new();
        }
        rows.iter()
            .map(|row| {
                let mut s = String::new();
                for c in row {
                    match (c.top, c.bottom) {
                        (Some(t), Some(b)) if t == b => s.push_str(&format!("{}█{RESET}", self.escape(t, false))),
                        (Some(t), Some(b)) => s.push_str(&format!("{}{}▀{RESET}", self.escape(t, false), self.escape(b, true))),
                        (Some(t), None) => s.push_str(&format!("{}▀{RESET}", self.escape(t, false))),
                        (None, Some(b)) => s.push_str(&format!("{}▄{RESET}", self.escape(b, false))),
                        (None, None) => s.push(' '),
                    }
                }
                s
            })
            .collect()
    }

    /// The opening's lines: the shards mark (`rise` of the way to its place, at `t` seconds) with
    /// `side` beside it, in the middle of its height. Just `side` where there's no room or colour.
    pub fn opening_rows(&self, side: &[String], t: f32, rise: f32) -> Vec<String> {
        let mark = self.art(&art::frame(MARK, t, rise));
        if mark.is_empty() || self.width < MARK + 50 {
            return side.iter().map(|s| if s.is_empty() { String::new() } else { format!("  {s}") }).collect();
        }
        let top = mark.len().saturating_sub(side.len()) / 2;
        mark.iter()
            .enumerate()
            .map(|(i, m)| match i.checked_sub(top).and_then(|j| side.get(j)) {
                Some(s) if !s.is_empty() => format!("  {m}    {s}"),
                _ => format!("  {m}"),
            })
            .collect()
    }
}

/// Shows the cursor again when dropped (motion hides it).
struct Cursor;

impl Cursor {
    fn hide() -> Cursor {
        write("\x1b[?25l");
        Cursor
    }
}

impl Drop for Cursor {
    fn drop(&mut self) {
        write("\x1b[?25h");
    }
}

/// Waits for Ctrl-C, which setup holds for as long as it runs (`signals`; never, without it):
/// whatever it waits on is let go of, the terminal is put back, and it says it stopped.
pub async fn stopped(signals: Option<&crate::rt::Signals>) {
    match signals {
        Some(s) => {
            let _ = s.next().await;
        }
        None => futures::future::pending::<()>().await,
    }
}

/// `work`, unless Ctrl-C comes first.
pub async fn unless_stopped<T>(signals: Option<&crate::rt::Signals>, work: impl Future<Output = T>) -> anyhow::Result<T> {
    let work = work.fuse();
    let stop = stopped(signals).fuse();
    futures::pin_mut!(work, stop);
    futures::select_biased! {
        () = stop => Err(Interrupted.into()),
        v = work => Ok(v),
    }
}

/// A question, asked on the blocking pool (it waits for a person), unless Ctrl-C comes first.
pub async fn asking<T: Send + 'static>(signals: Option<&crate::rt::Signals>, question: impl FnOnce() -> anyhow::Result<T> + Send + 'static) -> anyhow::Result<T> {
    unless_stopped(signals, crate::rt::blocking(question)).await??
}

/// Lines redrawn in place: up to the first of the `had` drawn last, then each anew.
fn redraw(rows: &[String], had: usize) {
    let mut s = if had > 0 { format!("\x1b[{had}A") } else { String::new() };
    for r in rows {
        s.push_str(&format!("\r{r}\x1b[K\n"));
    }
    write(&s);
}

/// The opening: the shards mark rising to its place beside `side` (kiln's name and what it is)
/// while `work` runs, and for at least the moment the shards take to arrive; then it stays as it
/// was last drawn. Without motion it's drawn once. Ctrl-C (`signals`) stops it.
pub async fn opening<T>(t: Term, side: &[String], signals: Option<&crate::rt::Signals>, work: impl Future<Output = T>) -> anyhow::Result<T> {
    if !t.motion || t.color == Color::None || t.width < MARK + 50 {
        for r in t.opening_rows(side, 0.0, 1.0) {
            t.line(&r);
        }
        return unless_stopped(signals, work).await;
    }
    let _cursor = Cursor::hide();
    let start = Instant::now();
    let mut work = std::pin::pin!(work.fuse());
    let mut out = None;
    let mut had = 0;
    loop {
        let s = start.elapsed().as_secs_f32();
        let rise = (s / RISE).min(1.0);
        let rows = t.opening_rows(side, s, rise);
        redraw(&rows, had);
        had = rows.len();
        if rise >= 1.0
            && let Some(out) = out.take()
        {
            return Ok(out);
        }
        let mut tick = std::pin::pin!(crate::rt::sleep(FRAME).fuse());
        let mut stop = std::pin::pin!(stopped(signals).fuse());
        futures::select_biased! {
            () = stop => return Err(Interrupted.into()),
            v = work => out = Some(v),
            () = tick => {}
        }
    }
}

/// `work`, while one line shows `label` with the prism flowing through it, a shard turning before
/// it, and how long it has taken; the line is cleared once `work` ends, for the caller to say how
/// it went. Without motion the line is written once. Ctrl-C (`signals`) stops it.
pub async fn wait<T>(t: Term, label: &str, signals: Option<&crate::rt::Signals>, work: impl Future<Output = T>) -> anyhow::Result<T> {
    if !t.motion {
        t.line(&format!("  {} {}", t.mark(Mark::Skip), t.muted(&format!("{label}…"))));
        return unless_stopped(signals, work).await;
    }
    let _cursor = Cursor::hide();
    let start = Instant::now();
    let mut work = std::pin::pin!(work.fuse());
    loop {
        let s = start.elapsed().as_secs_f32();
        let turn = TURNING.get((s * 8.0) as usize % TURNING.len()).copied().unwrap_or("◆");
        let took = if s >= 2.0 { t.faint(&format!("  {}s", s as u64)) } else { String::new() };
        write(&format!("\r\x1b[2K  {} {}{took}", t.paint(prism_at(s * 0.35), turn), t.prism(label, s * 0.35)));
        let mut tick = std::pin::pin!(crate::rt::sleep(FRAME * 2).fuse());
        let mut stop = std::pin::pin!(stopped(signals).fuse());
        futures::select_biased! {
            () = stop => {
                write("\r\x1b[2K");
                return Err(Interrupted.into());
            }
            v = work => {
                write("\r\x1b[2K");
                return Ok(v);
            }
            () = tick => {}
        }
    }
}

// ------------------------------------------------------------------------------------ questions

/// A key, as setup's questions read them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Enter,
    /// Ctrl-C (or Ctrl-D).
    Stop,
    Char(char),
    Other,
}

/// The keys in what a terminal sent: arrows as xterm sends them (`ESC [ A`, or `ESC O A` in its
/// application mode), Ctrl-P and Ctrl-N, Tab; every other escape sequence read whole and passed
/// over.
pub fn keys(bytes: &[u8]) -> Vec<Key> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        i += 1;
        let key = match b {
            0x1b => match (bytes.get(i), bytes.get(i + 1)) {
                (Some(b'[' | b'O'), Some(b'A')) => {
                    i += 2;
                    Key::Up
                }
                (Some(b'[' | b'O'), Some(b'B')) => {
                    i += 2;
                    Key::Down
                }
                (Some(b'['), _) => {
                    i += 1;
                    while let Some(&c) = bytes.get(i) {
                        i += 1;
                        if (0x40..=0x7e).contains(&c) {
                            break;
                        }
                    }
                    Key::Other
                }
                (Some(b'O'), Some(_)) => {
                    i += 2;
                    Key::Other
                }
                _ => Key::Other,
            },
            b'\r' => {
                if bytes.get(i) == Some(&b'\n') {
                    i += 1;
                }
                Key::Enter
            }
            b'\n' => Key::Enter,
            0x03 | 0x04 => Key::Stop,
            0x10 => Key::Up,
            0x0e | b'\t' => Key::Down,
            0x20..=0x7e => Key::Char(char::from(b)),
            _ => Key::Other,
        };
        out.push(key);
    }
    out
}

/// Standard input taking keys one at a time, unechoed, for as long as this lives (Unix). Output
/// still goes through the terminal's processing, so a line ends as a line.
struct Raw(#[cfg(unix)] Option<rustix::termios::Termios>);

impl Raw {
    #[cfg(unix)]
    fn on() -> Raw {
        let stdin = std::io::stdin();
        let Ok(was) = rustix::termios::tcgetattr(&stdin) else { return Raw(None) };
        let mut raw = was.clone();
        raw.make_raw();
        raw.output_modes.insert(rustix::termios::OutputModes::OPOST);
        match rustix::termios::tcsetattr(&stdin, rustix::termios::OptionalActions::Now, &raw) {
            Ok(()) => Raw(Some(was)),
            Err(_) => Raw(None),
        }
    }

    #[cfg(not(unix))]
    fn on() -> Raw {
        Raw()
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(was) = &self.0 {
            let _ = rustix::termios::tcsetattr(std::io::stdin(), rustix::termios::OptionalActions::Now, was);
        }
    }
}

/// The next keys typed (in raw mode: as soon as there's one).
fn read_keys() -> anyhow::Result<Vec<Key>> {
    use std::io::Read as _;
    let mut buf = [0u8; 64];
    let n = std::io::stdin().read(&mut buf)?;
    anyhow::ensure!(n > 0, "no answer: the input ended");
    Ok(keys(buf.get(..n).unwrap_or_default()))
}

/// A line typed, without its line ending.
fn read_line() -> anyhow::Result<String> {
    let mut line = String::new();
    let n = std::io::stdin().read_line(&mut line)?;
    anyhow::ensure!(n > 0, "no answer: the input ended");
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}

/// One of a list's choices.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    pub label: String,
    pub detail: String,
}

impl Choice {
    pub fn new(label: impl Into<String>, detail: impl Into<String>) -> Choice {
        Choice { label: label.into(), detail: detail.into() }
    }
}

/// The list as it's drawn with `at` chosen: a line each, a blank one, and how to choose.
fn list(t: &Term, choices: &[Choice], at: usize) -> Vec<String> {
    let column = choices.iter().map(|c| c.label.chars().count()).max().unwrap_or(0) + 3;
    let mut rows: Vec<String> = choices
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let pad = " ".repeat(column.saturating_sub(c.label.chars().count()));
            let detail = fit(&c.detail, t.width.saturating_sub(5 + column));
            if i == at {
                format!("  {} {}{pad}{}", t.mark(Mark::Todo), t.bold(&c.label), t.muted(&detail))
            } else {
                format!("    {}{pad}{}", t.secondary(&c.label), t.faint(&detail))
            }
        })
        .collect();
    rows.push(String::new());
    rows.push(format!("  {}", t.faint("↑ ↓ to choose · enter to go on · ctrl-c to leave")));
    rows
}

/// One of `choices`, `default` first: with the arrow keys (or j and k, or its number), Enter
/// taking it; by number where keys don't come one at a time. What was chosen stays on screen.
pub fn choose(t: Term, choices: &[Choice], default: usize) -> anyhow::Result<usize> {
    let last = choices.len().saturating_sub(1);
    let mut at = default.min(last);
    if !t.keys {
        for (i, c) in choices.iter().enumerate() {
            t.line(&format!("    {}  {:<w$}  {}", t.secondary(&(i + 1).to_string()), c.label, t.faint(&c.detail), w = choices.iter().map(|c| c.label.chars().count()).max().unwrap_or(0)));
        }
        loop {
            write(&format!("  {} {} ", t.muted("Which one?"), t.faint(&format!("[{}]", at + 1))));
            let answer = read_line()?;
            let answer = answer.trim();
            if answer.is_empty() {
                return Ok(at);
            }
            match answer.parse::<usize>().ok().and_then(|n| n.checked_sub(1)).filter(|n| *n <= last) {
                Some(n) => return Ok(n),
                None => t.line(&format!("    {} {}", t.mark(Mark::Fail), t.muted(&format!("a number from 1 to {}", last + 1)))),
            }
        }
    }
    let raw = Raw::on();
    let mut rows = list(&t, choices, at);
    redraw(&rows, 0);
    let chosen = 'keys: loop {
        for k in read_keys()? {
            match k {
                Key::Up | Key::Char('k') => at = at.checked_sub(1).unwrap_or(last),
                Key::Down | Key::Char('j') => at = if at >= last { 0 } else { at + 1 },
                Key::Char(c) if c.is_ascii_digit() => {
                    if let Some(n) = c.to_digit(10).and_then(|d| usize::try_from(d).ok()).and_then(|d| d.checked_sub(1)).filter(|n| *n <= last) {
                        at = n;
                        break 'keys at;
                    }
                }
                Key::Enter => break 'keys at,
                Key::Stop | Key::Char('q') => {
                    drop(raw);
                    return Err(Interrupted.into());
                }
                _ => {}
            }
        }
        let had = rows.len();
        rows = list(&t, choices, at);
        redraw(&rows, had);
    };
    drop(raw);
    // The list gives way to what was chosen.
    let mut s = format!("\x1b[{}A\r\x1b[J", rows.len());
    if let Some(c) = choices.get(chosen) {
        s.push_str(&format!("  {} {}  {}\n", t.mark(Mark::Todo), t.bold(&c.label), t.faint(&fit(&c.detail, t.width.saturating_sub(8 + c.label.chars().count())))));
    }
    write(&s);
    Ok(chosen)
}

/// Yes or no, `default` on Enter: a key (y or n) where keys come one at a time, else a line.
pub fn yes(t: Term, question: &str, default: bool) -> anyhow::Result<bool> {
    let hint = if default { "[Y/n]" } else { "[y/N]" };
    write(&format!("  {} {} ", question, t.faint(hint)));
    let answer = if t.keys {
        let raw = Raw::on();
        let answer = 'keys: loop {
            for k in read_keys()? {
                match k {
                    Key::Char('y' | 'Y') => break 'keys true,
                    Key::Char('n' | 'N') => break 'keys false,
                    Key::Enter => break 'keys default,
                    Key::Stop => {
                        drop(raw);
                        write("\n");
                        return Err(Interrupted.into());
                    }
                    _ => {}
                }
            }
        };
        drop(raw);
        answer
    } else {
        loop {
            match read_line()?.trim().to_ascii_lowercase().as_str() {
                "" => break default,
                "y" | "yes" => break true,
                "n" | "no" => break false,
                _ => write(&format!("  {} {} ", t.muted("y or n?"), t.faint(hint))),
            }
        }
    };
    if t.keys {
        write(&format!("{}\n", t.secondary(if answer { "yes" } else { "no" })));
    }
    Ok(answer)
}

/// A line of text, `default` on Enter, asked again until `check` takes it (and says what it
/// makes of it).
pub fn ask(t: Term, question: &str, default: &str, check: &dyn Fn(&str) -> Result<String, String>) -> anyhow::Result<String> {
    loop {
        let hint = if default.is_empty() { String::new() } else { format!(" {}", t.faint(&format!("[{default}]"))) };
        write(&format!("  {question}{hint} "));
        let typed = read_line()?;
        let answer = if typed.trim().is_empty() { default } else { typed.trim() };
        if typed.trim().is_empty() && !default.is_empty() && t.keys && t.color != Color::None {
            // Nothing typed: the line says what Enter took.
            write(&format!("\x1b[1A\r\x1b[2K  {question} {}\n", t.secondary(default)));
        }
        match check(answer) {
            Ok(v) => return Ok(v),
            Err(why) => t.line(&format!("    {} {}", t.mark(Mark::Fail), t.muted(&why))),
        }
    }
}

/// A secret, typed without echo, as typed (empty when the person just pressed Enter). Where keys
/// come one at a time it's read here, so Ctrl-C (a key then, not a signal) leaves the terminal
/// echoing again; elsewhere by rpassword.
pub fn secret(t: Term, question: &str) -> anyhow::Result<String> {
    use std::io::Read as _;
    if !t.keys {
        return Ok(rpassword::prompt_password(format!("  {question} "))?);
    }
    write(&format!("  {question} "));
    let raw = Raw::on();
    let mut typed: Vec<u8> = Vec::new();
    let mut escape = false;
    'keys: loop {
        let mut buf = [0u8; 64];
        let n = std::io::stdin().read(&mut buf)?;
        anyhow::ensure!(n > 0, "no answer: the input ended");
        for &b in buf.get(..n).unwrap_or_default() {
            match b {
                // An escape sequence (an arrow key, say) is no part of a secret: up to its last byte.
                _ if escape => escape = !(0x40..=0x7e).contains(&b) || b == b'[' || b == b'O',
                0x1b => escape = true,
                b'\r' | b'\n' => break 'keys,
                0x03 | 0x04 => {
                    drop(raw);
                    write("\n");
                    return Err(Interrupted.into());
                }
                0x7f | 0x08 => {
                    // One character back, all of its bytes.
                    while typed.last().is_some_and(|c| c & 0xc0 == 0x80) {
                        typed.pop();
                    }
                    typed.pop();
                }
                0x15 => typed.clear(),
                0x20.. => typed.push(b),
                _ => {}
            }
        }
    }
    drop(raw);
    write("\n");
    Ok(String::from_utf8_lossy(&typed).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| (*v).to_owned())
    }

    #[test]
    fn colour_and_motion_follow_the_terminal() {
        let t = Term::from_env(true, true, (120, 40), &env(&[("TERM", "xterm-256color"), ("COLORTERM", "truecolor")]));
        assert_eq!((t.color, t.motion, t.keys, t.width, t.light), (Color::True, true, cfg!(unix), 120, false));
        assert_eq!(Term::from_env(true, true, (120, 40), &env(&[("TERM", "xterm-256color")])).color, Color::Palette);
        assert_eq!(Term::from_env(true, true, (120, 40), &env(&[("TERM", "xterm-ghostty")])).color, Color::True);
        // No colour: not a terminal, NO_COLOR, a dumb terminal. No motion without colour.
        for t in [
            Term::from_env(false, true, (120, 40), &env(&[("COLORTERM", "truecolor")])),
            Term::from_env(true, true, (120, 40), &env(&[("NO_COLOR", "1"), ("COLORTERM", "truecolor")])),
            Term::from_env(true, true, (120, 40), &env(&[("TERM", "dumb")])),
        ] {
            assert_eq!((t.color, t.motion), (Color::None, false), "{t:?}");
        }
        // NO_COLOR set but empty is unset, as no-color.org says.
        assert_eq!(Term::from_env(true, true, (120, 40), &env(&[("NO_COLOR", ""), ("TERM", "xterm")])).color, Color::Palette);
        assert!(!Term::from_env(true, true, (120, 40), &env(&[("KILN_NO_MOTION", "1")])).motion);
        assert!(!Term::from_env(true, true, (120, 10), &env(&[])).motion, "too short to redraw the opening in place");
        assert!(!Term::from_env(true, false, (120, 40), &env(&[])).keys, "answers that aren't typed come as lines");
        assert!(Term::from_env(true, true, (120, 40), &env(&[("COLORFGBG", "0;15")])).light);
        assert!(!Term::from_env(true, true, (120, 40), &env(&[("COLORFGBG", "15;0")])).light);
        assert_eq!(Term::from_env(true, true, (10, 40), &env(&[])).width, 40);
    }

    #[test]
    fn plain_text_has_no_escapes() {
        let t = Term::plain();
        for s in [t.prism("kiln", 0.3), t.eyebrow("The front door"), t.ok("✓"), t.bold("x"), t.mark(Mark::Todo)] {
            assert!(!s.contains('\x1b'), "{s:?}");
        }
        assert_eq!(t.eyebrow("The front door"), "  ━━━━  THE FRONT DOOR");
        assert!(t.art(&art::frame(MARK, 0.0, 1.0)).is_empty(), "no mark without colour");
        assert_eq!(t.opening_rows(&["kiln".to_owned()], 0.0, 1.0), ["  kiln"]);
    }

    #[test]
    fn the_prism_runs_through_text() {
        let t = Term { color: Color::True, ..Term::plain() };
        let s = t.prism("kiln", 0.0);
        assert!(s.starts_with("\x1b[38;2;184;217;210mk"), "{s:?}");
        assert_eq!(s.matches("\x1b[38;2;").count(), 4, "a colour a letter");
        assert!(s.ends_with(RESET));
        assert_eq!(Some(prism_at(0.0)), PRISM.first().copied());
        assert_eq!(Some(prism_at(1.0)), PRISM.first().copied(), "it goes round");
        assert_eq!(Some(prism_at(0.2)), PRISM.get(1).copied());
        let p = Term { color: Color::Palette, ..Term::plain() };
        assert!(p.paint((0xa8, 0xc9, 0xbc), "ok").starts_with("\x1b[38;5;"));
        assert_eq!(palette((0, 0, 0)), 16);
        assert_eq!(palette((255, 255, 255)), 231);
        assert_eq!(palette((128, 128, 128)), 244);
        // The mark, as half blocks in its colours, beside the name.
        let rows = t.opening_rows(&["kiln".to_owned(), "setup".to_owned()], 0.0, 1.0);
        assert_eq!(rows.len(), MARK / 2);
        assert!(rows.iter().any(|r| r.contains('▀') || r.contains('▄') || r.contains('█')));
        assert!(rows.iter().any(|r| r.ends_with("    kiln")) && rows.iter().any(|r| r.ends_with("    setup")));
    }

    #[test]
    fn keys_are_read_as_terminals_send_them() {
        assert_eq!(keys(b"\x1b[A\x1b[B\x1bOA\x1bOB"), [Key::Up, Key::Down, Key::Up, Key::Down]);
        assert_eq!(keys(b"\r\n"), [Key::Enter], "one Enter, whatever ends the line");
        assert_eq!(keys(b"\r\r"), [Key::Enter, Key::Enter]);
        assert_eq!(keys(b"\x03"), [Key::Stop]);
        assert_eq!(keys(b"\x1b[1;5C2"), [Key::Other, Key::Char('2')], "an escape sequence is read whole");
        assert_eq!(keys(b"\x1b"), [Key::Other]);
        assert_eq!(keys(b"jk\x10\x0e\t"), [Key::Char('j'), Key::Char('k'), Key::Up, Key::Down, Key::Down]);
        assert_eq!(keys("é".as_bytes()), [Key::Other, Key::Other]);
    }

    #[test]
    fn text_is_wrapped_and_fitted() {
        assert_eq!(wrap("the front door makes this machine kiln's address", 20), ["the front door makes", "this machine kiln's", "address"]);
        assert_eq!(wrap("one\ntwo", 80), ["one", "two"]);
        assert_eq!(wrap("https://a-very-long-address.example/with/a/path short", 12), ["https://a-very-long-address.example/with/a/path", "short"]);
        assert_eq!(fit("kiln.internal", 20), "kiln.internal");
        assert_eq!(fit("kiln.internal", 6), "kiln.…");
        assert_eq!(fit("kiln", 0), "…");
    }

    #[test]
    fn a_list_marks_the_one_chosen() {
        let t = Term::plain();
        let choices = [Choice::new("This machine", "a service that starts at boot"), Choice::new("Docker", "a container")];
        let rows = list(&t, &choices, 1);
        assert_eq!(rows.first().map(String::as_str), Some("    This machine   a service that starts at boot"));
        assert_eq!(rows.get(1).map(String::as_str), Some("  ◆ Docker         a container"));
        assert_eq!(rows.len(), 4, "the choices, a blank line and the keys");
    }
}
