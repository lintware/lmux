//! Pure helpers for talking to tmux: key-name mapping and control-mode
//! `%output` octal unescaping. Kept free of I/O so they can be unit tested.

/// Decode tmux control-mode escaped output into raw bytes.
///
/// tmux escapes non-printable bytes as `\ooo` (exactly three octal digits) and
/// backslash as `\\`. Everything else passes through as-is.
pub fn unescape_output(s: &str) -> Vec<u8> {
    unescape_output_bytes(s.as_bytes())
}

/// Byte-oriented form of [`unescape_output`]. The control stream is read as raw
/// bytes, so the escaped payload never has to be valid UTF-8.
pub fn unescape_output_bytes(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            let next = bytes[i + 1];
            if (b'0'..=b'7').contains(&next) {
                let mut val: u32 = 0;
                let mut n = 0;
                let mut j = i + 1;
                while n < 3 && j < bytes.len() && (b'0'..=b'7').contains(&bytes[j]) {
                    val = val * 8 + (bytes[j] - b'0') as u32;
                    j += 1;
                    n += 1;
                }
                out.push((val & 0xff) as u8);
                i = j;
                continue;
            } else {
                // Escaped literal backslash or other single char.
                out.push(next);
                i += 2;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// Map a human key name (`enter`, `ctrl+c`, `pageup`, `f5`, …) to a tmux
/// `send-keys` key name. Returns `None` if the name is unknown.
pub fn map_key_name(key: &str) -> Option<String> {
    let k = key.trim();
    if k.is_empty() {
        return None;
    }
    let lower = k.to_ascii_lowercase();

    // Modifier combinations: ctrl+x / c-x / ^x, alt+x / m-x, shift+x, plus combos.
    if let Some((mods, base)) = strip_mods(&lower) {
        if base.chars().count() == 1 && base.is_ascii() {
            // Single characters combine as C-a / M-a / S-a.
            let mut prefix = String::new();
            if mods.ctrl {
                prefix.push_str("C-");
            }
            if mods.alt {
                prefix.push_str("M-");
            }
            if mods.shift && !mods.ctrl && !mods.alt {
                prefix.push('S');
            }
            let ch = if mods.ctrl || mods.alt {
                base.to_string()
            } else {
                base.to_ascii_lowercase()
            };
            return Some(format!("{prefix}{ch}"));
        }
        if let Some(name) = base_key_name(base) {
            // Named keys: shift+tab -> BTab, others keep the base name.
            if mods.shift && base == "tab" {
                return Some("BTab".to_string());
            }
            let mut prefix = String::new();
            if mods.ctrl {
                prefix.push_str("C-");
            }
            if mods.alt {
                prefix.push_str("M-");
            }
            return Some(format!("{prefix}{name}"));
        }
        return None;
    }

    if lower.chars().count() == 1 {
        return Some(lower);
    }
    base_key_name(&lower).map(|s| s.to_string())
}

#[derive(Default)]
struct Mods {
    ctrl: bool,
    alt: bool,
    shift: bool,
}

/// Split `ctrl+alt+shift+x` style prefixes. Returns (mods, base).
fn strip_mods(lower: &str) -> Option<(Mods, &str)> {
    // ^x shorthand
    if let Some(rest) = lower.strip_prefix('^') {
        if !rest.is_empty() {
            let mut m = Mods::default();
            m.ctrl = true;
            return Some((m, rest));
        }
        return None;
    }
    let mut mods = Mods::default();
    let mut rest = lower;
    let mut any = false;
    loop {
        let (name, sep) = match rest.find(|c| c == '+' || c == '-') {
            Some(i) => (&rest[..i], true),
            None => (rest, false),
        };
        let consumed = match name {
            "ctrl" | "control" | "c" => {
                mods.ctrl = true;
                true
            }
            "alt" | "meta" | "option" | "opt" | "m" => {
                mods.alt = true;
                true
            }
            "shift" | "s" => {
                mods.shift = true;
                true
            }
            _ => false,
        };
        if !consumed || !sep {
            break;
        }
        any = true;
        rest = &rest[name.len() + 1..];
    }
    if any {
        Some((mods, rest))
    } else {
        None
    }
}

fn base_key_name(k: &str) -> Option<&'static str> {
    Some(match k {
        "enter" | "return" | "cr" => "Enter",
        "escape" | "esc" => "Escape",
        "tab" => "Tab",
        "space" | "spc" => "Space",
        "backspace" | "bs" | "bksp" => "BSpace",
        "delete" | "del" => "DC",
        "insert" | "ins" => "IC",
        "up" | "arrowup" => "Up",
        "down" | "arrowdown" => "Down",
        "left" | "arrowleft" => "Left",
        "right" | "arrowright" => "Right",
        "home" => "Home",
        "end" => "End",
        "pageup" | "pgup" | "prior" => "PageUp",
        "pagedown" | "pgdn" | "next" => "PageDown",
        "f1" => "F1",
        "f2" => "F2",
        "f3" => "F3",
        "f4" => "F4",
        "f5" => "F5",
        "f6" => "F6",
        "f7" => "F7",
        "f8" => "F8",
        "f9" => "F9",
        "f10" => "F10",
        "f11" => "F11",
        "f12" => "F12",
        "backtab" | "btab" => "BTab",
        "npage" => "NPage",
        "ppage" => "PPage",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn octal_unescape_basic() {
        assert_eq!(unescape_output("\\033[31mhi\\015\\012"), b"\x1b[31mhi\r\n");
        assert_eq!(unescape_output("plain"), b"plain");
    }

    #[test]
    fn octal_unescape_partial_digits() {
        // tmux always writes three digits, but be lenient.
        assert_eq!(unescape_output("\\7"), vec![7u8]);
        assert_eq!(unescape_output("\\12"), vec![0o12u8]);
    }

    #[test]
    fn octal_unescape_backslash_and_mixed() {
        assert_eq!(unescape_output("a\\\\b"), b"a\\b");
        assert_eq!(unescape_output("\\0101"), b"\x081");
    }

    #[test]
    fn key_names() {
        assert_eq!(map_key_name("enter").unwrap(), "Enter");
        assert_eq!(map_key_name("ESCAPE").unwrap(), "Escape");
        assert_eq!(map_key_name("up").unwrap(), "Up");
        assert_eq!(map_key_name("pageup").unwrap(), "PageUp");
        assert_eq!(map_key_name("f5").unwrap(), "F5");
        assert_eq!(map_key_name("ctrl+c").unwrap(), "C-c");
        assert_eq!(map_key_name("C-c").unwrap(), "C-c");
        assert_eq!(map_key_name("^c").unwrap(), "C-c");
        assert_eq!(map_key_name("alt+x").unwrap(), "M-x");
        assert_eq!(map_key_name("shift+tab").unwrap(), "BTab");
        assert_eq!(map_key_name("ctrl+alt+delete").unwrap(), "C-M-DC");
    }

    #[test]
    fn unknown_keys_none() {
        assert!(map_key_name("notakey").is_none());
        assert!(map_key_name("").is_none());
    }
}
