//! Follows rio's parser (rio-vt `performer::parser`, the vt100.net DEC
//! machine) byte for byte, for what that parser keeps private: where escape
//! sequences end, so a snapshot can carry a half-received one; and the few
//! sequences this crate answers or tracks itself, so the caller can split
//! its write just before their final byte.
//!
//! It must agree with rio on every transition that enters or leaves ground.
//! The tests compare the two on the cases that matter.

/// Unfinished escape sequences kept so a snapshot can be taken at any byte.
pub const CONTINUATION_MAX_BYTES: usize = 64 * 1024;

const MAX_PARAMS: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Ground,
    Esc {
        inter: bool,
    },
    Csi(Csi),
    /// DCS entry, param and intermediate: all hook on a final byte.
    DcsHead {
        param: bool,
        inter: bool,
    },
    DcsIgnore,
    DcsPass,
    /// OSC, APC, SOS and PM: BEL, CAN, SUB and ESC end them alike.
    Str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CsiPart {
    Entry,
    Param,
    Inter,
    Ignore,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Csi {
    part: CsiPart,
    private: u8,
    inter: u8,
    inters: u8,
    params: [u16; MAX_PARAMS],
    n: u8,
    full: bool,
}

impl Csi {
    fn new() -> Csi {
        Csi { part: CsiPart::Entry, private: 0, inter: 0, inters: 0, params: [0; MAX_PARAMS], n: 1, full: false }
    }

    fn digit(&mut self, b: u8) {
        let p = &mut self.params[self.n as usize - 1];
        *p = p.saturating_mul(10).saturating_add((b - b'0') as u16);
    }

    fn next_param(&mut self) {
        if (self.n as usize) < MAX_PARAMS {
            self.n += 1;
        } else {
            self.full = true;
        }
    }

    fn params(&self) -> &[u16] {
        &self.params[..self.n as usize]
    }
}

/// A sequence the caller handles, reported with its final byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Seq {
    /// `ESC c`, full reset.
    Ris,
    /// DECSET/DECRST touching the alternate screen (47, 1047, 1049) or 2031.
    PrivateMode {
        set: bool,
        alt: bool,
        color_scheme: Option<bool>,
    },
    /// `CSI ? 996 n`: which color scheme is in use.
    ColorSchemeQuery,
    /// `CSI ? 2031 $ p`: is color scheme reporting on.
    ColorSchemeModeQuery,
    /// `CSI ! p`, DECSTR.
    SoftReset,
    /// `CSI > q`, XTVERSION.
    Version,
    /// `CSI c` and `CSI > c`.
    PrimaryAttributes,
    SecondaryAttributes,
    /// `CSI 14 t` and `CSI 16 t`: sizes in pixels.
    PixelSize,
    CellSize,
    /// DECSTBM with its raw top and bottom (0 meaning default).
    Margins(u16, u16),
    /// `CSI 2 J`, erase the whole screen.
    EraseDisplay,
}

fn classify(c: &Csi, last: u8) -> Option<Seq> {
    if c.full || c.inters > 1 {
        return None;
    }
    let p = c.params();
    let first = p[0];
    match (c.private, c.inter, last) {
        (b'?', 0, b'h' | b'l') => {
            let alt = p.iter().any(|m| matches!(m, 47 | 1047 | 1049));
            let scheme = p.contains(&2031);
            (alt || scheme).then_some(Seq::PrivateMode {
                set: last == b'h',
                alt,
                color_scheme: scheme.then_some(last == b'h'),
            })
        }
        (b'?', 0, b'n') if first == 996 => Some(Seq::ColorSchemeQuery),
        (b'?', b'$', b'p') if first == 2031 => Some(Seq::ColorSchemeModeQuery),
        (0, b'!', b'p') => Some(Seq::SoftReset),
        (b'>', 0, b'q') if first == 0 => Some(Seq::Version),
        (0, 0, b'c') if first == 0 => Some(Seq::PrimaryAttributes),
        (b'>', 0, b'c') if first == 0 => Some(Seq::SecondaryAttributes),
        (0, 0, b't') if first == 14 => Some(Seq::PixelSize),
        (0, 0, b't') if first == 16 => Some(Seq::CellSize),
        (0, 0, b'r') => Some(Seq::Margins(first, p.get(1).copied().unwrap_or(0))),
        (0, 0, b'J') if first == 2 => Some(Seq::EraseDisplay),
        _ => None,
    }
}

/// Bytes a valid UTF-8 sequence starting with `lead` has, and the range its second byte must be in.
fn utf8_lead(lead: u8) -> Option<(usize, u8, u8)> {
    Some(match lead {
        0xc2..=0xdf => (2, 0x80, 0xbf),
        0xe0 => (3, 0xa0, 0xbf),
        0xed => (3, 0x80, 0x9f),
        0xe1..=0xef => (3, 0x80, 0xbf),
        0xf0 => (4, 0x90, 0xbf),
        0xf4 => (4, 0x80, 0x8f),
        0xf1..=0xf3 => (4, 0x80, 0xbf),
        _ => return None,
    })
}

/// Whether `s` is the start of a UTF-8 character that needs more bytes.
fn incomplete_utf8(s: &[u8]) -> bool {
    let Some((len, lo, hi)) = s.first().and_then(|&b| utf8_lead(b)) else { return false };
    s.len() < len
        && s.get(1).is_none_or(|b| (lo..=hi).contains(b))
        && s.iter().skip(2).all(|b| (0x80..=0xbf).contains(b))
}

/// Where a trailing, still incomplete UTF-8 character starts in a ground run.
fn utf8_tail(run: &[u8]) -> Option<usize> {
    let from = run.len().saturating_sub(3);
    (from..run.len()).rev().find(|&i| run[i] >= 0xc0).filter(|&i| incomplete_utf8(&run[i..]))
}

#[derive(Clone, Debug)]
pub struct Scanner {
    state: State,
    /// Everything since the parser last stood at ground, unless it grew past the limit.
    pending: Vec<u8>,
    overflow: bool,
}

impl Default for Scanner {
    fn default() -> Self {
        Scanner { state: State::Ground, pending: Vec::new(), overflow: false }
    }
}

/// One step of a scan: bytes consumed, and the sequence whose final byte was the last of them.
pub struct Step {
    pub consumed: usize,
    pub seq: Option<Seq>,
    /// Offset just past the last point the parser stood at ground, if it did.
    ground: Option<usize>,
}

impl Scanner {
    /// What reconstructs the parser's unfinished state; `None` past the limit.
    pub fn continuation(&self) -> Option<&[u8]> {
        (!self.overflow).then_some(&self.pending)
    }

    pub fn at_ground(&self) -> bool {
        self.state == State::Ground && self.pending.is_empty() && !self.overflow
    }

    /// Bytes up to and including the next point the parser reaches ground, if any.
    pub fn until_ground(&self, bytes: &[u8]) -> Option<usize> {
        if self.at_ground() {
            return Some(0);
        }
        let mut probe = Scanner { state: self.state, pending: self.pending.clone(), overflow: self.overflow };
        let mut at = 0;
        while at < bytes.len() {
            let step = probe.step(&bytes[at..]);
            if let Some(g) = step.ground {
                return Some(at + g);
            }
            at += step.consumed;
        }
        None
    }

    /// Scans up to and including the next sequence the caller handles, or to the end.
    pub fn scan(&mut self, bytes: &[u8]) -> Step {
        let step = self.step(bytes);
        let tail = match step.ground {
            Some(g) => {
                self.pending.clear();
                self.overflow = false;
                &bytes[g..step.consumed]
            }
            None => &bytes[..step.consumed],
        };
        if !self.overflow {
            if self.pending.len() + tail.len() > CONTINUATION_MAX_BYTES {
                self.pending = Vec::new();
                self.overflow = true;
            } else {
                self.pending.extend_from_slice(tail);
            }
        }
        step
    }

    fn step(&mut self, bytes: &[u8]) -> Step {
        let mut ground = None;
        let mut i = 0;
        // A character split across writes finishes (or breaks) first.
        if self.state == State::Ground && !self.pending.is_empty() && !self.overflow {
            let mut s = self.pending.clone();
            while i < bytes.len() && (0x80..=0xbf).contains(&bytes[i]) {
                s.push(bytes[i]);
                i += 1;
                if !incomplete_utf8(&s) {
                    break;
                }
            }
            if i == bytes.len() && incomplete_utf8(&s) {
                return Step { consumed: i, seq: None, ground: None };
            }
            ground = Some(i);
        }
        while i < bytes.len() {
            let b = bytes[i];
            match &mut self.state {
                State::Ground => {
                    let run = bytes[i..].iter().position(|&b| b == 0x1b).map_or(bytes.len(), |n| i + n);
                    ground = Some(match utf8_tail(&bytes[i..run]) {
                        Some(t) if run == bytes.len() => i + t,
                        _ => run,
                    });
                    i = run;
                    if i < bytes.len() {
                        self.state = State::Esc { inter: false };
                        i += 1;
                    }
                    continue;
                }
                State::Esc { inter } => match b {
                    0x18 | 0x1a => self.state = State::Ground,
                    0x1b => *inter = false,
                    0x20..=0x2f => *inter = true,
                    _ if *inter && (0x30..=0x7e).contains(&b) => self.state = State::Ground,
                    _ if *inter && b > 0x7f => {}
                    b'P' => self.state = State::DcsHead { param: false, inter: false },
                    b'[' => self.state = State::Csi(Csi::new()),
                    b']' | b'X' | b'^' | b'_' => self.state = State::Str,
                    b'c' => {
                        self.state = State::Ground;
                        return Step { consumed: i + 1, seq: Some(Seq::Ris), ground: Some(i + 1) };
                    }
                    0x30..=0x7e => self.state = State::Ground,
                    _ => {}
                },
                State::Csi(c) => match (c.part, b) {
                    (_, 0x18 | 0x1a) => self.state = State::Ground,
                    (_, 0x1b) => self.state = State::Esc { inter: false },
                    (_, 0x00..=0x1f | 0x7f..) => {}
                    (CsiPart::Ignore, 0x20..=0x3f) => {}
                    (CsiPart::Ignore, _) => self.state = State::Ground,
                    (CsiPart::Inter, 0x30..=0x3f) => c.part = CsiPart::Ignore,
                    (CsiPart::Param, 0x3c..=0x3f) => c.part = CsiPart::Ignore,
                    (_, 0x20..=0x2f) => {
                        c.inter = b;
                        c.inters += 1;
                        c.part = CsiPart::Inter;
                    }
                    (CsiPart::Entry, 0x3c..=0x3f) => {
                        c.private = b;
                        c.part = CsiPart::Param;
                    }
                    (_, b'0'..=b'9') => {
                        c.digit(b);
                        c.part = CsiPart::Param;
                    }
                    (_, b':' | b';') => {
                        c.next_param();
                        c.part = CsiPart::Param;
                    }
                    (_, 0x40..=0x7e) => {
                        let seq = classify(c, b);
                        self.state = State::Ground;
                        if seq.is_some() {
                            return Step { consumed: i + 1, seq, ground: Some(i + 1) };
                        }
                    }
                },
                State::DcsHead { param, inter } => match b {
                    0x18 | 0x1a => self.state = State::Ground,
                    0x1b => self.state = State::Esc { inter: false },
                    0x20..=0x2f => *inter = true,
                    0x30..=0x3f if *inter => self.state = State::DcsIgnore,
                    0x3c..=0x3f if *param => self.state = State::DcsIgnore,
                    0x30..=0x3f => *param = true,
                    0x40..=0x7e => self.state = State::DcsPass,
                    _ => {}
                },
                State::DcsIgnore | State::DcsPass | State::Str => {
                    let (bel, st) = (self.state == State::Str, self.state == State::DcsPass);
                    let ends = |b: u8| match b {
                        0x18 | 0x1a | 0x1b => true,
                        0x07 => bel,
                        0x9c => st,
                        _ => false,
                    };
                    match bytes[i..].iter().position(|&b| ends(b)) {
                        None => i = bytes.len(),
                        Some(n) => {
                            i += n;
                            i += 1;
                            if bytes[i - 1] == 0x1b {
                                self.state = State::Esc { inter: false };
                            } else {
                                self.state = State::Ground;
                                ground = Some(i);
                            }
                        }
                    }
                    continue;
                }
            }
            i += 1;
            if self.state == State::Ground {
                ground = Some(i);
            }
        }
        Step { consumed: bytes.len(), seq: None, ground }
    }
}
