//! Terminal input encoders that depend on the program's terminal modes.
//! Pure functions, unit-tested here; the UI decides when to call them.

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
}
