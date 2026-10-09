//! Terminal input encoders that depend on the program's terminal modes:
//! bracketed paste and mouse reports (SGR / UTF-8 / X10). Pure functions,
//! unit-tested here; the UI decides when to call them.

use alacritty_terminal::term::TermMode;

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

/// The bytes a paste of `text` writes to the terminal.
///
/// With bracketed paste on, the text is wrapped in `ESC[200~ … ESC[201~` and
/// any `ESC[201~` inside it is removed, so pasted text cannot end the bracket
/// early and have the rest run as typed input. Without it, line feeds become
/// carriage returns, which is what the Return key sends.
pub fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        let mut inner = text.to_owned();
        while inner.contains("\x1b[201~") {
            inner = inner.replace("\x1b[201~", "");
        }
        let mut out = Vec::with_capacity(inner.len() + PASTE_START.len() + PASTE_END.len());
        out.extend_from_slice(PASTE_START);
        out.extend_from_slice(inner.as_bytes());
        out.extend_from_slice(PASTE_END);
        out
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    }
}

/// Which mouse button an event is about, in the protocol's numbering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
    /// Motion with no button held (`MOUSE_MOTION` only).
    None,
}

impl MouseButton {
    fn code(self) -> u8 {
        match self {
            MouseButton::Left => 0,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
            MouseButton::None => 3,
            MouseButton::WheelUp => 64,
            MouseButton::WheelDown => 65,
        }
    }

    pub fn from_egui(b: egui::PointerButton) -> Option<Self> {
        match b {
            egui::PointerButton::Primary => Some(MouseButton::Left),
            egui::PointerButton::Middle => Some(MouseButton::Middle),
            egui::PointerButton::Secondary => Some(MouseButton::Right),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseAction {
    Press,
    Release,
    Motion,
}

/// True when the program asked for mouse reports of any kind.
pub fn mouse_reporting(mode: TermMode) -> bool {
    mode.intersects(TermMode::MOUSE_MODE)
}

/// Whether the program wants this kind of event at all.
pub fn wants(mode: TermMode, action: MouseAction, button: MouseButton) -> bool {
    match action {
        MouseAction::Press | MouseAction::Release => mouse_reporting(mode),
        MouseAction::Motion if button == MouseButton::None => mode.contains(TermMode::MOUSE_MOTION),
        MouseAction::Motion => mode.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION),
    }
}

/// Encode one mouse event at the zero-based cell `(col, row)`.
///
/// SGR (`ESC[<b;x;yM`/`m`) when the program enabled it, else UTF-8 coordinates
/// when it asked for those, else classic X10 (`ESC[M` + three bytes), whose
/// coordinates stop at 223: an event past that is dropped, as xterm does.
pub fn mouse_report(
    mode: TermMode,
    action: MouseAction,
    button: MouseButton,
    col: usize,
    row: usize,
    mods: egui::Modifiers,
) -> Option<Vec<u8>> {
    if !wants(mode, action, button) {
        return None;
    }
    let mut b = button.code() as u32;
    if action == MouseAction::Motion {
        b += 32;
    }
    if mods.shift {
        b += 4;
    }
    if mods.alt {
        b += 8;
    }
    if mods.ctrl {
        b += 16;
    }
    let (x, y) = (col as u32 + 1, row as u32 + 1);

    if mode.contains(TermMode::SGR_MOUSE) {
        let fin = if action == MouseAction::Release { 'm' } else { 'M' };
        return Some(format!("\x1b[<{b};{x};{y}{fin}").into_bytes());
    }
    // Wheel "releases" do not exist in the legacy encodings.
    if action == MouseAction::Release && matches!(button, MouseButton::WheelUp | MouseButton::WheelDown) {
        return None;
    }
    // The legacy encodings cannot say which button went up.
    if action == MouseAction::Release {
        b = (b & !0b11) | 3;
    }
    let mut out = b"\x1b[M".to_vec();
    if mode.contains(TermMode::UTF8_MOUSE) {
        for v in [b + 32, x + 32, y + 32] {
            let ch = char::from_u32(v)?;
            let mut buf = [0u8; 4];
            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
        }
        return Some(out);
    }
    if x > 223 || y > 223 {
        return None;
    }
    out.push((b + 32) as u8);
    out.push((x + 32) as u8);
    out.push((y + 32) as u8);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bracketed_paste_wraps() {
        assert_eq!(paste_bytes("ls\n", true), b"\x1b[200~ls\n\x1b[201~".to_vec());
    }

    #[test]
    fn bracketed_paste_strips_the_end_marker() {
        let evil = "echo hi\x1b[201~rm -rf ~\n";
        assert_eq!(paste_bytes(evil, true), b"\x1b[200~echo hirm -rf ~\n\x1b[201~".to_vec());
        // Removing one marker must not assemble another.
        let nested = "a\x1b[20\x1b[201~1~b";
        let out = paste_bytes(nested, true);
        let inner = &out[6..out.len() - 6];
        assert!(!inner.windows(6).any(|w| w == b"\x1b[201~"));
    }

    #[test]
    fn plain_paste_turns_newlines_into_returns() {
        assert_eq!(paste_bytes("a\nb\r\nc", false), b"a\rb\rc".to_vec());
    }

    fn sgr() -> TermMode {
        TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE
    }

    #[test]
    fn sgr_press_and_release() {
        let m = egui::Modifiers::default();
        assert_eq!(
            mouse_report(sgr(), MouseAction::Press, MouseButton::Left, 4, 2, m),
            Some(b"\x1b[<0;5;3M".to_vec())
        );
        assert_eq!(
            mouse_report(sgr(), MouseAction::Release, MouseButton::Left, 4, 2, m),
            Some(b"\x1b[<0;5;3m".to_vec())
        );
        assert_eq!(
            mouse_report(sgr(), MouseAction::Press, MouseButton::Right, 0, 0, m),
            Some(b"\x1b[<2;1;1M".to_vec())
        );
    }

    #[test]
    fn sgr_wheel_and_modifiers() {
        let ctrl = egui::Modifiers { ctrl: true, ..Default::default() };
        assert_eq!(
            mouse_report(sgr(), MouseAction::Press, MouseButton::WheelUp, 9, 9, egui::Modifiers::default()),
            Some(b"\x1b[<64;10;10M".to_vec())
        );
        assert_eq!(
            mouse_report(sgr(), MouseAction::Press, MouseButton::WheelDown, 0, 0, ctrl),
            Some(b"\x1b[<81;1;1M".to_vec())
        );
    }

    #[test]
    fn motion_needs_drag_or_motion_mode() {
        let m = egui::Modifiers::default();
        assert_eq!(mouse_report(sgr(), MouseAction::Motion, MouseButton::Left, 1, 1, m), None);
        let drag = sgr() | TermMode::MOUSE_DRAG;
        assert_eq!(
            mouse_report(drag, MouseAction::Motion, MouseButton::Left, 1, 1, m),
            Some(b"\x1b[<32;2;2M".to_vec())
        );
        assert_eq!(mouse_report(drag, MouseAction::Motion, MouseButton::None, 1, 1, m), None);
        let motion = sgr() | TermMode::MOUSE_MOTION;
        assert_eq!(
            mouse_report(motion, MouseAction::Motion, MouseButton::None, 1, 1, m),
            Some(b"\x1b[<35;2;2M".to_vec())
        );
    }

    #[test]
    fn x10_encoding() {
        let mode = TermMode::MOUSE_REPORT_CLICK;
        let m = egui::Modifiers::default();
        assert_eq!(
            mouse_report(mode, MouseAction::Press, MouseButton::Left, 0, 0, m),
            Some(vec![0x1b, b'[', b'M', 32, 33, 33])
        );
        // Release is button 3.
        assert_eq!(
            mouse_report(mode, MouseAction::Release, MouseButton::Left, 2, 1, m),
            Some(vec![0x1b, b'[', b'M', 35, 35, 34])
        );
        assert_eq!(
            mouse_report(mode, MouseAction::Press, MouseButton::WheelDown, 0, 0, m),
            Some(vec![0x1b, b'[', b'M', 97, 33, 33])
        );
        assert_eq!(mouse_report(mode, MouseAction::Release, MouseButton::WheelDown, 0, 0, m), None);
        assert_eq!(mouse_report(mode, MouseAction::Press, MouseButton::Left, 300, 0, m), None);
        assert_eq!(mouse_report(TermMode::empty(), MouseAction::Press, MouseButton::Left, 0, 0, m), None);
    }

    #[test]
    fn utf8_mouse_encodes_large_columns() {
        let mode = TermMode::MOUSE_REPORT_CLICK | TermMode::UTF8_MOUSE;
        let out = mouse_report(mode, MouseAction::Press, MouseButton::Left, 299, 0, egui::Modifiers::default())
            .unwrap();
        let s = String::from_utf8(out).unwrap();
        assert_eq!(s.chars().nth(4), Some(char::from_u32(300 + 32).unwrap()));
    }
}
