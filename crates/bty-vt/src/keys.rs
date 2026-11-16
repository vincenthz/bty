//! Key presses to the bytes a terminal sends, in the program's modes:
//! legacy, application cursor keys, xterm modifyOtherKeys, and the kitty
//! keyboard protocol's press encodings (disambiguate, report all keys).
//!
//! Adapted from Rio's `librio/src/key.rs` (MIT, Raphael Amorim): librio is
//! a C-ABI crate that isn't published, and bty only sends named presses.

/// A key, unshifted: `shift+a` is `Char('a')` with `SHIFT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Tab,
    Backspace,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    /// F1 to F12.
    F(u8),
}

pub const SHIFT: u8 = 1;
pub const CTRL: u8 = 2;
pub const ALT: u8 = 4;
pub const SUPER: u8 = 8;

/// The terminal state an encoding depends on.
#[derive(Clone, Copy, Default)]
pub(crate) struct Modes {
    pub app_cursor: bool,
    /// Kitty keyboard flags (CSI = flags u).
    pub kitty: u8,
    /// xterm modifyOtherKeys level (CSI > 4 ; n m).
    pub modify_other_keys: u8,
}

const KITTY_DISAMBIGUATE: u8 = 1;
const KITTY_ALL_AS_ESC: u8 = 8;

/// CSI's modifier parameter: 1 plus shift 1, alt 2, ctrl 4, super 8.
fn param(mods: u8) -> u8 {
    1 + (mods & SHIFT) + (mods & ALT) / 2 + (mods & CTRL) * 2 + (mods & SUPER)
}

/// The C0 byte ctrl turns a key into.
fn ctrl_byte(c: char) -> Option<u8> {
    Some(match c {
        ' ' | '@' | '2' => 0x00,
        'a'..='z' | 'A'..='Z' => (c as u8) & 0x1f,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '6' => 0x1e,
        '_' | '7' | '/' => 0x1f,
        '?' | '8' => 0x7f,
        _ => return None,
    })
}

/// Final byte of a key's `CSI x` form, and whether it is SS3 unmodified always (F1-F4)
/// or only in application cursor mode.
fn csi_final(key: Key) -> Option<(char, bool)> {
    Some(match key {
        Key::Up => ('A', false),
        Key::Down => ('B', false),
        Key::Right => ('C', false),
        Key::Left => ('D', false),
        Key::Home => ('H', false),
        Key::End => ('F', false),
        Key::F(1) => ('P', true),
        Key::F(2) => ('Q', true),
        Key::F(3) => ('R', true),
        Key::F(4) => ('S', true),
        _ => return None,
    })
}

/// The number of a key's `CSI n ~` form.
fn tilde(key: Key) -> Option<u8> {
    Some(match key {
        Key::Insert => 2,
        Key::Delete => 3,
        Key::PageUp => 5,
        Key::PageDown => 6,
        Key::F(5) => 15,
        Key::F(n @ 6..=10) => n + 11,
        Key::F(n @ 11..=12) => n + 12,
        _ => return None,
    })
}

fn kitty_codepoint(key: Key) -> u32 {
    match key {
        Key::Char(c) => c as u32,
        Key::Escape => 27,
        Key::Enter => 13,
        Key::Tab => 9,
        Key::Backspace => 127,
        // The rest have legacy CSI forms, used before this is reached.
        _ => 0,
    }
}

fn kitty(key: Key, mods: u8) -> Vec<u8> {
    let p = if mods == 0 { String::new() } else { format!(";{}", param(mods)) };
    if let Some(n) = tilde(key) {
        return format!("\x1b[{n}{p}~").into_bytes();
    }
    if let Some((f, _)) = csi_final(key) {
        let one = if mods == 0 { "" } else { "1" };
        return format!("\x1b[{one}{p}{f}").into_bytes();
    }
    format!("\x1b[{}{p}u", kitty_codepoint(key)).into_bytes()
}

fn legacy(key: Key, mods: u8, text: &str, modes: Modes) -> Option<Vec<u8>> {
    if let Some((f, ss3)) = csi_final(key) {
        return Some(
            match mods {
                0 if ss3 || modes.app_cursor => format!("\x1bO{f}"),
                0 => format!("\x1b[{f}"),
                _ => format!("\x1b[1;{}{f}", param(mods)),
            }
            .into_bytes(),
        );
    }
    if let Some(n) = tilde(key) {
        return Some(if mods == 0 { format!("\x1b[{n}~") } else { format!("\x1b[{n};{}~", param(mods)) }.into_bytes());
    }
    let base = match key {
        Key::Enter => vec![b'\r'],
        Key::Tab if mods & SHIFT != 0 => return Some(b"\x1b[Z".to_vec()),
        Key::Tab => vec![b'\t'],
        Key::Backspace => vec![0x7f],
        Key::Escape => vec![0x1b],
        Key::Char(c) if mods & CTRL != 0 => vec![ctrl_byte(c)?],
        Key::Char(_) if !text.is_empty() => text.as_bytes().to_vec(),
        Key::Char(c) => c.to_string().into_bytes(),
        _ => return None,
    };
    // Alt is meta: an ESC before what the key sends.
    Some(if mods & ALT != 0 { [&[0x1b], &base[..]].concat() } else { base })
}

/// The bytes a press sends; `None` when this terminal has no way to say it.
/// `text` is what the key types with its modifiers (`A` for `shift+a`).
pub(crate) fn encode(key: Key, mods: u8, text: &str, modes: Modes) -> Option<Vec<u8>> {
    let all = modes.kitty & KITTY_ALL_AS_ESC != 0;
    let ambiguous = mods & (CTRL | ALT | SUPER) != 0
        || key == Key::Escape
        || (mods != 0 && matches!(key, Key::Enter | Key::Tab | Key::Backspace));
    if all || (modes.kitty & KITTY_DISAMBIGUATE != 0 && ambiguous) {
        return Some(kitty(key, mods));
    }
    if let Key::Char(c) = key
        && mods & (CTRL | ALT) != 0
        && (modes.modify_other_keys >= 2
            || (modes.modify_other_keys == 1 && mods & CTRL != 0 && ctrl_byte(c).is_none()))
    {
        return Some(format!("\x1b[27;{};{}~", param(mods), c as u32).into_bytes());
    }
    // Super is the platform's own: nothing legacy can send for it.
    if mods & SUPER != 0 {
        return None;
    }
    legacy(key, mods, text, modes)
}
