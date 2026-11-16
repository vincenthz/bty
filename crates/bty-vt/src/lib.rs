//! The terminal bty keeps for each program, on Rio's VT core.
//!
//! `rio-vt` parses and holds the screen. Around it this crate adds what a
//! runtime that relays bytes to other terminals needs and rio keeps private
//! or leaves to its renderer: where escape sequences end (`scan`), repaints
//! and snapshots (`format`), the primary screen while the alternate one is
//! shown, answers that carry the viewer's facts (colors, cell size, version),
//! DECSTR and color-scheme reports (mode 2031), and key encoding (`keys`).

mod format;
mod keys;
mod scan;

pub use keys::{ALT, CTRL, Key, SHIFT, SUPER};

use rio_vt::ansi::CursorShape;
use rio_vt::config::colors::ColorRgb;
use rio_vt::crosswords::grid::{Dimensions, Grid};
use rio_vt::crosswords::square::Square;
use rio_vt::crosswords::{Crosswords, CrosswordsSize, Mode};
use rio_vt::event::{EventListener, ProgramState, ProgramStatusEvent, RioEvent, WindowId};
use rio_vt::performer::handler::Processor;
use scan::{Scanner, Seq};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

/// Rio counts scrollback in lines; bty's setting is in bytes.
const BYTES_PER_LINE: usize = 1024;
const MAX_LINES: usize = 1 << 20;
/// Cell size reported in pixels until a viewer tells us its own.
const DEFAULT_CELL: (u32, u32) = (10, 20);
/// Colors answered until a viewer tells us its own.
const DEFAULT_FG: Rgb = [0xff, 0xff, 0xff];
const DEFAULT_BG: Rgb = [0x28, 0x2c, 0x34];
const SNAPSHOT_MAGIC: &[u8] = b"bty-vt snapshot 1\n";
/// Device attributes: VT220 with ANSI color (`CSI ? 62 ; 22 c`), and a VT220 at version 10.
const PRIMARY_ATTRIBUTES: &str = "\x1b[?62;22c";
const SECONDARY_ATTRIBUTES: &str = "\x1b[>1;10;0c";

pub type Rgb = [u8; 3];

/// One OSC 7501 report, exactly as the program sent it (strings decoded).
#[derive(Clone, Debug, PartialEq)]
pub struct StatusReport {
    /// idle, working, done, blocked, error, or clear (remove records).
    pub state: &'static str,
    /// permission, question or auth, only when blocked and said.
    pub kind: Option<&'static str>,
    /// 0..=100, only for working and blocked when sent.
    pub progress: Option<u8>,
    /// Empty for the root record.
    pub id: String,
    pub app: String,
    pub title: String,
    pub message: String,
}

type ColorReply = Arc<dyn Fn(ColorRgb) -> String + Send + Sync>;

enum Reply {
    Bytes(String),
    /// OSC 4/10/11/12 query: answered once the terminal's colors can be read.
    Color(usize, ColorReply),
}

/// What rio tells us while it parses.
#[derive(Default)]
struct Inbox {
    replies: Vec<Reply>,
    statuses: Vec<StatusReport>,
    title_changed: bool,
}

#[derive(Clone)]
struct Listener(Arc<Mutex<Inbox>>);

impl EventListener for Listener {
    fn send_event(&self, event: RioEvent, _: WindowId) {
        let mut inbox = self.0.lock().unwrap_or_else(|p| p.into_inner());
        match event {
            // XTGETTCAP is the viewer's to answer: its terminfo, not rio's, is what the program meets.
            RioEvent::PtyWrite(_, text) if text.starts_with("\x1bP1+r") || text.starts_with("\x1bP0+r") => {}
            RioEvent::PtyWrite(_, text) => inbox.replies.push(Reply::Bytes(text)),
            RioEvent::ColorRequest(_, index, reply) => inbox.replies.push(Reply::Color(index, reply)),
            RioEvent::Title(..) => inbox.title_changed = true,
            RioEvent::ProgramStatus(ProgramStatusEvent::Report(s)) => inbox.statuses.push(StatusReport {
                state: match s.state {
                    ProgramState::Idle => "idle",
                    ProgramState::Working => "working",
                    ProgramState::Done => "done",
                    ProgramState::Blocked => "blocked",
                    ProgramState::Error => "error",
                    ProgramState::Clear => "clear",
                },
                kind: s.kind.map(|k| match k {
                    rio_vt::event::BlockedKind::Permission => "permission",
                    rio_vt::event::BlockedKind::Question => "question",
                    rio_vt::event::BlockedKind::Auth => "auth",
                }),
                progress: s.progress,
                id: s.id.unwrap_or_default(),
                app: s.app.unwrap_or_default(),
                title: s.title.unwrap_or_default(),
                message: s.msg.unwrap_or_default(),
            }),
            _ => {}
        }
    }

    fn send_event_with_high_priority(&self, event: RioEvent, id: WindowId) {
        self.send_event(event, id);
    }
}

/// One parsed terminal.
pub struct Terminal {
    term: Crosswords<Listener>,
    parser: Processor,
    scan: Scanner,
    inbox: Arc<Mutex<Inbox>>,
    responses: Vec<u8>,
    statuses: Vec<StatusReport>,
    title_changed: bool,
    /// The primary screen as it was when the alternate one took over (rio
    /// keeps its own private), so a repaint can give viewers both.
    primary: Option<Box<Grid<Square>>>,
    /// DECSTBM, 1-based; `None` for the whole screen (rio keeps it private).
    margins: Option<(u16, u16)>,
    /// Mode 2031: report color scheme changes.
    scheme_reports: bool,
    xtversion: String,
    fg: Rgb,
    bg: Rgb,
    cursor: Rgb,
    cell: (u32, u32),
}

fn size(cols: u16, rows: u16) -> CrosswordsSize {
    CrosswordsSize::new((cols as usize).max(rio_vt::crosswords::MIN_COLUMNS), (rows as usize).max(1))
}

fn dark([r, g, b]: Rgb) -> bool {
    (r as u32 * 299 + g as u32 * 587 + b as u32 * 114) / 1000 < 128
}

/// xterm's 256-color palette, for queries about colors the program never set.
fn palette(i: usize) -> Rgb {
    const BASE: [Rgb; 16] = [
        [0, 0, 0],
        [205, 0, 0],
        [0, 205, 0],
        [205, 205, 0],
        [0, 0, 238],
        [205, 0, 205],
        [0, 205, 205],
        [229, 229, 229],
        [127, 127, 127],
        [255, 0, 0],
        [0, 255, 0],
        [255, 255, 0],
        [92, 92, 255],
        [255, 0, 255],
        [0, 255, 255],
        [255, 255, 255],
    ];
    match i {
        0..16 => BASE[i],
        16..232 => {
            let level = |v: usize| if v == 0 { 0 } else { (55 + v * 40) as u8 };
            let v = i - 16;
            [level(v / 36), level(v / 6 % 6), level(v % 6)]
        }
        _ => [(8 + (i.min(255) - 232) * 10) as u8; 3],
    }
}

impl Terminal {
    pub fn new(cols: u16, rows: u16, scrollback_bytes: usize) -> Terminal {
        let inbox = Arc::new(Mutex::new(Inbox::default()));
        let lines = (scrollback_bytes / BYTES_PER_LINE).min(MAX_LINES);
        let listener = Listener(inbox.clone());
        let mut term = Crosswords::new(size(cols, rows), CursorShape::Block, listener, WindowId::from(0), 0, lines);
        term.set_program_status_enabled(true);
        Terminal {
            term,
            parser: Processor::default(),
            scan: Scanner::default(),
            inbox,
            responses: Vec::new(),
            statuses: Vec::new(),
            title_changed: false,
            primary: None,
            margins: None,
            scheme_reports: false,
            xtversion: String::new(),
            fg: DEFAULT_FG,
            bg: DEFAULT_BG,
            cursor: DEFAULT_FG,
            cell: DEFAULT_CELL,
        }
    }

    pub fn write(&mut self, bytes: &[u8]) {
        let mut at = 0;
        while at < bytes.len() {
            let step = self.scan.scan(&bytes[at..]);
            let end = at + step.consumed;
            match step.seq {
                None => self.feed(&bytes[at..end]),
                Some(seq) => {
                    self.feed(&bytes[at..end - 1]);
                    self.handle(seq, bytes[end - 1]);
                }
            }
            at = end;
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.parser.advance(&mut self.term, bytes);
        let inbox = std::mem::take(&mut *self.inbox.lock().unwrap_or_else(|p| p.into_inner()));
        for reply in inbox.replies {
            match reply {
                Reply::Bytes(text) => self.responses.extend(text.as_bytes()),
                Reply::Color(index, reply) => {
                    let [r, g, b] = self.color(index);
                    self.responses.extend(reply(ColorRgb { r, g, b }).as_bytes());
                }
            }
        }
        self.statuses.extend(inbox.statuses);
        self.title_changed |= inbox.title_changed;
    }

    /// A color as the program sees it: what it set, else the viewer's or the palette's.
    fn color(&self, index: usize) -> Rgb {
        if let Some(c) = (index < 269).then(|| self.term.colors[index]).flatten() {
            return c.map(|v| (v.clamp(0.0, 1.0) * 255.0).round() as u8)[..3].try_into().expect("rgb");
        }
        match index {
            256 => self.fg,
            257 => self.bg,
            258 => self.cursor,
            i => palette(i),
        }
    }

    /// Feeds a sequence's final byte, doing what rio doesn't (or does differently).
    fn handle(&mut self, seq: Seq, last: u8) {
        let keep = self.responses.len();
        let ours = match seq {
            Seq::PrivateMode { set, alt, color_scheme } => {
                let saved = (alt && set && !self.alt_screen()).then(|| Box::new(self.term.grid.clone()));
                self.feed(&[last]);
                if let Some(on) = color_scheme {
                    self.scheme_reports = on;
                }
                if !self.alt_screen() {
                    self.primary = None;
                } else if saved.is_some() {
                    self.primary = saved;
                }
                return;
            }
            Seq::Ris => {
                self.feed(&[last]);
                (self.primary, self.margins, self.scheme_reports) = (None, None, false);
                return;
            }
            Seq::SoftReset => {
                self.feed(&[last]);
                self.soft_reset();
                return;
            }
            Seq::Margins(top, bottom) => {
                self.feed(&[last]);
                let rows = self.size().1;
                let (top, bottom) = (top.max(1), if bottom == 0 { rows } else { bottom });
                if top < bottom {
                    let m = (top.min(rows + 1), bottom.min(rows));
                    self.margins = (m != (1, rows)).then_some(m);
                }
                return;
            }
            // Rio scrolls one blank line into history when the screen is already blank.
            Seq::EraseDisplay => {
                if !self.alt_screen() && format::blank_screen(&self.term.grid) {
                    let pos = self.term.grid.cursor.pos;
                    self.feed(b"\x18\x1b[H\x1b[J");
                    self.term.grid.cursor.pos = pos;
                } else {
                    self.feed(&[last]);
                }
                return;
            }
            Seq::Version if self.xtversion.is_empty() => {
                self.feed(&[last]);
                return;
            }
            Seq::Version => format!("\x1bP>|{}\x1b\\", self.xtversion),
            Seq::ColorSchemeQuery => self.scheme_report(),
            Seq::ColorSchemeModeQuery => format!("\x1b[?2031;{}$y", if self.scheme_reports { 1 } else { 2 }),
            Seq::PrimaryAttributes => PRIMARY_ATTRIBUTES.into(),
            Seq::SecondaryAttributes => SECONDARY_ATTRIBUTES.into(),
            Seq::PixelSize => {
                let (cols, rows) = self.size();
                format!("\x1b[4;{};{}t", rows as u32 * self.cell.1, cols as u32 * self.cell.0)
            }
            Seq::CellSize => format!("\x1b[6;{};{}t", self.cell.1, self.cell.0),
        };
        self.feed(&[last]);
        self.responses.truncate(keep);
        self.responses.extend(ours.as_bytes());
    }

    fn scheme_report(&self) -> String {
        format!("\x1b[?997;{}n", if dark(self.bg) { 1 } else { 2 })
    }

    /// DECSTR, which rio doesn't do: pen, margins, origin, insert, autowrap,
    /// cursor visibility, keypad and cursor keys, charsets and the saved
    /// cursor go back to their defaults; the cursor stays where it is.
    fn soft_reset(&mut self) {
        let pos = self.term.grid.cursor.pos;
        self.feed(b"\x1b[0m\x1b[r\x1b[4l\x1b[?6l\x1b[?7h\x1b[?25h\x1b>\x1b[?1l\x1b(B\x1b)B\x1b*B\x1b+B\x0f\x1b[H\x1b7");
        self.term.grid.cursor.pos = pos;
        self.term.grid.cursor.should_wrap = false;
        self.margins = None;
    }

    /// Bytes that reconstruct the unfinished parser state; unavailable past the tracking limit.
    pub fn continuation(&self) -> Option<Vec<u8>> {
        self.scan.continuation().map(<[u8]>::to_vec)
    }

    /// Consumes through the first clean parser boundary, or the entire slice if none is reached.
    pub fn write_until_ground(&mut self, bytes: &[u8]) -> Option<usize> {
        let n = self.scan.until_ground(bytes);
        self.write(&bytes[..n.unwrap_or(bytes.len())]);
        n
    }

    /// Query replies and subscribed notifications since the last call, for the program's PTY.
    pub fn take_responses(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.responses)
    }

    /// Drops replies made after the first `keep` bytes (ones someone else gives).
    pub fn drop_responses_after(&mut self, keep: usize) {
        self.responses.truncate(keep);
    }

    /// Bytes of replies waiting in `take_responses`.
    pub fn pending_responses(&self) -> usize {
        self.responses.len()
    }

    /// OSC 7501 reports received since the last call, in order.
    pub fn take_status_reports(&mut self) -> Vec<StatusReport> {
        std::mem::take(&mut self.statuses)
    }

    /// Whether the title changed since the last call.
    pub fn take_title_changed(&mut self) -> bool {
        std::mem::take(&mut self.title_changed)
    }

    /// The bytes a key press sends, given the modes the program set (cursor
    /// keys, kitty keyboard, modifyOtherKeys). `mods` is `SHIFT | CTRL | ALT
    /// | SUPER`; `text` is what the key types. Empty if it can't be sent.
    pub fn encode_key(&self, key: Key, mods: u8, text: &str) -> Vec<u8> {
        let modes = keys::Modes {
            app_cursor: self.term.mode().contains(Mode::APP_CURSOR),
            kitty: self.term.keyboard_mode().bits(),
            modify_other_keys: self.term.modify_other_keys().unwrap_or(0),
        };
        keys::encode(key, mods, text, modes).unwrap_or_default()
    }

    /// One mouse-wheel notch at the middle of the screen, the way the program
    /// asked for mouse reports; empty if it didn't ask (it doesn't take the wheel).
    pub fn wheel(&self, up: bool) -> Vec<u8> {
        let mode = self.term.mode();
        if !mode.intersects(Mode::MOUSE_REPORT_CLICK | Mode::MOUSE_DRAG | Mode::MOUSE_MOTION) {
            return Vec::new();
        }
        let (cols, rows) = self.size();
        let (button, x, y) = (if up { 64u32 } else { 65 }, cols as u32 / 2 + 1, rows as u32 / 2 + 1);
        if mode.contains(Mode::SGR_MOUSE) {
            return format!("\x1b[<{button};{x};{y}M").into_bytes();
        }
        let mut out = b"\x1b[M".to_vec();
        for v in [button, x, y].map(|v| v + 32) {
            if mode.contains(Mode::UTF8_MOUSE) {
                out.extend(char::from_u32(v).unwrap_or(' ').to_string().as_bytes());
            } else {
                out.push(v.min(255) as u8);
            }
        }
        out
    }

    /// The visible screen as rows, each padded to the screen width.
    pub fn screen_rows(&self) -> Vec<String> {
        (0..self.size().1 as i32).map(|line| format::row(&self.term.grid, line)).collect()
    }

    /// The program name and version XTVERSION reports.
    pub fn set_xtversion(&mut self, version: &str) {
        self.xtversion = version.to_string();
    }

    /// Display facts from a viewer: default colors and cell size in pixels.
    /// `None` keeps what was known before. Changed colors queue a mode 2031 report
    /// for subscribers, even within the same light/dark scheme (they may re-query).
    pub fn set_display(&mut self, fg: Option<Rgb>, bg: Option<Rgb>, cursor: Option<Rgb>, cell: Option<(u32, u32)>) {
        let mut changed = false;
        for (slot, new) in [(&mut self.fg, fg), (&mut self.bg, bg), (&mut self.cursor, cursor)] {
            if let Some(c) = new {
                changed |= *slot != c;
                *slot = c;
            }
        }
        if let Some(cell) = cell {
            self.cell = cell;
        }
        if changed && self.scheme_reports {
            let report = self.scheme_report();
            self.responses.extend(report.as_bytes());
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        let size = size(cols, rows);
        let (lines, columns) = (size.screen_lines(), size.columns());
        self.term.resize(size);
        if let Some(primary) = &mut self.primary {
            primary.resize(true, lines, columns);
        }
        self.margins = None;
    }

    pub fn size(&self) -> (u16, u16) {
        (self.term.columns() as u16, self.term.screen_lines() as u16)
    }

    pub fn alt_screen(&self) -> bool {
        self.term.mode().contains(Mode::ALT_SCREEN)
    }

    /// A DEC private mode (e.g. 1004 focus reporting). Rio has one flag for
    /// the alternate screen, so 47, 1047 and 1049 all read it.
    pub fn dec_mode(&self, mode: u16) -> bool {
        let flag = match mode {
            1 => Mode::APP_CURSOR,
            6 => Mode::ORIGIN,
            7 => Mode::LINE_WRAP,
            9 => Mode::MOUSE_REPORT_X10,
            25 => Mode::SHOW_CURSOR,
            47 | 1047 | 1049 => Mode::ALT_SCREEN,
            1000 => Mode::MOUSE_REPORT_CLICK,
            1002 => Mode::MOUSE_DRAG,
            1003 => Mode::MOUSE_MOTION,
            1004 => Mode::FOCUS_IN_OUT,
            1005 => Mode::UTF8_MOUSE,
            1006 => Mode::SGR_MOUSE,
            1007 => Mode::ALTERNATE_SCROLL,
            1042 => Mode::URGENCY_HINTS,
            2004 => Mode::BRACKETED_PASTE,
            2026 => Mode::SYNC_UPDATE,
            2027 => Mode::GRAPHEME_CLUSTER,
            2031 => return self.scheme_reports,
            _ => return false,
        };
        self.term.mode().contains(flag)
    }

    pub fn title(&self) -> String {
        self.term.title.clone()
    }

    /// The active screen and its scrollback as plain text, soft wraps joined.
    pub fn plain_text(&self) -> String {
        let grid = &self.term.grid;
        format::plain(grid, -(grid.history_size() as i32), grid.screen_lines() as i32)
    }

    /// Bytes that repaint this terminal on a fresh viewer: up to `history`
    /// scrollback lines (main screen only), then the screen with its modes,
    /// cursor and title.
    pub fn paint(&self, history: usize) -> Vec<u8> {
        // A soft reset leaves mouse, focus, paste, sync and key modes alone: turn
        // them off too, so a viewer that missed their "off" (a dropped backlog)
        // ends up with exactly the screen's own modes, replayed below.
        let mut out = String::from(
            "\x1b[!p\x1b[?2026l\x1b[?9l\x1b[?1000l\x1b[?1001l\x1b[?1002l\x1b[?1003l\x1b[?1004l\
\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?1016l\x1b[?2004l\x1b[?1l\x1b[?66l\x1b[?2031l\x1b[?2048l\x1b[=0;1u\x1b[>4m\x1b[0 q\
\x1b[?1049l\x1b[?1047l\x1b[?47l\x1b[H\x1b[2J",
        );
        let grid = &self.term.grid;
        match &self.primary {
            // The viewer gets the hidden primary screen too, for when the program leaves.
            Some(primary) if self.alt_screen() => {
                format::screen(&mut out, primary, history);
                format::cursor(&mut out, primary, 0);
                // The primary screen's pen, margins and charsets must not leak into this one.
                out.push_str("\x1b[?1049h\x1b[!p");
                format::screen(&mut out, grid, 0);
            }
            _ => format::screen(&mut out, grid, if self.alt_screen() { 0 } else { history }),
        }
        let mode = self.term.mode();
        let mut top = 0;
        if let Some((t, b)) = self.margins {
            let _ = write!(out, "\x1b[{t};{b}r");
            if mode.contains(Mode::ORIGIN) {
                out.push_str("\x1b[?6h");
                top = t as i32 - 1;
            }
        }
        format::cursor(&mut out, grid, top);
        for (flag, on) in [
            (Mode::APP_CURSOR, "\x1b[?1h"),
            (Mode::APP_KEYPAD, "\x1b="),
            (Mode::MOUSE_REPORT_X10, "\x1b[?9h"),
            (Mode::MOUSE_REPORT_CLICK, "\x1b[?1000h"),
            (Mode::MOUSE_DRAG, "\x1b[?1002h"),
            (Mode::MOUSE_MOTION, "\x1b[?1003h"),
            (Mode::UTF8_MOUSE, "\x1b[?1005h"),
            (Mode::SGR_MOUSE, "\x1b[?1006h"),
            (Mode::FOCUS_IN_OUT, "\x1b[?1004h"),
            (Mode::BRACKETED_PASTE, "\x1b[?2004h"),
            (Mode::INSERT, "\x1b[4h"),
            (Mode::LINE_FEED_NEW_LINE, "\x1b[20h"),
        ] {
            if mode.contains(flag) {
                out.push_str(on);
            }
        }
        for (flag, off) in [
            (Mode::LINE_WRAP, "\x1b[?7l"),
            (Mode::SHOW_CURSOR, "\x1b[?25l"),
            (Mode::ALTERNATE_SCROLL, "\x1b[?1007l"),
            (Mode::GRAPHEME_CLUSTER, "\x1b[?2027l"),
        ] {
            if !mode.contains(flag) {
                out.push_str(off);
            }
        }
        if self.scheme_reports {
            out.push_str("\x1b[?2031h");
        }
        let kitty = self.term.keyboard_mode().bits();
        if kitty != 0 {
            let _ = write!(out, "\x1b[={kitty};1u");
        }
        if let Some(other_keys) = self.term.modify_other_keys().filter(|&n| n != 0) {
            let _ = write!(out, "\x1b[>4;{other_keys}m");
        }
        let shape = match self.term.cursor_shape {
            CursorShape::Underline => 3,
            CursorShape::Beam => 5,
            _ => 1,
        } + u8::from(!self.term.blinking_cursor);
        if shape != 2 {
            let _ = write!(out, "\x1b[{shape} q");
        }
        if let Some(dir) = &self.term.current_directory {
            let _ = write!(out, "\x1b]7;{}\x1b\\", format::file_url(dir));
        }
        if !self.term.title.is_empty() {
            let _ = write!(out, "\x1b]2;{}\x1b\\", self.term.title);
        }
        out.into_bytes()
    }

    /// The active screen and its scrollback with their colors, but no modes,
    /// cursor or title: what a program left, to show above whatever runs next.
    pub fn replay(&self) -> Vec<u8> {
        let mut out = String::new();
        let all = |out: &mut String, g: &Grid<Square>| {
            format::lines(out, g, -(g.history_size() as i32), g.screen_lines() as i32, true);
        };
        // A full-screen program's last frame goes below the primary screen and scrollback it hid.
        if let Some(primary) = self.primary.as_deref().filter(|_| self.alt_screen()) {
            all(&mut out, primary);
            out.push_str("\r\n");
        }
        all(&mut out, &self.term.grid);
        out.into_bytes()
    }

    /// Everything needed to bring this terminal back: its size, a full
    /// repaint, colors the program set, and any half-received escape sequence.
    pub fn snapshot(&self) -> Option<Vec<u8>> {
        let (cols, rows) = self.size();
        let mut out = SNAPSHOT_MAGIC.to_vec();
        out.extend(cols.to_le_bytes());
        out.extend(rows.to_le_bytes());
        out.extend(self.paint(usize::MAX));
        for index in 0..259 {
            if self.term.colors[index].is_some() {
                let [r, g, b] = self.color(index);
                let osc = match index {
                    256 => "10".to_string(),
                    257 => "11".into(),
                    258 => "12".into(),
                    i => format!("4;{i}"),
                };
                out.extend(format!("\x1b]{osc};rgb:{r:02x}/{g:02x}/{b:02x}\x1b\\").as_bytes());
            }
        }
        out.extend(self.scan.continuation().unwrap_or_default());
        Some(out)
    }

    /// Restores a snapshot; `None` if it doesn't decode (e.g. another version's).
    pub fn restore(bytes: &[u8], scrollback_bytes: usize) -> Option<Terminal> {
        let rest = bytes.strip_prefix(SNAPSHOT_MAGIC)?;
        let (size, body) = rest.split_at_checked(4)?;
        let cols = u16::from_le_bytes([size[0], size[1]]);
        let rows = u16::from_le_bytes([size[2], size[3]]);
        let mut t = Terminal::new(cols, rows, scrollback_bytes);
        t.write(body);
        t.take_responses();
        t.take_status_reports();
        t.take_title_changed();
        Some(t)
    }
}

#[cfg(test)]
mod tests;
