//! Where a terminal's replay can be cut so that its newest part, drawn alone, is the screen the whole replay
//! ends on (W21-R02; docs/wire.md, "A terminal's screen first").
//!
//! A shell's replay is its backlog, up to 256 KiB, and a viewer on a slow link waits for all of it before it
//! shows anything. The screen it ends on is drawn by its last few lines. A viewer that asks is sent those
//! ahead: the replay's bytes after a cut, which its terminal draws from nothing; then the whole replay, drawn
//! from nothing again. What goes ahead is the replay's own bytes, drawn by the page's own terminal, so nothing
//! of how the page draws (its widths, its colours, what it makes of a sequence) is second-guessed here: an
//! emulator's account of the screen was tried first, and the page drew it otherwise than the replay more often
//! than not. What has to hold is that the page's terminal ends in the same state both ways, and that is a
//! matter of where the cut is.
//!
//! **Why a cut stands.** Take a terminal that has drawn the replay up to the cut (*whole*) and a fresh one
//! given what a part begins with (*alone*), and give both the bytes after the cut.
//! - *Nothing hidden differs.* [`Scan`] follows what a terminal keeps that shows only in what's drawn later
//!   (the attributes text is written in, the scroll region, the character sets, the modes output is laid out
//!   by, an open hyperlink, the alternate screen), as the page's terminal changes it, and a cut is only where
//!   all of it is as a fresh terminal has it, and inside no sequence. The cursor's own look changes how
//!   nothing else is drawn, so it's written ahead of the part as it is at the cut ([`Look`]).
//! - *The cursor is where the part puts it: at the start of the last row, which is empty.* A cut is after a
//!   line feed that came with the cursor in the first column, and only once as many line feeds as the screen
//!   has rows have come since anything moved the cursor other than along its line or down by a line feed. The
//!   cursor is then on the last row, which that line feed scrolled in, empty. A part begins by putting its
//!   cursor there, so from the first byte the two scroll at the same line feeds, wrap at the same columns and
//!   fill each new row alike.
//! - *The lines after the cut take the screen.* For as many line feeds as the screen has rows after the cut,
//!   nothing moves the cursor but along its line or down by a line feed: every row those lines draw is drawn
//!   the same on both, and each line feed scrolls a row from before the cut off the screen. Once they have
//!   come, every row of both screens is one of those lines: the two terminals are in one state, and stay so
//!   whatever comes after.
//! - *A saved cursor* is the one thing a part can read of what came before it, two ways. One restored after
//!   the cut and saved before it isn't the part's. And the page's terminal keeps a saved cursor's row counted
//!   from the top of all it has drawn, its scrollback too, which a part has less of: restored after the screen
//!   has scrolled under it, or its scrollback been erased, it's on another row. A cut is let go of at such a
//!   restore. (A full-screen program saves as it takes the alternate screen and restores as it leaves it,
//!   with the screen itself untouched between: that one is restored the same.)
//!
//! The latest cut that stands is taken: it leaves the least to send ahead.
//!
//! **What the page keeps through a reset.** The page draws the part and then the replay on one terminal,
//! each behind a full reset, and its terminal keeps three things through one: whether the cursor shows, its
//! blink as an option, and whether a line feed returns the carriage. The first two change how nothing else
//! is drawn, and a part says them as they are at its cut, so the replay ends on them either way. The third
//! changes how every line after it is drawn: a replay that says it, anywhere, has no part.
//!
//! **What it's read against.** The page's terminal is xterm.js 6.0.0 (`ui/package-lock.json`), and what
//! follows is its `InputHandler.ts`: every sequence it registers a handler for is one of those below, kept as
//! it keeps it; one it has no handler for it ignores, and so does this. A test here holds the page's version to
//! that one: another's handlers are to be read against this, and the two drawings compared again in a browser
//! (`ci/ui/terminal_part.mjs`, which draws generated output both ways in the page's terminal and compares
//! every cell), before the test is moved.

use std::collections::VecDeque;
use std::ops::Range;

use vte::{Params, Perform};

/// What every replay, and every part, begins with: the attributes reset.
pub const RESET: &[u8] = b"\x1b[0m";

/// The part of `replay` to send ahead of it, where there's one worth sending: [`RESET`], the cursor's look
/// at the cut and its place (the start of the last row), the replay's output from the cut on, and what the
/// replay ends with after its output (the modes its input goes by). `output` is where in the replay its
/// output is; `rows` the height of the terminal it's drawn on.
pub fn ahead(replay: &[u8], output: Range<usize>, rows: u16) -> Option<Vec<u8>> {
    let part = newest_part(replay, output, rows)?;
    // Ahead of the replay only where that gains more than it costs: the screen comes sooner by the replay's
    // bytes less the part's, and the replay later by the part's.
    (replay.len().saturating_sub(part.len()) > part.len()).then_some(part)
}

/// The part of `replay` from its latest cut, whatever its size: [`ahead`] is what's sent.
pub fn newest_part(replay: &[u8], output: Range<usize>, rows: u16) -> Option<Vec<u8>> {
    let (body, modes) = (replay.get(output.clone())?, replay.get(output.end..)?);
    let cut = newest(body, rows)?;
    // The cursor at the start of the last row, where the whole replay has it at the cut.
    let last_row = format!("\x1b[{rows};1H");
    Some([RESET, &cut.look, last_row.as_bytes(), body.get(cut.at..)?, modes].concat())
}

/// A cut: where the part begins in the replay's bytes, and what's written ahead of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cut {
    /// The part is the replay's bytes from here.
    pub at: usize,
    /// The cursor's look as it is at the cut ([`Look`]), as the sequences that give a fresh terminal it.
    pub look: Vec<u8>,
}

/// The latest place a replay's bytes (`body`: what a fresh terminal of `rows` rows draws) can be cut so that
/// the part after it stands alone; none where there's no such place.
pub fn newest(body: &[u8], rows: u16) -> Option<Cut> {
    let mut scan = Scan::new(usize::from(rows));
    let mut parser = vte::Parser::new();
    // What a byte is within, as the page's parser has it: the parser here says neither where in its input it
    // is nor what it's within, and the two don't read everything alike.
    let (mut within, mut before, mut read) = (Within::Ground, 0, 0);
    for (at, byte) in body.iter().copied().enumerate() {
        // Where the two part ways, nothing after is known as the page has it. Inside a sequence, a byte past
        // ASCII: the page's parser gives the sequence up and prints the rest of it, this one passes over the
        // byte. Inside a string, a C1 control as a character: the page's ends the string there (and obeys
        // the control), this one takes it for more of the string.
        let parted = match within {
            Within::Escape | Within::Csi => byte >= 0x80,
            Within::String { .. } => before == 0xc2 && (0x80..=0x9f).contains(&byte),
            Within::Ground => false,
        };
        if parted {
            parser.advance(&mut scan, body.get(read..at).unwrap_or_default());
            scan.lost();
            (within, read) = (Within::Ground, at);
        }
        // A sub-parameter left empty (`4:m`): the sequence it's in is told, once what's before it is read.
        if within == Within::Csi && before == b':' && matches!(byte, b':' | b';' | 0x40..=0x7e) {
            parser.advance(&mut scan, body.get(read..at).unwrap_or_default());
            (scan.empty, read) = (true, at);
        }
        before = byte;
        let ground = within == Within::Ground;
        within = match (within, byte) {
            (_, 0x1b) => Within::Escape,
            (Within::Escape, b'[') => Within::Csi,
            (Within::Escape, b']') => Within::String { osc: true },
            (Within::Escape, b'P' | b'X' | b'^' | b'_') => Within::String { osc: false },
            // A sequence's last byte, or one that gives it up; a bell ends a string that's a command.
            (Within::Escape, 0x30..=0x7e) | (Within::Csi, 0x40..=0x7e) | (Within::String { osc: true }, 0x07) | (_, 0x18 | 0x1a) => Within::Ground,
            (within, _) => within,
        };
        // A cut is after a line feed that's inside nothing: one inside a sequence is followed by its rest.
        if byte == b'\n' && ground {
            parser.advance(&mut scan, body.get(read..=at).unwrap_or_default());
            read = at.saturating_add(1);
            scan.line_end(read);
        }
    }
    parser.advance(&mut scan, body.get(read..).unwrap_or_default());
    scan.stands.filter(|_| !scan.carried).map(|cut| Cut { at: cut.at, look: cut.look.bytes() })
}

/// What a byte of a terminal's output is within, as its parser has it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Within {
    Ground,
    /// After an escape, before the byte that says what it begins.
    Escape,
    /// A control sequence (`ESC [`), up to its last byte.
    Csi,
    /// A string: a command's (`ESC ]`, which a bell ends too), or a device's, an application's, a private
    /// message's (`ESC P`, `ESC _`, `ESC ^`, `ESC X`), up to the escape that ends it.
    String { osc: bool },
}

/// The cursor's own look: whether it shows, its shape and whether it blinks. It changes how nothing else is
/// drawn, so a part needn't be cut where it's a fresh terminal's: it begins with it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Look {
    /// Whether it's hidden, once `CSI ? 25 l` or `h` has said: the page's terminal keeps that through a full
    /// reset, so a part says it again rather than count on a fresh terminal's.
    hidden: Option<bool>,
    /// The shape `CSI Ps SP q` gave it, as the odd parameter that names it: 1 a block, 3 an underline, 5 a bar.
    shape: Option<u8>,
    /// Whether it blinks, as that sequence said too.
    blinks: Option<bool>,
    /// Whether it blinks where that sequence said nothing, as `CSI ? 12 h` and `l` set it: an option of the
    /// page's terminal, which a full reset leaves as it is.
    blink_option: Option<bool>,
}

impl Look {
    /// `CSI Ps SP q`, as xterm.js's `setCursorStyle`: 0 gives the terminal's own back; 1 to 6 a shape, the odd
    /// ones blinking; any other its blink alone.
    fn shaped(&mut self, param: u16) {
        if param == 0 {
            (self.shape, self.blinks) = (None, None);
            return;
        }
        if let Ok(n @ 1..=6) = u8::try_from(param) {
            self.shape = Some(n.saturating_sub(1) / 2 * 2 + 1);
        }
        self.blinks = Some(param % 2 == 1);
    }

    /// The sequences that give a fresh terminal this look.
    fn bytes(self) -> Vec<u8> {
        let mut out = Vec::new();
        if let Some(on) = self.blink_option {
            out.extend_from_slice(if on { b"\x1b[?12h" } else { b"\x1b[?12l" });
        }
        // A shape's parameter, the next one up where it doesn't blink; 7 and 8 set the blink alone.
        let steady = u8::from(self.blinks != Some(true));
        if let Some(param) = self.shape.map(|shape| shape.saturating_add(steady)).or(self.blinks.map(|_| 7_u8.saturating_add(steady))) {
            out.extend_from_slice(format!("\x1b[{param} q").as_bytes());
        }
        if let Some(hidden) = self.hidden {
            out.extend_from_slice(if hidden { b"\x1b[?25l" } else { b"\x1b[?25h" });
        }
        out
    }
}

/// Whether text would be written otherwise than a fresh terminal writes it: which attributes are on, not
/// what they are. As xterm.js's `charAttributes` changes them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Attrs {
    foreground: bool,
    background: bool,
    underline_colour: bool,
    /// Bold, dim, italic, underlined, blinking, inverse, hidden, struck through, overlined.
    on: u16,
    /// A colour sequence that doesn't read as one: what it left isn't known until the attributes are reset.
    unread: bool,
}

const BOLD: u16 = 1;
const DIM: u16 = 1 << 1;
const ITALIC: u16 = 1 << 2;
const UNDERLINED: u16 = 1 << 3;
const BLINKING: u16 = 1 << 4;
const INVERSE: u16 = 1 << 5;
const HIDDEN: u16 = 1 << 6;
const STRUCK: u16 = 1 << 7;
const OVERLINED: u16 = 1 << 8;

impl Attrs {
    fn plain(self) -> bool {
        self == Self::default()
    }

    /// `CSI Pm m`. Each attribute is read by its parameter's own value, whatever rides on it (`1:2` is
    /// bold), but an underline's style and a colour's mode and values. `empty` says a sub-parameter of the
    /// sequence was left empty, which this parser reads as 0 and the page's as none given.
    fn sgr(&mut self, params: &Params, empty: bool) {
        let mut params = params.iter();
        while let Some(param) = params.next() {
            let [code, rest @ ..] = param else { continue };
            match code {
                0 => *self = Self::default(),
                1 => self.on |= BOLD,
                2 => self.on |= DIM,
                3 => self.on |= ITALIC,
                // An underline's style rides on it (`4:3`): style 0 is none, and none given is a single one.
                4 if rest.first() == Some(&0) && !empty => self.on &= !UNDERLINED,
                4 | 21 => self.on |= UNDERLINED,
                5 => self.on |= BLINKING,
                7 => self.on |= INVERSE,
                8 => self.on |= HIDDEN,
                9 => self.on |= STRUCK,
                22 => self.on &= !(BOLD | DIM),
                23 => self.on &= !ITALIC,
                24 => self.on &= !UNDERLINED,
                25 => self.on &= !BLINKING,
                27 => self.on &= !INVERSE,
                28 => self.on &= !HIDDEN,
                29 => self.on &= !STRUCK,
                53 => self.on |= OVERLINED,
                55 => self.on &= !OVERLINED,
                30..=37 | 90..=97 => self.foreground = true,
                39 => self.foreground = false,
                40..=47 | 100..=107 => self.background = true,
                49 => self.background = false,
                59 => self.underline_colour = false,
                38 | 48 | 58 => {
                    // A colour by number (mode 5) or by its parts (mode 2): its mode and values ride on it
                    // (`38:5:n`), or are the parameters after it (`38;5;n`, `38;2;r;g;b`).
                    let set = match rest.first() {
                        Some(mode) => Some(matches!(mode, 2 | 5)),
                        None => match params.next() {
                            Some([5]) => params.next().filter(|value| value.len() == 1).map(|_| true),
                            Some([2]) => (0..3).all(|_| params.next().is_some_and(|value| value.len() == 1)).then_some(true),
                            _ => None,
                        },
                    };
                    match (set, code) {
                        (Some(true), 38) => self.foreground = true,
                        (Some(true), 48) => self.background = true,
                        (Some(true), _) => self.underline_colour = true,
                        // A mode that is neither changes nothing.
                        (Some(false), _) => {}
                        (None, _) => {
                            self.unread = true;
                            return;
                        }
                    }
                }
                // One the page's terminal doesn't know changes nothing.
                _ => {}
            }
        }
    }
}

/// A cursor saved: when, and what went with it.
#[derive(Debug, Clone, Copy)]
struct Saved {
    /// The line feeds there had been.
    at: usize,
    /// What had been drawn on the screen itself ([`Scan::drawn`]).
    drawn: usize,
    attrs: Attrs,
    /// The character sets were a fresh terminal's.
    plain_sets: bool,
}

/// A place the replay may be cut.
#[derive(Debug, Clone, Copy)]
struct Place {
    at: usize,
    /// The line feeds there had been, its own counted.
    since: usize,
    look: Look,
}

/// A replay read through: what its terminal keeps hidden as it goes, and where it can be cut.
struct Scan {
    rows: usize,
    attrs: Attrs,
    look: Look,
    alternate: bool,
    /// A scroll region other than the whole screen: the screen's own, and the alternate one's.
    region: [bool; 2],
    origin: bool,
    no_wrap: bool,
    reverse_wrap: bool,
    insert: bool,
    /// The four character sets, each other than US-ASCII; and which is in use.
    sets: [bool; 4],
    shifted: u8,
    link: bool,
    /// Something the page's terminal keeps that this doesn't follow (a tab stop, a protected cell, a soft
    /// reset, a control this parser reads otherwise than the page's does): no cut until a full reset.
    unread: bool,
    /// A colour of the terminal's own set, which a full reset isn't known to put back: no cut after it.
    kept: bool,
    /// Whether a line feed returns the carriage too was said (`CSI 20 h` or `l`). That's an option of the
    /// page's terminal, which keeps it through a full reset: read in a part, it would change how the replay
    /// after it is drawn from its first line. A replay that says it has no part.
    carried: bool,
    /// The cursor as each screen last saved it.
    saved: [Option<Saved>; 2],
    first_column: bool,
    /// Line feeds so far; and since the cursor last moved otherwise, as many as the screen's rows at the most.
    lines: usize,
    settled: usize,
    /// How many times the screen itself (not the alternate one) may have scrolled or had its scrollback
    /// erased: each character printed on it (one may wrap), each line feed, each erasure of saved lines.
    drawn: usize,
    /// Cuts fewer line feeds than the screen has rows have come since; and the latest that many have.
    waiting: VecDeque<Place>,
    stands: Option<Place>,
    /// The last thing read was a line feed.
    fed: bool,
    /// The sequence being read has a sub-parameter left empty ([`Attrs::sgr`]).
    empty: bool,
}

/// The character sets xterm.js draws otherwise than US-ASCII (`B`): its `CHARSETS`' other keys. A
/// designation by any other letter it ignores.
const DRAWN_SETS: &[u8] = b"0A4C5RQKYE6ZH7=";

impl Scan {
    fn new(rows: usize) -> Self {
        Self {
            rows,
            attrs: Attrs::default(),
            look: Look::default(),
            alternate: false,
            region: [false; 2],
            origin: false,
            no_wrap: false,
            reverse_wrap: false,
            insert: false,
            sets: [false; 4],
            shifted: 0,
            link: false,
            unread: false,
            kept: false,
            carried: false,
            saved: [None; 2],
            // A fresh terminal: the cursor at the start of an empty screen's first row.
            first_column: true,
            lines: 0,
            settled: 0,
            drawn: 0,
            waiting: VecDeque::new(),
            stands: None,
            fed: false,
            empty: false,
        }
    }

    /// Everything hidden is as a fresh terminal has it.
    fn fresh(&self) -> bool {
        let modes = self.alternate || self.region != [false; 2] || self.origin || self.no_wrap || self.reverse_wrap || self.insert;
        self.attrs.plain() && !modes && self.plain_sets() && !self.link && !self.unread && !self.kept
    }

    fn plain_sets(&self) -> bool {
        self.sets == [false; 4] && self.shifted == 0
    }

    /// The input up to `at` is read: a cut there, where it ended in a line feed and one may be.
    fn line_end(&mut self, at: usize) {
        if std::mem::take(&mut self.fed) && self.first_column && self.settled >= self.rows && self.fresh() {
            self.waiting.push_back(Place { at, since: self.lines, look: self.look });
        }
    }

    /// Something that may scroll the screen itself, or erases its scrollback.
    fn scrolls(&mut self) {
        if !self.alternate {
            self.drawn = self.drawn.saturating_add(1);
        }
    }

    /// The cursor went down a line, or the screen up one.
    fn line(&mut self) {
        self.scrolls();
        self.lines = self.lines.saturating_add(1);
        self.settled = self.settled.saturating_add(1).min(self.rows);
        while self.waiting.front().is_some_and(|place| self.lines.saturating_sub(place.since) >= self.rows) {
            self.stands = self.waiting.pop_front();
        }
    }

    /// The cursor moved otherwise than along its line or down by a line feed, or the screen changed past its
    /// line: the cursor may not be on the last row, and no cut that waits has taken the screen.
    fn moved(&mut self) {
        self.settled = 0;
        self.first_column = false;
        self.waiting.clear();
    }

    /// Something this doesn't follow.
    fn lost(&mut self) {
        self.unread = true;
        self.moved();
    }

    /// The cursor saved.
    fn save(&mut self) {
        let slot = Saved { at: self.lines, drawn: self.drawn, attrs: self.attrs, plain_sets: self.plain_sets() };
        if let Some(saved) = self.saved.get_mut(usize::from(self.alternate)) {
            *saved = Some(slot);
        }
    }

    /// The cursor restored, as xterm.js's `restoreCursor`: its place, the attributes' two words and the
    /// character set in use.
    fn restored(&mut self) {
        let slot = self.saved.get(usize::from(self.alternate)).copied().flatten();
        // One saved before a cut isn't the part's to restore; nor is one the screen itself may have scrolled
        // under since, or had its scrollback erased: its row is counted from the top of all that's drawn.
        if slot.is_some_and(|saved| self.stands.is_some_and(|cut| saved.at < cut.since) || (!self.alternate && saved.drawn != self.drawn)) {
            self.stands = None;
        }
        // Both plain, they stay so; else what's left of the two isn't followed until they're reset.
        if !(slot.is_none_or(|saved| saved.attrs.plain()) && self.attrs.plain()) {
            self.attrs.unread = true;
        }
        if !(slot.is_none_or(|saved| saved.plain_sets) && self.plain_sets()) {
            self.unread = true;
        }
        self.moved();
    }

    /// The alternate screen entered or left: it's emptied as it's left, its scroll region with it, and the
    /// cursor is wherever the other screen's was.
    fn screen(&mut self, alternate: bool) {
        if self.alternate && !alternate {
            self.region = [self.region.first().copied().unwrap_or(false), false];
        }
        self.alternate = alternate;
        self.moved();
    }

    /// `ESC c`: a fresh terminal. A cut that waited stands: both screens are empty from here.
    fn reset(&mut self) {
        let stands = self.waiting.pop_back().or(self.stands);
        // What the page's terminal keeps through it: whether the cursor shows and its blink as an option, and
        // what a full reset isn't known to put back.
        let look = Look { hidden: self.look.hidden, blink_option: self.look.blink_option, ..Look::default() };
        *self = Self { lines: self.lines, stands, look, kept: self.kept, carried: self.carried, ..Self::new(self.rows) };
    }

    /// `CSI Pt ; Pb r`, as xterm.js's `setScrollRegion`: a bottom of none, or past the screen, is its last
    /// row; a region is set only where its bottom is below its top.
    fn scroll_region(&mut self, params: &Params) {
        let mut values = params.iter().map(|param| usize::from(param.first().copied().unwrap_or(0)));
        let top = values.next().filter(|top| *top != 0).unwrap_or(1);
        let bottom = values.next().filter(|bottom| *bottom != 0 && *bottom <= self.rows).unwrap_or(self.rows);
        if bottom > top
            && let Some(region) = self.region.get_mut(usize::from(self.alternate))
        {
            *region = !(top == 1 && bottom == self.rows);
        }
        self.moved();
    }

    /// `CSI Pm h` and `l`.
    fn modes(&mut self, params: &Params, set: bool) {
        for param in params.iter() {
            match param.first() {
                Some(4) => self.insert = set,
                Some(20) => self.carried = true,
                _ => {}
            }
        }
    }

    /// `CSI ? Pm h` and `l`, as xterm.js's `setModePrivate` and `resetModePrivate`. Those that only change
    /// what input is sent (the keys' and the mouse's modes, focus, paste) and one it doesn't know change
    /// nothing here.
    fn private_modes(&mut self, params: &Params, set: bool) {
        for param in params.iter() {
            match param.first() {
                Some(2) if set => self.sets = [false; 4],
                Some(6) => {
                    self.origin = set;
                    self.moved();
                }
                Some(7) => self.no_wrap = !set,
                Some(12) => self.look.blink_option = Some(set),
                Some(25) => self.look.hidden = Some(!set),
                Some(45) => self.reverse_wrap = set,
                Some(47 | 1047) => self.screen(set),
                Some(1048) if set => self.save(),
                Some(1048) => self.restored(),
                Some(1049) if set => {
                    self.save();
                    self.screen(true);
                }
                Some(1049) => {
                    self.screen(false);
                    self.restored();
                }
                _ => {}
            }
        }
    }

    /// `OSC 8 ; params ; uri`, as xterm.js's `setHyperlink`: a link opens with a URI, and closes with neither
    /// a URI nor an id.
    fn hyperlink(&mut self, rest: &[&[u8]]) {
        let [id, uri @ ..] = rest else { return };
        if uri.is_empty() {
            return;
        }
        if uri.len() > 1 || uri.first().is_some_and(|uri| !uri.is_empty()) {
            self.link = true;
        } else if id.trim_ascii().is_empty() {
            self.link = false;
        }
    }
}

/// The first parameter's value, 0 where there's none.
fn first(params: &Params) -> u16 {
    params.iter().next().and_then(|param| param.first().copied()).unwrap_or(0)
}

impl Perform for Scan {
    fn print(&mut self, _: char) {
        self.fed = false;
        self.first_column = false;
        self.scrolls();
    }

    fn execute(&mut self, byte: u8) {
        self.fed = false;
        match byte {
            b'\n' => {
                self.line();
                self.fed = true;
            }
            // A vertical tab and a form feed are line feeds.
            0x0b | 0x0c => self.line(),
            b'\r' => self.first_column = true,
            b'\t' => self.first_column = false,
            // With reverse wraparound a backspace in the first column goes up a line.
            0x08 if self.reverse_wrap => self.moved(),
            0x0e => self.shifted = 1,
            0x0f => self.shifted = 0,
            // The C1 controls: the page's parser begins sequences at some, and takes them only as
            // characters, where this one takes a lone byte too.
            0x80..=0x9f => self.lost(),
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        self.fed = false;
        let empty = std::mem::take(&mut self.empty);
        if ignore {
            return self.lost();
        }
        match (intermediates, action) {
            ([], 'm') => self.attrs.sgr(params, empty),
            // Along the line: the cursor forward, by tab stops.
            ([], 'C' | 'a' | 'I' | 'Z') => self.first_column = false,
            // A character repeated: printed.
            ([], 'b') => {
                self.first_column = false;
                self.scrolls();
            }
            // To a column: the first is 1, or none given.
            ([], 'G' | '`') => self.first_column = first(params) <= 1,
            // The display erased: its lines scrolled out of view (3) are on no screen.
            ([] | [b'?'], 'J') if first(params) == 3 => self.scrolls(),
            ([] | [b'?'], 'J') => self.moved(),
            // Up, down, to a row; lines put in, taken out, scrolled; columns put in, taken out, scrolled.
            ([], 'A' | 'B' | 'E' | 'F' | 'H' | 'f' | 'd' | 'e' | 'L' | 'M' | 'S' | 'T') | ([b' '], '@' | 'A') | ([b'\''], '}' | '~') => self.moved(),
            ([], 'r') => self.scroll_region(params),
            ([], 's') => self.save(),
            ([], 'u') => self.restored(),
            ([], 'h') => self.modes(params, true),
            ([], 'l') => self.modes(params, false),
            ([b'?'], 'h') => self.private_modes(params, true),
            ([b'?'], 'l') => self.private_modes(params, false),
            ([b' '], 'q') => self.look.shaped(first(params)),
            // A tab stop cleared, a cell protected.
            ([], 'g') | ([b'"'], 'q') => self.unread = true,
            // A soft reset.
            ([b'!'], 'p') => self.lost(),
            // What's left is along the line and on it (the cursor back, the line and its characters erased,
            // put in and taken out), asked of the terminal and answered, or nothing to the page's.
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        self.fed = false;
        if ignore {
            return self.lost();
        }
        match (intermediates, byte) {
            ([], b'7') => self.save(),
            ([], b'8') => self.restored(),
            // An index is a line feed; a next line returns the carriage too.
            ([], b'D') => self.line(),
            ([], b'E') => {
                self.first_column = true;
                self.line();
            }
            ([], b'H') => self.unread = true,
            // A reverse index; the screen filled with its alignment pattern.
            ([], b'M') | ([b'#'], b'8') => self.moved(),
            ([], b'c') => self.reset(),
            ([], b'~') => self.shifted = 1,
            ([], b'n' | b'}') => self.shifted = 2,
            ([], b'o' | b'|') => self.shifted = 3,
            ([b'%'], b'@' | b'G') => {
                self.shifted = 0;
                if let Some(set) = self.sets.first_mut() {
                    *set = false;
                }
            }
            ([which @ (b'(' | b')' | b'*' | b'+' | b'-' | b'.')], flag) if flag == b'B' || DRAWN_SETS.contains(&flag) => {
                let level = match which {
                    b'(' => 0,
                    b')' | b'-' => 1,
                    b'*' | b'.' => 2,
                    _ => 3,
                };
                if let Some(set) = self.sets.get_mut(level) {
                    *set = flag != b'B';
                }
            }
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _: bool) {
        self.fed = false;
        let [number, rest @ ..] = params else { return };
        // A command's number is digits and nothing else: with a sign, say, the page's terminal has no such command.
        match std::str::from_utf8(number).ok().filter(|number| number.bytes().all(|b| b.is_ascii_digit())).and_then(|number| number.parse::<u16>().ok()) {
            // A colour of the terminal's own (the text's, the background's, the cursor's; one of its
            // palette's, each after its number): one asked for (`?`) isn't set.
            Some(10..=12) if rest.iter().any(|value| *value != b"?") => self.kept = true,
            Some(4) if rest.chunks(2).any(|pair| pair.get(1).is_some_and(|value| *value != b"?")) => self.kept = true,
            Some(8) => self.hyperlink(rest),
            _ => {}
        }
    }

    fn hook(&mut self, _: &Params, _: &[u8], _: bool, _: char) {
        self.fed = false;
    }

    fn put(&mut self, _: u8) {
        self.fed = false;
    }

    fn unhook(&mut self) {
        self.fed = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROWS: u16 = 6;

    /// What a fresh terminal of `rows` by `cols` shows once it has drawn `bytes`, as the server's own emulator
    /// reads them: every cell and its attributes, the cursor, and the modes input goes by.
    fn drawn(bytes: &[u8], rows: u16, cols: u16) -> Vec<u8> {
        let mut fresh = vt100::Parser::new(rows, cols, 0);
        fresh.process(bytes);
        fresh.screen().state_formatted()
    }

    /// `n` lines a shell might print, each ending as a terminal's lines do.
    fn lines(n: usize) -> Vec<u8> {
        (0..n).flat_map(|i| format!("line {i} of a build\r\n").into_bytes()).collect()
    }

    /// Where `body`'s part begins, counted in the lines before it; none where it has no part.
    fn cut(body: &[u8]) -> Option<usize> {
        newest(body, ROWS).map(|cut| body.get(..cut.at).unwrap_or_default().iter().filter(|b| **b == b'\n').count())
    }

    /// W21-R02: a shell's plain output is cut as many lines from its end as the screen has rows: its latest
    /// lines, which take the screen.
    #[test]
    fn w21_r02_plain_output_is_cut_a_screen_from_its_end() {
        assert_eq!(cut(&lines(40)), Some(40 - usize::from(ROWS)));
        // With a prompt after its last line too.
        assert_eq!(cut(&[lines(40), b"$ cargo te".to_vec()].concat()), Some(40 - usize::from(ROWS)));
        // The part is the bytes from the cut, and begins at a line's start.
        let body = lines(40);
        let at = newest(&body, ROWS).map(|cut| cut.at).unwrap_or_default();
        assert!(body.get(at..).is_some_and(|part| part.starts_with(b"line 34 of a build\r\n")), "cut at {at}");
        // Fewer than two screens of lines: none to put the cursor on the last row and then take the screen.
        assert_eq!(cut(&lines(2 * usize::from(ROWS) - 1)), None);
        assert_eq!(cut(&lines(2 * usize::from(ROWS))), Some(usize::from(ROWS)));
        assert_eq!(cut(b""), None);
        // Lines that end in a line feed alone leave the cursor wherever it was: no cut is after one.
        let bare: Vec<u8> = (0..40).flat_map(|i| format!("line {i}\n").into_bytes()).collect();
        assert_eq!(cut(&bare), None);
    }

    /// W21-R02: a line feed inside a sequence is no place for a cut: what follows it is the rest of the
    /// sequence, which a part would print.
    #[test]
    fn w21_r02_no_cut_is_inside_a_sequence() {
        let rows = usize::from(ROWS);
        // Each line ends inside a sequence that sets a colour and is over on the next: none is cut after.
        let split: Vec<u8> = (0..40).flat_map(|i| format!("\x1b[0mline {i}\r\x1b[3\n1m").into_bytes()).collect();
        assert_eq!(cut(&split), None);
        // One such line among plain ones: the cut that would be after it isn't, and those around it are.
        let one = |at: usize| -> Vec<u8> { (0..40).flat_map(|i| if i == at { b"odd\r\x1b[3\n9m".to_vec() } else { format!("line {i}\r\n").into_bytes() }).collect() };
        assert_eq!(cut(&one(40 - rows - 1)), Some(40 - rows - 1), "the cut a screen from the end is after the odd line: the one before it stands");
        assert_eq!(cut(&one(10)), Some(40 - rows));
        // Where the page's parser and this one part ways, nothing after is known as the page has it: a byte
        // past ASCII inside a sequence (the page gives the sequence up there), a C1 control as a character
        // inside a string (the page ends the string there).
        for parted in ["\x1b[3é1m", "\x1b[\u{9b}0m", "\x1bé(B", "\x1b]0;title\u{9c}", "\x1bP1$r\u{85}\x1b\\", "\x1b_app\u{9c}"] {
            let body = [lines(20), parted.as_bytes().to_vec(), lines(40)].concat();
            assert_eq!(cut(&body), Some(20 - rows), "{}", parted.escape_debug());
            assert_eq!(cut(&[body, b"\x1bc".to_vec(), lines(40)].concat()), Some(100 - rows), "{}: after a full reset", parted.escape_debug());
        }
        // Past ASCII in a string is the string's: a title in another script holds no cut back.
        let title: Vec<u8> = (0..40).flat_map(|i| format!("\x1b]0;名前 é\x07line {i}\r\n").into_bytes()).collect();
        assert_eq!(cut(&title), Some(40 - rows));
        // An escape that ends a string begins a sequence too: a line feed inside that one is inside it.
        let ends: Vec<u8> = (0..40).flat_map(|i| format!("line {i}\r\x1b]0;title\x1b\n(B").into_bytes()).collect();
        assert_eq!(cut(&ends), None);
        // A title that holds a line feed is a title; a sequence given up, or left unfinished before text, is over.
        let within: [&[u8]; 4] = [b"\x1b]0;a\ntitle\x07", b"\x1b[3\x18", b"\x1b[?1;?h", b"\x1bP1$r\n\x1b\\"];
        for sequence in within {
            let body: Vec<u8> = (0..40).flat_map(|i| [sequence.to_vec(), format!("line {i}\r\n").into_bytes()].concat()).collect();
            assert!(cut(&body).is_some_and(|at| at >= 40 - rows), "{}: {:?}", String::from_utf8_lossy(sequence).escape_debug(), cut(&body));
        }
    }

    /// W21-R02: what a terminal keeps hidden holds a cut back while it's other than a fresh terminal's, and
    /// no longer once it's put back.
    #[test]
    fn w21_r02_no_cut_is_where_something_hidden_isnt_a_fresh_terminals() {
        // What sets it, and what puts it back.
        let hidden: [(&str, &[u8], &[u8]); 27] = [
            ("a colour", b"\x1b[31m", b"\x1b[39m"),
            ("a background", b"\x1b[48;5;238m", b"\x1b[49m"),
            ("a colour by its parts", b"\x1b[38;2;10;200;120m", b"\x1b[0m"),
            ("a colour whose values ride on it", b"\x1b[38:5:196m", b"\x1b[m"),
            ("bold", b"\x1b[1m", b"\x1b[22m"),
            ("dim and bold", b"\x1b[1;2m", b"\x1b[22m"),
            ("italic", b"\x1b[3m", b"\x1b[23m"),
            ("an underline", b"\x1b[4m", b"\x1b[24m"),
            ("a curly underline", b"\x1b[4:3m", b"\x1b[4:0m"),
            ("an underline's colour", b"\x1b[58:5:196m", b"\x1b[59m"),
            ("inverse", b"\x1b[7m", b"\x1b[27m"),
            ("struck through", b"\x1b[9m", b"\x1b[29m"),
            ("overlined", b"\x1b[53m", b"\x1b[55m"),
            ("blinking", b"\x1b[5m", b"\x1b[25m"),
            ("a colour sequence cut short", b"\x1b[38;5m", b"\x1b[0m"),
            ("bold with something riding on it", b"\x1b[1:2:3m", b"\x1b[22:1m"),
            ("an underline with its style left empty", b"\x1b[4:m", b"\x1b[4:0m"),
            ("a colour with its space left empty", b"\x1b[38:2::10:200:120m", b"\x1b[39m"),
            ("a line-drawing set", b"\x1b(0", b"\x1b(B"),
            ("the second set shifted in", b"\x0e", b"\x0f"),
            ("a hyperlink", b"\x1b]8;;https://kiln.example\x1b\\", b"\x1b]8;;\x1b\\"),
            ("autowrap off", b"\x1b[?7l", b"\x1b[?7h"),
            ("insert mode", b"\x1b[4h", b"\x1b[4l"),
            ("reverse wraparound", b"\x1b[?45h", b"\x1b[?45l"),
            ("a tab stop set", b"\x1bH", b"\x1bc"),
            ("a tab stop cleared", b"\x1b[3g", b"\x1bc"),
            ("a protected cell", b"\x1b[1\"q", b"\x1bc"),
        ];
        for (what, set, back) in hidden {
            // The cuts that waited as it came stand: what comes after one is drawn the same on both.
            let held = [lines(20), set.to_vec(), lines(40)].concat();
            assert_eq!(cut(&held), Some(20), "{what}: the latest cut is the last before it");
            let freed = [held, back.to_vec(), b"\r\n".to_vec(), lines(40)].concat();
            assert_eq!(cut(&freed), Some(101 - usize::from(ROWS)), "{what}: a cut again once it's put back");
        }
        // A hyperlink isn't closed by a command that isn't one.
        let unclosed = [lines(20), b"\x1b]8;;https://kiln.example\x07".to_vec(), lines(20), b"\x1b]+8;;\x07".to_vec(), lines(40)].concat();
        assert_eq!(cut(&unclosed), Some(20));
        // A control this parser reads otherwise than the page's: nothing after it is known until a full reset.
        let control = [lines(20), "\u{9b}".as_bytes().to_vec(), lines(40)].concat();
        assert_eq!(cut(&control), Some(20 - usize::from(ROWS)));
        assert_eq!(cut(&[control, b"\x1bc".to_vec(), lines(40)].concat()), Some(100 - usize::from(ROWS)));
        // So with a sequence of more parameters than a parser keeps.
        let many = format!("\x1b[{}m", "1;".repeat(40)).into_bytes();
        assert_eq!(cut(&[lines(20), many, lines(40)].concat()), Some(20 - usize::from(ROWS)));
        // A colour of the terminal's own, once set, is never known to be put back.
        for set in [b"\x1b]11;#101010\x07".as_slice(), b"\x1b]4;1;rgb:ff/00/00\x07", b"\x1b]10;red;?\x07"] {
            let body = [lines(20), set.to_vec(), lines(40), b"\x1bc".to_vec(), lines(40)].concat();
            assert_eq!(cut(&body), Some(20), "{}", String::from_utf8_lossy(set));
        }
        // Whether a line feed returns the carriage is the page's terminal's to keep through a reset: read in
        // a part, it would be how the replay after it is drawn. A replay that says it, anywhere, has no part.
        for said in [b"\x1b[20h".as_slice(), b"\x1b[20l", b"\x1b[4;20h"] {
            assert_eq!(cut(&[lines(40), said.to_vec()].concat()), None, "{}: said after a cut", String::from_utf8_lossy(said).escape_debug());
            assert_eq!(cut(&[said.to_vec(), b"\x1bc".to_vec(), lines(40)].concat()), None, "{}: said before one", String::from_utf8_lossy(said).escape_debug());
        }
    }

    /// W21-R02: what draws nothing the page keeps, or only changes what input is sent, holds no cut back.
    #[test]
    fn w21_r02_what_changes_nothing_drawn_holds_no_cut_back() {
        let harmless: [(&str, &[u8]); 23] = [
            ("the directory", b"\x1b]7;file://host/Users/ada\x1b\\"),
            ("a prompt's mark", b"\x1b]133;A\x07"),
            ("a title", b"\x1b]0;build\x07"),
            ("the background asked for", b"\x1b]11;?\x07"),
            ("a palette colour asked for", b"\x1b]4;1;?\x07"),
            ("a hyperlink opened and closed", b"\x1b]8;;https://kiln.example\x1b\\link\x1b]8;;\x1b\\"),
            ("a command that isn't one", b"\x1b]+8;;https://kiln.example\x07\x1b]+11;#102030\x07"),
            ("US-ASCII designated", b"\x1b(B"),
            ("bash's meta mode", b"\x1b[?1034h"),
            ("bracketed paste", b"\x1b[?2004h"),
            ("the mouse reported", b"\x1b[?1000;1006h"),
            ("application keys", b"\x1b[?1h\x1b="),
            ("focus reports", b"\x1b[?1004h"),
            ("synchronized output", b"\x1b[?2026h\x1b[?2026l"),
            ("xterm's key modifiers", b"\x1b[>4;2m"),
            ("kitty's keyboard", b"\x1b[>1u\x1b[<u\x1b[?u"),
            ("the cursor's place asked for", b"\x1b[6n"),
            ("device attributes asked for", b"\x1b[c\x1b[>c"),
            ("saved lines erased", b"\x1b[3J"),
            ("the title stack", b"\x1b[22;0t\x1b[23;0t"),
            ("a device control string", b"\x1bP+q544e\x1b\\"),
            ("an attribute the page doesn't know", b"\x1b[6m"),
            ("a bell and a null", b"\x07\x00"),
        ];
        for (what, sequence) in harmless {
            let body = [lines(20), sequence.to_vec(), lines(20)].concat();
            assert_eq!(cut(&body), Some(40 - usize::from(ROWS)), "{what}");
        }
        // Along the line, and on it: a progress bar drawn over itself, a right-hand prompt, a tab.
        let along: [&[u8]; 8] = [b"\rcompiled 3 of 9\x1b[K", b"\x1b[30Cright\x1b[35D", b"\x1b[1Ghere", b"a\tb", b"abc\x08\x08x", b"ab\x1b[2Pc\x1b[3@", b"x\x1b[20b", b"\x1b[2Kgone\x1b[5X"];
        for sequence in along {
            let body: Vec<u8> = (0..40).flat_map(|i| [format!("line {i} ").into_bytes(), sequence.to_vec(), b"\r\n".to_vec()].concat()).collect();
            assert_eq!(cut(&body), Some(40 - usize::from(ROWS)), "{}", String::from_utf8_lossy(sequence));
        }
    }

    /// W21-R02: after the cursor moves otherwise than along its line or down by a line feed, a cut waits for
    /// a screen of lines (the cursor is then on the last row, empty) and stands after a screen more (they
    /// have taken the screen): until then the latest that stands is the one from before.
    #[test]
    fn w21_r02_a_cut_waits_two_screens_after_the_cursor_moves() {
        let moves: [(&str, &[u8]); 23] = [
            ("up", b"\x1b[1A"),
            ("down", b"\x1b[1B"),
            ("to the line before", b"\x1b[1F"),
            ("to the line after", b"\x1b[1E"),
            ("to a place", b"\x1b[3;4H"),
            ("to a place, the other way", b"\x1b[3;4f"),
            ("to a row", b"\x1b[2d"),
            ("down by rows", b"\x1b[1e"),
            ("a reverse index", b"\x1bM"),
            ("the display erased", b"\x1b[2J"),
            ("the display erased below", b"\x1b[J"),
            ("a line put in", b"\x1b[1L"),
            ("a line taken out", b"\x1b[1M"),
            ("scrolled up", b"\x1b[1S"),
            ("scrolled down", b"\x1b[1T"),
            ("a scroll region", b"\x1b[1;4r\x1b[r"),
            ("origin mode", b"\x1b[?6h\x1b[?6l"),
            ("the alternate screen", b"\x1b[?1049h\x1b[H\x1b[2Jfull screen\x1b[?1049l"),
            ("the alternate screen, the old way", b"\x1b[?47hfull\x1b[?47l"),
            ("the cursor saved and restored", b"\x1b7\x1b8"),
            ("the cursor saved and restored, the other way", b"\x1b[s\x1b[u"),
            ("the alignment pattern", b"\x1b#8"),
            ("a column put in", b"\x1b[1'}"),
        ];
        let rows = usize::from(ROWS);
        for (what, sequence) in moves {
            let before = [lines(20), sequence.to_vec()].concat();
            // A line short of two screens after it: none stands but the one from before.
            let short = [before.clone(), lines(2 * rows - 1)].concat();
            assert_eq!(cut(&short), Some(20 - rows), "{what}: {} lines after it", 2 * rows - 1);
            // Two screens: one does, a screen from the end.
            let enough = [before, lines(2 * rows)].concat();
            assert_eq!(cut(&enough), Some(20 + rows), "{what}: {} lines after it", 2 * rows);
        }
        // A screen that redraws lines above its last every few lines never has one.
        let redrawn: Vec<u8> = (0..200).flat_map(|i| format!("pulling layer {i}\r\n\x1b[1A\x1b[2Kpulled layer {i}\r\n").into_bytes()).collect();
        assert_eq!(cut(&redrawn), None);
        // Whatever comes after a cut's lines have taken the screen, it stands: both terminals are in one state.
        let after = [lines(40), b"\x1b[2;3H\x1b[31mover\x1b[1A\x1b[?1049hfull screen\x1b[?1049l\x1b(0\x1b[3J".to_vec()].concat();
        assert_eq!(cut(&after), Some(40 - rows));
    }

    /// W21-R02: a cursor restored in a part that was saved before it is let go of; one saved in the part is
    /// the part's own.
    #[test]
    fn w21_r02_a_cursor_saved_before_a_cut_isnt_the_parts_to_restore() {
        let saved_before = [b"\x1b7".to_vec(), lines(40), b"\x1b8".to_vec()].concat();
        assert_eq!(cut(&saved_before), None, "the cut stood until the restore");
        assert_eq!(cut(&[b"\x1b[s".to_vec(), lines(40), b"\x1b[u".to_vec()].concat()), None);
        assert_eq!(cut(&[b"\x1b[?1048h".to_vec(), lines(40), b"\x1b[?1048l".to_vec()].concat()), None);
        // Saved after the cut's lines took the screen and restored with nothing drawn between, it's restored
        // the same on both.
        let saved_after = [lines(40), b"\x1b7\x1b[1;31m\x1b[5C\x1b8".to_vec()].concat();
        assert_eq!(cut(&saved_after), Some(40 - usize::from(ROWS)));
        // With the screen scrolled under it between, or its scrollback erased, or a character printed that may
        // have wrapped, its row is another on the page's terminal: counted from the top of all it has drawn.
        for between in [b"\r\n".as_slice(), b"\x1b[3J", b"x", b"x\x1b[3b"] {
            let scrolled = [lines(40), b"\x1b7".to_vec(), between.to_vec(), b"\x1b8".to_vec()].concat();
            assert_eq!(cut(&scrolled), None, "{}", String::from_utf8_lossy(between).escape_debug());
        }
        // A full-screen program saves as it starts and restores as it ends, both after the cut, and what it
        // draws between is on the alternate screen: the screen itself hasn't scrolled.
        let program = [lines(40), b"\x1b[?1049h\x1b[2Jfull screen\r\nline two\r\n\x1b[3J\x1b[?1049l".to_vec()].concat();
        assert_eq!(cut(&program), Some(40 - usize::from(ROWS)));
        // One never saved is restored to a fresh terminal's, on both.
        assert_eq!(cut(&[lines(40), b"\x1b8".to_vec()].concat()), Some(40 - usize::from(ROWS)));
        // One saved on the alternate screen isn't the one the screen restores.
        let other = [b"\x1b7".to_vec(), lines(40), b"\x1b[?47h\x1b7\x1b[?47l\x1b8".to_vec()].concat();
        assert_eq!(cut(&other), None);
    }

    /// W21-R02: a full reset makes a fresh terminal of both: what was hidden before it is gone, and a cut that
    /// waited stands.
    #[test]
    fn w21_r02_a_full_reset_is_a_fresh_terminal() {
        let rows = usize::from(ROWS);
        // A cut two lines old as the reset comes stands; and after it cuts are a screen from the end again.
        let reset = [lines(20), b"\x1b[2A".to_vec(), lines(rows + 2), b"\x1bc".to_vec()].concat();
        assert_eq!(cut(&reset), Some(20 + rows + 2));
        // What was hidden is a fresh terminal's again: a cut a screen of lines after it, once a screen more have come.
        let after = [lines(3), b"\x1b[31m\x1b(0\x1b[2;4r".to_vec(), b"\x1bc".to_vec(), lines(2 * rows)].concat();
        assert_eq!(cut(&after), Some(3 + rows));
    }

    /// W21-R02: the cursor's look at a cut is written ahead of its part, as the page's terminal would have it.
    #[test]
    fn w21_r02_a_part_begins_with_the_cursors_look() {
        let look = |set: &[u8]| newest(&[set.to_vec(), lines(40)].concat(), ROWS).map(|cut| String::from_utf8_lossy(&cut.look).into_owned());
        assert_eq!(look(b"").as_deref(), Some(""));
        assert_eq!(look(b"\x1b[?25l").as_deref(), Some("\x1b[?25l"));
        // Once said either way it's said again: the page's terminal keeps it through the reset a part begins with.
        assert_eq!(look(b"\x1b[?25l\x1b[?25h").as_deref(), Some("\x1b[?25h"));
        // A shape and its blink: a steady bar, a blinking block, the terminal's own again.
        assert_eq!(look(b"\x1b[6 q").as_deref(), Some("\x1b[6 q"));
        assert_eq!(look(b"\x1b[2 q\x1b[1 q").as_deref(), Some("\x1b[1 q"));
        assert_eq!(look(b"\x1b[4 q\x1b[0 q").as_deref(), Some(""));
        assert_eq!(look(b"\x1b[4 q\x1b[ q").as_deref(), Some(""), "none given is 0");
        // A parameter past the shapes sets the blink alone.
        assert_eq!(look(b"\x1b[8 q").as_deref(), Some("\x1b[8 q"));
        assert_eq!(look(b"\x1b[3 q\x1b[8 q").as_deref(), Some("\x1b[4 q"));
        // The blink as an option; with a shape, and hidden.
        assert_eq!(look(b"\x1b[?12l").as_deref(), Some("\x1b[?12l"));
        assert_eq!(look(b"\x1b[?12l\x1b[5 q\x1b[?25l\x1bc").as_deref(), Some("\x1b[?12l\x1b[?25l"), "a full reset puts the shape back, and neither of the others");
        assert_eq!(look(b"\x1b[?12h\x1b[5 q\x1b[?25l").as_deref(), Some("\x1b[?12h\x1b[5 q\x1b[?25l"));
        // As it is at the cut, not as it was set after.
        let later = newest(&[lines(40), b"\x1b[?25l".to_vec()].concat(), ROWS).map(|cut| cut.look);
        assert_eq!(later.as_deref(), Some(b"".as_slice()));
    }

    /// W21-R02: what goes ahead is the reset, the cursor's look and its place on the last row, the replay's
    /// output from its cut and the modes the replay ends with; only of a replay more than twice its size. The
    /// server's own emulator draws both to one screen.
    #[test]
    fn w21_r02_a_part_is_its_replays_newest_lines_and_worth_sending() {
        let (rows, cols) = (ROWS, 40);
        let replay = |output: &[u8]| [RESET, output, b"\x1b[?2004h"].concat();
        let part = |output: &[u8]| ahead(&replay(output), RESET.len()..RESET.len() + output.len(), rows);
        let output = [b"\x1b[?25l".to_vec(), lines(40), b"$ cargo te".to_vec()].concat();
        let sent = part(&output).unwrap_or_default();
        let newest: Vec<u8> = (34..40).flat_map(|i| format!("line {i} of a build\r\n").into_bytes()).collect();
        assert_eq!(String::from_utf8_lossy(&sent), String::from_utf8_lossy(&[RESET, b"\x1b[?25l", b"\x1b[6;1H", &newest, b"$ cargo te", b"\x1b[?2004h"].concat()));
        assert_eq!(drawn(&sent, rows, cols), drawn(&replay(&output), rows, cols));
        // A replay no more than twice its part gains less than it costs.
        let at_most = |n: usize| part(&lines(n)).is_some();
        assert!(!at_most(usize::from(rows) * 2) && at_most(usize::from(rows) * 2 + 2), "{:?}", (6..20).map(at_most).collect::<Vec<_>>());
        // Where its output isn't where it's said to be, none.
        assert_eq!(ahead(&replay(&output), 0..10_000, rows), None);
    }

    /// The page's terminal is the xterm.js whose handlers this was read against: with another, read its
    /// `InputHandler.ts` against this module and compare the two drawings in a browser again, then move this.
    #[test]
    fn w21_r02_the_pages_terminal_is_the_one_this_was_read_against() -> anyhow::Result<()> {
        let lock = std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ui/package-lock.json"))?;
        let lock: serde_json::Value = serde_json::from_str(&lock)?;
        assert_eq!(lock.pointer("/packages/node_modules~1@xterm~1xterm/version").and_then(serde_json::Value::as_str), Some("6.0.0"));
        Ok(())
    }
}
