//! Terminal input encoders that depend on the program's terminal modes:
//! bracketed paste and mouse reports (SGR / UTF-8 / X10). Pure functions,
//! unit-tested here; the UI decides when to call them.

use crate::term::Modes;

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
pub fn mouse_reporting(mode: Modes) -> bool {
    mode.mouse_reporting()
}

/// Whether the program wants this kind of event at all.
pub fn wants(mode: Modes, action: MouseAction, button: MouseButton) -> bool {
    match action {
        MouseAction::Press | MouseAction::Release => mouse_reporting(mode),
        MouseAction::Motion if button == MouseButton::None => mode.mouse_motion,
        MouseAction::Motion => mode.mouse_drag || mode.mouse_motion,
    }
}

/// Encode one mouse event at the zero-based cell `(col, row)`.
///
/// SGR (`ESC[<b;x;yM`/`m`) when the program enabled it, else UTF-8 coordinates
/// when it asked for those, else classic X10 (`ESC[M` + three bytes), whose
/// coordinates stop at 223: an event past that is dropped, as xterm does.
pub fn mouse_report(
    mode: Modes,
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

    if mode.sgr_mouse {
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
    if mode.utf8_mouse {
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

/// A URL found in a line of text: columns `start..end` (exclusive).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UrlSpan {
    pub start: usize,
    pub end: usize,
    pub url: String,
}

fn url_char(c: char) -> bool {
    c.is_ascii_graphic() && !matches!(c, '<' | '>' | '"' | '`' | '{' | '}' | '|' | '\\' | '^')
}

/// Every http(s) URL in `line`, one char per terminal column.
pub fn find_urls(line: &[char]) -> Vec<UrlSpan> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < line.len() {
        let rest = &line[i..];
        let scheme = if starts_with(rest, "https://") {
            8
        } else if starts_with(rest, "http://") {
            7
        } else {
            i += 1;
            continue;
        };
        let mut end = i + scheme;
        while end < line.len() && url_char(line[end]) {
            end += 1;
        }
        // Trailing punctuation belongs to the sentence, not the URL; a closing
        // bracket stays only when the URL opened one.
        while end > i + scheme {
            let count = |ch: char| line[i..end].iter().filter(|&&x| x == ch).count();
            let keep = match line[end - 1] {
                ')' => count('(') >= count(')'),
                ']' => count('[') >= count(']'),
                '.' | ',' | ';' | ':' | '!' | '?' | '\'' => false,
                _ => true,
            };
            if keep {
                break;
            }
            end -= 1;
        }
        if end > i + scheme {
            out.push(UrlSpan { start: i, end, url: line[i..end].iter().collect() });
        }
        i = end.max(i + 1);
    }
    out
}

/// The URL covering column `col` of `line`, if any.
pub fn url_at(line: &[char], col: usize) -> Option<UrlSpan> {
    find_urls(line).into_iter().find(|u| (u.start..u.end).contains(&col))
}

fn starts_with(chars: &[char], prefix: &str) -> bool {
    chars.len() >= prefix.len() && chars.iter().zip(prefix.chars()).all(|(a, b)| a.eq_ignore_ascii_case(&b))
}

/// Whether an OSC 8 hyperlink's target is safe to hand to `open`: web and
/// mail links only, so a program cannot make a click launch a local file.
pub fn openable_link(uri: &str) -> bool {
    let lower = uri.to_ascii_lowercase();
    ["http://", "https://", "mailto:"].iter().any(|s| lower.starts_with(s))
}

thread_local! {
    static URL_OPENER: std::cell::RefCell<Option<Box<dyn Fn(&str)>>> = const { std::cell::RefCell::new(None) };
}

/// Replace how URLs are opened on this thread (tests record them instead of
/// launching a browser).
pub fn set_url_opener(f: Option<Box<dyn Fn(&str)>>) {
    URL_OPENER.with(|o| *o.borrow_mut() = f);
}

/// Open `url` in the default browser (`open` on macOS, `xdg-open` elsewhere).
pub fn open_url(url: &str) {
    let hooked = URL_OPENER.with(|o| match o.borrow().as_ref() {
        Some(f) => {
            f(url);
            true
        }
        None => false,
    });
    if hooked {
        return;
    }
    let cmd = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    if let Err(e) = std::process::Command::new(cmd).arg(url).spawn() {
        eprintln!("[url] {cmd} {url}: {e}");
    }
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

    fn sgr() -> Modes {
        Modes { mouse_click: true, sgr_mouse: true, ..Modes::default() }
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
        let drag = Modes { mouse_drag: true, ..sgr() };
        assert_eq!(
            mouse_report(drag, MouseAction::Motion, MouseButton::Left, 1, 1, m),
            Some(b"\x1b[<32;2;2M".to_vec())
        );
        assert_eq!(mouse_report(drag, MouseAction::Motion, MouseButton::None, 1, 1, m), None);
        let motion = Modes { mouse_motion: true, ..sgr() };
        assert_eq!(
            mouse_report(motion, MouseAction::Motion, MouseButton::None, 1, 1, m),
            Some(b"\x1b[<35;2;2M".to_vec())
        );
    }

    #[test]
    fn x10_encoding() {
        let mode = Modes { mouse_click: true, ..Modes::default() };
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
        assert_eq!(mouse_report(Modes::default(), MouseAction::Press, MouseButton::Left, 0, 0, m), None);
    }

    #[test]
    fn utf8_mouse_encodes_large_columns() {
        let mode = Modes { mouse_click: true, utf8_mouse: true, ..Modes::default() };
        let out = mouse_report(mode, MouseAction::Press, MouseButton::Left, 299, 0, egui::Modifiers::default())
            .unwrap();
        let s = String::from_utf8(out).unwrap();
        assert_eq!(s.chars().nth(4), Some(char::from_u32(300 + 32).unwrap()));
    }

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    #[test]
    fn finds_urls() {
        let line = chars("see https://example.com/a?b=1, and (http://x.org/p_(1)).");
        let urls = find_urls(&line);
        assert_eq!(urls.len(), 2);
        assert_eq!(urls[0].url, "https://example.com/a?b=1");
        assert_eq!(urls[1].url, "http://x.org/p_(1)");
        assert_eq!(url_at(&line, 4).map(|u| u.url), Some("https://example.com/a?b=1".into()));
        assert_eq!(url_at(&line, 0), None);
        assert_eq!(url_at(&line, urls[0].end), None);
    }

    #[test]
    fn a_bare_scheme_is_not_a_url() {
        assert!(find_urls(&chars("https:// nothing")).is_empty());
        assert!(find_urls(&chars("http")).is_empty());
    }

    #[test]
    fn only_web_and_mail_links_open() {
        assert!(openable_link("https://example.com"));
        assert!(openable_link("mailto:a@b.c"));
        assert!(!openable_link("file:///Applications/Calculator.app"));
    }
}
