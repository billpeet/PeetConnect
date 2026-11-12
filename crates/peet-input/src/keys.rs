//! Key-name to hardware-scancode mapping (Set 1 / "XT" scancodes).
//!
//! Scancodes describe a physical key position and are layout independent, which is what the
//! wire protocol carries. This table covers the keys a CLI or viewer needs to name explicitly;
//! ordinary text should travel as [`peet_proto::InputEvent::Text`] instead.

/// A named key resolved to its scancode and whether it needs the extended (`E0`) prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key {
    pub scancode: u16,
    pub extended: bool,
}

const fn k(scancode: u16) -> Key {
    Key {
        scancode,
        extended: false,
    }
}

const fn e(scancode: u16) -> Key {
    Key {
        scancode,
        extended: true,
    }
}

/// Resolves a case-insensitive key name (e.g. `"enter"`, `"ctrl"`, `"f5"`, `"a"`, `"up"`).
#[allow(clippy::match_same_arms)]
pub fn lookup(name: &str) -> Option<Key> {
    let lower = name.to_ascii_lowercase();
    Some(match lower.as_str() {
        "esc" | "escape" => k(0x01),
        "1" => k(0x02),
        "2" => k(0x03),
        "3" => k(0x04),
        "4" => k(0x05),
        "5" => k(0x06),
        "6" => k(0x07),
        "7" => k(0x08),
        "8" => k(0x09),
        "9" => k(0x0A),
        "0" => k(0x0B),
        "-" | "minus" => k(0x0C),
        "=" | "equals" => k(0x0D),
        "backspace" | "bksp" => k(0x0E),
        "tab" => k(0x0F),
        "q" => k(0x10),
        "w" => k(0x11),
        "e" => k(0x12),
        "r" => k(0x13),
        "t" => k(0x14),
        "y" => k(0x15),
        "u" => k(0x16),
        "i" => k(0x17),
        "o" => k(0x18),
        "p" => k(0x19),
        "[" => k(0x1A),
        "]" => k(0x1B),
        "enter" | "return" => k(0x1C),
        "ctrl" | "lctrl" | "control" => k(0x1D),
        "rctrl" => e(0x1D),
        "a" => k(0x1E),
        "s" => k(0x1F),
        "d" => k(0x20),
        "f" => k(0x21),
        "g" => k(0x22),
        "h" => k(0x23),
        "j" => k(0x24),
        "k" => k(0x25),
        "l" => k(0x26),
        ";" => k(0x27),
        "'" => k(0x28),
        "`" => k(0x29),
        "shift" | "lshift" => k(0x2A),
        "\\" => k(0x2B),
        "z" => k(0x2C),
        "x" => k(0x2D),
        "c" => k(0x2E),
        "v" => k(0x2F),
        "b" => k(0x30),
        "n" => k(0x31),
        "m" => k(0x32),
        "," => k(0x33),
        "." => k(0x34),
        "/" => k(0x35),
        "rshift" => k(0x36),
        "alt" | "lalt" => k(0x38),
        "ralt" | "altgr" => e(0x38),
        "space" => k(0x39),
        "capslock" => k(0x3A),
        "f1" => k(0x3B),
        "f2" => k(0x3C),
        "f3" => k(0x3D),
        "f4" => k(0x3E),
        "f5" => k(0x3F),
        "f6" => k(0x40),
        "f7" => k(0x41),
        "f8" => k(0x42),
        "f9" => k(0x43),
        "f10" => k(0x44),
        "f11" => k(0x57),
        "f12" => k(0x58),
        "up" => e(0x48),
        "down" => e(0x50),
        "left" => e(0x4B),
        "right" => e(0x4D),
        "home" => e(0x47),
        "end" => e(0x4F),
        "pageup" | "pgup" => e(0x49),
        "pagedown" | "pgdn" => e(0x51),
        "insert" | "ins" => e(0x52),
        "delete" | "del" => e(0x53),
        "win" | "lwin" | "super" | "meta" => e(0x5B),
        "rwin" => e(0x5C),
        "apps" | "menu" => e(0x5D),
        _ => return None,
    })
}

/// Splits a chord like `"ctrl+shift+t"` into its named parts.
pub fn parse_chord(chord: &str) -> Vec<&str> {
    chord
        .split('+')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_resolve() {
        assert_eq!(lookup("Enter"), Some(k(0x1C)));
        assert_eq!(lookup("UP"), Some(e(0x48)));
        assert_eq!(lookup("a"), Some(k(0x1E)));
        assert_eq!(lookup("nope"), None);
    }

    #[test]
    fn chords_split() {
        assert_eq!(parse_chord("ctrl + shift+t"), vec!["ctrl", "shift", "t"]);
    }
}
