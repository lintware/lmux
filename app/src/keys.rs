//! Keyboard mapping: egui key events → terminal bytes and browser DOM keys.

use egui::{Key, Modifiers};

/// Terminal input for a special or control key. Printable text is handled
/// separately (see [`text_input`]) so IME and dead keys work.
pub fn terminal_key(key: Key, mods: Modifiers) -> Option<Vec<u8>> {
    let ctrl = mods.ctrl;
    let alt = mods.alt;
    let shift = mods.shift;

    // Ctrl + letter → control code.
    if ctrl && !mods.command {
        if let Some(b) = ctrl_byte(key) {
            return Some(vec![b]);
        }
    }

    let seq: &[u8] = match key {
        Key::Enter => return Some(vec![0x0d]),
        Key::Tab => {
            if shift {
                return Some(b"\x1b[Z".to_vec());
            }
            return Some(vec![0x09]);
        }
        Key::Backspace => return Some(vec![0x7f]),
        Key::Escape => return Some(vec![0x1b]),
        Key::Space => return Some(vec![0x20]),
        Key::ArrowUp => b"\x1b[A",
        Key::ArrowDown => b"\x1b[B",
        Key::ArrowRight => b"\x1b[C",
        Key::ArrowLeft => b"\x1b[D",
        Key::Home => b"\x1b[H",
        Key::End => b"\x1b[F",
        Key::PageUp => b"\x1b[5~",
        Key::PageDown => b"\x1b[6~",
        Key::Insert => b"\x1b[2~",
        Key::Delete => b"\x1b[3~",
        Key::F1 => b"\x1bOP",
        Key::F2 => b"\x1bOQ",
        Key::F3 => b"\x1bOR",
        Key::F4 => b"\x1bOS",
        Key::F5 => b"\x1b[15~",
        Key::F6 => b"\x1b[17~",
        Key::F7 => b"\x1b[18~",
        Key::F8 => b"\x1b[19~",
        Key::F9 => b"\x1b[20~",
        Key::F10 => b"\x1b[21~",
        Key::F11 => b"\x1b[23~",
        Key::F12 => b"\x1b[24~",
        _ => return None,
    };

    let mut out = Vec::with_capacity(seq.len() + 4);
    // Modifier-encoded cursor keys: \x1b[1;<m><final>.
    if is_cursor(key) && (shift || alt || ctrl) {
        let m = 1 + (shift as u8) + (alt as u8) * 2 + (ctrl as u8) * 4;
        let final_byte = *seq.last().unwrap();
        out.extend_from_slice(b"\x1b[1;");
        out.push(b'0' + m);
        out.push(final_byte);
        return Some(out);
    }
    out.extend_from_slice(seq);
    if alt {
        out.insert(0, 0x1b);
    }
    Some(out)
}

fn is_cursor(key: Key) -> bool {
    matches!(
        key,
        Key::ArrowUp | Key::ArrowDown | Key::ArrowLeft | Key::ArrowRight | Key::Home | Key::End
    )
}

fn ctrl_byte(key: Key) -> Option<u8> {
    Some(match key {
        Key::A => 0x01,
        Key::B => 0x02,
        Key::C => 0x03,
        Key::D => 0x04,
        Key::E => 0x05,
        Key::F => 0x06,
        Key::G => 0x07,
        Key::H => 0x08,
        Key::I => 0x09,
        Key::J => 0x0a,
        Key::K => 0x0b,
        Key::L => 0x0c,
        Key::M => 0x0d,
        Key::N => 0x0e,
        Key::O => 0x0f,
        Key::P => 0x10,
        Key::Q => 0x11,
        Key::R => 0x12,
        Key::S => 0x13,
        Key::T => 0x14,
        Key::U => 0x15,
        Key::V => 0x16,
        Key::W => 0x17,
        Key::X => 0x18,
        Key::Y => 0x19,
        Key::Z => 0x1a,
        Key::OpenBracket => 0x1b,
        Key::Backslash => 0x1c,
        Key::CloseBracket => 0x1d,
        Key::Space => 0x00,
        _ => return None,
    })
}

/// Printable text from an egui text event.
pub fn text_input(text: &str, mods: Modifiers) -> Option<Vec<u8>> {
    if mods.command || mods.ctrl {
        // Control/meta chords are either keybindings or control codes.
        return None;
    }
    if text.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(text.len() + 1);
    if mods.alt {
        out.push(0x1b);
    }
    out.extend_from_slice(text.as_bytes());
    Some(out)
}

/// DOM `KeyboardEvent.key` and `.code` for a browser surface.
pub fn dom_key(key: Key) -> Option<(&'static str, &'static str)> {
    Some(match key {
        Key::ArrowUp => ("ArrowUp", "ArrowUp"),
        Key::ArrowDown => ("ArrowDown", "ArrowDown"),
        Key::ArrowLeft => ("ArrowLeft", "ArrowLeft"),
        Key::ArrowRight => ("ArrowRight", "ArrowRight"),
        Key::Enter => ("Enter", "Enter"),
        Key::Escape => ("Escape", "Escape"),
        Key::Tab => ("Tab", "Tab"),
        Key::Backspace => ("Backspace", "Backspace"),
        Key::Delete => ("Delete", "Delete"),
        Key::Home => ("Home", "Home"),
        Key::End => ("End", "End"),
        Key::PageUp => ("PageUp", "PageUp"),
        Key::PageDown => ("PageDown", "PageDown"),
        Key::Space => (" ", "Space"),
        Key::A => ("a", "KeyA"),
        Key::B => ("b", "KeyB"),
        Key::C => ("c", "KeyC"),
        Key::D => ("d", "KeyD"),
        Key::E => ("e", "KeyE"),
        Key::F => ("f", "KeyF"),
        Key::G => ("g", "KeyG"),
        Key::H => ("h", "KeyH"),
        Key::I => ("i", "KeyI"),
        Key::J => ("j", "KeyJ"),
        Key::K => ("k", "KeyK"),
        Key::L => ("l", "KeyL"),
        Key::M => ("m", "KeyM"),
        Key::N => ("n", "KeyN"),
        Key::O => ("o", "KeyO"),
        Key::P => ("p", "KeyP"),
        Key::Q => ("q", "KeyQ"),
        Key::R => ("r", "KeyR"),
        Key::S => ("s", "KeyS"),
        Key::T => ("t", "KeyT"),
        Key::U => ("u", "KeyU"),
        Key::V => ("v", "KeyV"),
        Key::W => ("w", "KeyW"),
        Key::X => ("x", "KeyX"),
        Key::Y => ("y", "KeyY"),
        Key::Z => ("z", "KeyZ"),
        Key::Num0 => ("0", "Digit0"),
        Key::Num1 => ("1", "Digit1"),
        Key::Num2 => ("2", "Digit2"),
        Key::Num3 => ("3", "Digit3"),
        Key::Num4 => ("4", "Digit4"),
        Key::Num5 => ("5", "Digit5"),
        Key::Num6 => ("6", "Digit6"),
        Key::Num7 => ("7", "Digit7"),
        Key::Num8 => ("8", "Digit8"),
        Key::Num9 => ("9", "Digit9"),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m() -> Modifiers {
        Modifiers::default()
    }

    #[test]
    fn enter_is_cr() {
        assert_eq!(terminal_key(Key::Enter, m()), Some(vec![0x0d]));
    }

    #[test]
    fn ctrl_c_is_etx() {
        let mods = Modifiers { ctrl: true, ..Default::default() };
        assert_eq!(terminal_key(Key::C, mods), Some(vec![0x03]));
    }

    #[test]
    fn arrows() {
        assert_eq!(terminal_key(Key::ArrowUp, m()), Some(b"\x1b[A".to_vec()));
        assert_eq!(terminal_key(Key::ArrowLeft, m()), Some(b"\x1b[D".to_vec()));
    }

    #[test]
    fn shift_tab_is_cbt() {
        let mods = Modifiers { shift: true, ..Default::default() };
        assert_eq!(terminal_key(Key::Tab, mods), Some(b"\x1b[Z".to_vec()));
    }

    #[test]
    fn alt_prefixes_escape() {
        let mods = Modifiers { alt: true, ..Default::default() };
        assert_eq!(text_input("a", mods), Some(b"\x1ba".to_vec()));
    }

    #[test]
    fn ctrl_arrow_modifier_encoding() {
        let mods = Modifiers { ctrl: true, ..Default::default() };
        assert_eq!(terminal_key(Key::ArrowRight, mods), Some(b"\x1b[1;5C".to_vec()));
    }

    #[test]
    fn backspace_and_delete() {
        assert_eq!(terminal_key(Key::Backspace, m()), Some(vec![0x7f]));
        assert_eq!(terminal_key(Key::Delete, m()), Some(b"\x1b[3~".to_vec()));
    }

    #[test]
    fn command_chords_are_not_text() {
        let mods = Modifiers { command: true, ..Default::default() };
        assert_eq!(text_input("d", mods), None);
    }

    #[test]
    fn dom_keys() {
        assert_eq!(dom_key(Key::A), Some(("a", "KeyA")));
        assert_eq!(dom_key(Key::Enter), Some(("Enter", "Enter")));
    }
}
