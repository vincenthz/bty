//! Key names (`enter`, `up`, `ctrl+c`, `alt+x`, `f5`, `a`) → the bytes a real
//! terminal would send, using the modes the program set.

use crate::rpc::RpcError;
use bty_vt::{Key, Terminal as Screen};

/// A key name without modifiers, and the character it types.
fn key_code(name: &str) -> Option<(Key, char)> {
    let c = name.chars().next()?;
    let single = name.chars().count() == 1;
    Some(match name {
        _ if single && (c.is_ascii_lowercase() || c.is_ascii_digit() || "`[],=-.';/".contains(c)) => (Key::Char(c), c),
        "\\" | "backslash" => (Key::Char('\\'), '\\'),
        "backspace" => (Key::Backspace, '\0'),
        "enter" | "return" => (Key::Enter, '\0'),
        "space" => (Key::Char(' '), ' '),
        "tab" => (Key::Tab, '\0'),
        "delete" | "del" => (Key::Delete, '\0'),
        "end" => (Key::End, '\0'),
        "home" => (Key::Home, '\0'),
        "insert" => (Key::Insert, '\0'),
        "pagedown" | "pgdn" => (Key::PageDown, '\0'),
        "pageup" | "pgup" => (Key::PageUp, '\0'),
        "down" => (Key::Down, '\0'),
        "left" => (Key::Left, '\0'),
        "right" => (Key::Right, '\0'),
        "up" => (Key::Up, '\0'),
        "esc" | "escape" => (Key::Escape, '\0'),
        _ => {
            let n: u8 = name.strip_prefix('f')?.parse().ok().filter(|n| (1..=12).contains(n))?;
            (Key::F(n), '\0')
        }
    })
}

/// Encodes one key name like `ctrl+shift+tab` for this screen.
pub fn encode(screen: &Screen, spec: &str) -> Result<Vec<u8>, RpcError> {
    let bad = || RpcError::invalid_params(format!("unknown key {spec:?} (try enter, esc, up, ctrl+c, alt+x, f5, a)"));
    let lower = spec.to_ascii_lowercase();
    let mut parts: Vec<&str> = lower.split('+').collect();
    // `ctrl++` style ends in an empty part for the plus key itself; we don't support it.
    let name = parts.pop().filter(|n| !n.is_empty()).ok_or_else(bad)?;
    let mut mods = 0u8;
    for m in parts {
        mods |= match m {
            "shift" => bty_vt::SHIFT,
            "ctrl" | "control" => bty_vt::CTRL,
            "alt" | "meta" | "opt" => bty_vt::ALT,
            "super" | "cmd" => bty_vt::SUPER,
            _ => return Err(bad()),
        };
    }
    let (key, ch) = key_code(name).ok_or_else(bad)?;
    let text = match ch {
        '\0' => String::new(),
        c if mods & bty_vt::SHIFT != 0 => c.to_ascii_uppercase().to_string(),
        c => c.to_string(),
    };
    let bytes = screen.encode_key(key, mods, if mods & (bty_vt::CTRL | bty_vt::ALT) != 0 { "" } else { &text });
    if bytes.is_empty() { Err(bad()) } else { Ok(bytes) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(screen: &Screen, names: &[&str]) -> Vec<String> {
        names.iter().map(|n| String::from_utf8_lossy(&encode(screen, n).unwrap()).into_owned()).collect()
    }

    #[test]
    fn names_encode_like_a_real_terminal_in_the_programs_modes() {
        let mut screen = Screen::new(80, 24, 1 << 20);
        assert_eq!(
            keys(
                &screen,
                &["enter", "esc", "tab", "backspace", "a", "shift+a", "7", "ctrl+c", "alt+x", "up", "f1", "space"]
            ),
            ["\r", "\x1b", "\t", "\x7f", "a", "A", "7", "\x03", "\x1bx", "\x1b[A", "\x1bOP", " "]
        );
        // Application cursor keys (vim, less) change what arrows send.
        screen.write(b"\x1b[?1h");
        assert_eq!(keys(&screen, &["up", "left"]), ["\x1bOA", "\x1bOD"]);
        // A program that asked for the kitty keyboard gets kitty encodings.
        screen.write(b"\x1b[>1u");
        assert_eq!(keys(&screen, &["esc", "ctrl+backslash"]), ["\x1b[27u", "\x1b[92;5u"]);
        for bad in ["hello", "hyper+x", "f13", "ctrl+", ""] {
            assert!(encode(&screen, bad).is_err(), "{bad}");
        }
    }
}
