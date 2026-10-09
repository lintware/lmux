//! Binary websocket frames (daemon → app).
//!
//! Layout, all integers big-endian:
//!
//! ```text
//! [kind: u8][surface_len: u8][surface: surface_len bytes UTF-8][body...]
//! ```
//!
//! - `KIND_TERM_OUTPUT`: body is raw terminal output bytes, to be written into
//!   the surface's terminal emulator as-is.
//! - `KIND_BROWSER_FRAME`: body is
//!   `[seq: u32][width: u16][height: u16][jpeg bytes...]`. The app shows it and
//!   replies with `ClientMsg::BrowserFrameAck { seq }`.

pub const KIND_TERM_OUTPUT: u8 = 1;
pub const KIND_BROWSER_FRAME: u8 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame<'a> {
    TermOutput {
        surface: &'a str,
        data: &'a [u8],
    },
    BrowserFrame {
        surface: &'a str,
        seq: u32,
        width: u16,
        height: u16,
        jpeg: &'a [u8],
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    Truncated,
    UnknownKind(u8),
    SurfaceTooLong,
    BadSurfaceUtf8,
}

impl<'a> Frame<'a> {
    pub fn encode(&self) -> Result<Vec<u8>, FrameError> {
        let (kind, surface) = match self {
            Frame::TermOutput { surface, .. } => (KIND_TERM_OUTPUT, *surface),
            Frame::BrowserFrame { surface, .. } => (KIND_BROWSER_FRAME, *surface),
        };
        let surface_len = u8::try_from(surface.len()).map_err(|_| FrameError::SurfaceTooLong)?;
        let mut out = Vec::with_capacity(2 + surface.len() + 16);
        out.push(kind);
        out.push(surface_len);
        out.extend_from_slice(surface.as_bytes());
        match self {
            Frame::TermOutput { data, .. } => out.extend_from_slice(data),
            Frame::BrowserFrame {
                seq,
                width,
                height,
                jpeg,
                ..
            } => {
                out.extend_from_slice(&seq.to_be_bytes());
                out.extend_from_slice(&width.to_be_bytes());
                out.extend_from_slice(&height.to_be_bytes());
                out.extend_from_slice(jpeg);
            }
        }
        Ok(out)
    }

    pub fn decode(buf: &'a [u8]) -> Result<Self, FrameError> {
        let (&kind, rest) = buf.split_first().ok_or(FrameError::Truncated)?;
        let (&surface_len, rest) = rest.split_first().ok_or(FrameError::Truncated)?;
        let surface_len = surface_len as usize;
        if rest.len() < surface_len {
            return Err(FrameError::Truncated);
        }
        let (surface, body) = rest.split_at(surface_len);
        let surface = std::str::from_utf8(surface).map_err(|_| FrameError::BadSurfaceUtf8)?;
        match kind {
            KIND_TERM_OUTPUT => Ok(Frame::TermOutput { surface, data: body }),
            KIND_BROWSER_FRAME => {
                if body.len() < 8 {
                    return Err(FrameError::Truncated);
                }
                let seq = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
                let width = u16::from_be_bytes([body[4], body[5]]);
                let height = u16::from_be_bytes([body[6], body[7]]);
                Ok(Frame::BrowserFrame {
                    surface,
                    seq,
                    width,
                    height,
                    jpeg: &body[8..],
                })
            }
            other => Err(FrameError::UnknownKind(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let frames = [
            Frame::TermOutput {
                surface: "surface:4",
                data: b"\x1b[31mhi\r\n",
            },
            Frame::BrowserFrame {
                surface: "surface:9",
                seq: 7,
                width: 1280,
                height: 800,
                jpeg: &[0xff, 0xd8, 0xff],
            },
        ];
        for f in frames {
            let bytes = f.encode().unwrap();
            assert_eq!(Frame::decode(&bytes).unwrap(), f);
        }
    }
}
