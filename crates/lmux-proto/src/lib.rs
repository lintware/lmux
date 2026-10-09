//! Shared wire types for lmux.
//!
//! Two channels carry these types:
//! - CLI ⇄ daemon: newline-delimited JSON over the unix socket. Each line is an
//!   [`RpcRequest`]; the daemon answers with one [`RpcResponse`] line.
//! - App ⇄ daemon: one websocket. Text messages are JSON [`ClientMsg`] /
//!   [`ServerMsg`]. Binary messages are frames described in [`frame`].
//!
//! Refs are short, human-typable strings that stay stable for the daemon's
//! lifetime: `folder:1`, `workspace:3`, `pane:7`, `surface:12`.
//!
//! The app mirrors these types by hand in `app/src/proto.ts`. Change both
//! together.

use serde::{Deserialize, Serialize};

pub mod frame;
pub mod methods;

pub const PROTOCOL_VERSION: u32 = 1;

pub type Ref = String;

// ---------------------------------------------------------------------------
// Session tree
// ---------------------------------------------------------------------------

/// Everything the sidebar and layout need. Sent in full on connect and after
/// any structural change (for v1 the daemon may always resend the full tree).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Tree {
    pub host: String,
    pub folders: Vec<Folder>,
    /// Workspaces in sidebar order. A workspace with `folder: None` is shown at top level.
    pub workspaces: Vec<Workspace>,
    pub selected_workspace: Option<Ref>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Folder {
    pub id: Ref,
    pub name: String,
    /// The folder this one sits inside; `None` is a top-level folder.
    #[serde(default)]
    pub parent: Option<Ref>,
    pub collapsed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Workspace {
    pub id: Ref,
    pub title: String,
    pub folder: Option<Ref>,
    pub cwd: String,
    pub git_branch: Option<String>,
    pub agent: AgentState,
    /// Short text for the sidebar card, usually the agent's last message.
    pub summary: Option<String>,
    pub unread_notifications: u32,
    pub status_pills: Vec<StatusPill>,
    pub progress: Option<Progress>,
    pub layout: Layout,
    pub panes: Vec<Pane>,
    pub focused_pane: Option<Ref>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    #[default]
    None,
    Running,
    Idle,
    NeedsInput,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StatusPill {
    pub key: String,
    pub value: String,
    pub icon: Option<String>,
    pub color: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Progress {
    /// 0.0 ..= 1.0
    pub value: f32,
    pub label: Option<String>,
}

/// Binary split tree. Leaves are panes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Layout {
    Pane {
        pane: Ref,
    },
    Split {
        direction: SplitDirection,
        /// Fraction of space given to `first`, 0.0 ..= 1.0.
        ratio: f32,
        first: Box<Layout>,
        second: Box<Layout>,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SplitDirection {
    /// Side by side (first is left).
    Horizontal,
    /// Stacked (first is top).
    Vertical,
}

/// Where to put a new split relative to an existing pane (cmux `new-split <dir>`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SplitSide {
    Left,
    Right,
    Up,
    Down,
}

/// A split region holding one or more surfaces as tabs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Pane {
    pub id: Ref,
    pub surfaces: Vec<Surface>,
    pub active_surface: Option<Ref>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Surface {
    pub id: Ref,
    pub title: String,
    #[serde(flatten)]
    pub kind: SurfaceKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SurfaceKind {
    Terminal {
        /// tmux pane id backing this surface, e.g. `%14`. Informational.
        tmux_pane: Option<String>,
        cols: u16,
        rows: u16,
    },
    Browser {
        url: String,
        loading: bool,
        can_go_back: bool,
        can_go_forward: bool,
    },
}

// ---------------------------------------------------------------------------
// Notifications
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Notification {
    pub id: String,
    pub title: String,
    pub subtitle: Option<String>,
    pub body: Option<String>,
    pub workspace: Option<Ref>,
    pub surface: Option<Ref>,
    /// Unix milliseconds.
    pub created_at: u64,
    pub read: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LogEntry {
    pub level: LogLevel,
    pub source: Option<String>,
    pub message: String,
    pub created_at: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Info,
    Success,
    Warning,
    Error,
}

// ---------------------------------------------------------------------------
// RPC (used by the CLI over the unix socket, and by the app inside ClientMsg::Rpc)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RpcRequest {
    pub id: u64,
    /// One of the constants in [`methods`].
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RpcResponse {
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RpcError {
    pub code: RpcErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RpcErrorCode {
    UnknownMethod,
    InvalidParams,
    NotFound,
    Unsupported,
    Internal,
    /// A close asked with `if_idle` found a program still running; the
    /// message names it.
    Busy,
}

// ---------------------------------------------------------------------------
// App websocket messages
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    Hello {
        protocol_version: u32,
        client_name: String,
    },
    Rpc {
        request: RpcRequest,
    },
    /// Start streaming a surface. The daemon replies with a snapshot
    /// (`term_snapshot`, or for browsers a first frame) and then live updates.
    Subscribe {
        surface: Ref,
    },
    /// Stop streaming. Browser screencasts stop when no client is subscribed.
    Unsubscribe {
        surface: Ref,
    },
    /// Keyboard/paste input for a terminal surface, as typed (UTF-8).
    TermInput {
        surface: Ref,
        data: String,
    },
    TermResize {
        surface: Ref,
        cols: u16,
        rows: u16,
    },
    /// CSS pixel size of the canvas showing a browser surface.
    BrowserResize {
        surface: Ref,
        width: u32,
        height: u32,
        device_scale_factor: f32,
    },
    BrowserInput {
        surface: Ref,
        event: BrowserInputEvent,
    },
    /// Acknowledge a browser frame so the daemon sends the next one.
    BrowserFrameAck {
        surface: Ref,
        seq: u32,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    Welcome {
        protocol_version: u32,
        server_version: String,
        host: String,
    },
    RpcResult {
        response: RpcResponse,
    },
    Tree {
        tree: Tree,
    },
    /// Full screen state for a terminal surface. `data` is ANSI text that
    /// reproduces the screen when written into a fresh terminal of `cols`x`rows`,
    /// including cursor positioning at the end.
    TermSnapshot {
        surface: Ref,
        cols: u16,
        rows: u16,
        data: String,
    },
    /// A terminal surface ended (process exited / closed).
    TermExited {
        surface: Ref,
    },
    BrowserMeta {
        surface: Ref,
        url: String,
        title: String,
        loading: bool,
        can_go_back: bool,
        can_go_forward: bool,
    },
    Notification {
        notification: Notification,
    },
    Log {
        workspace: Option<Ref>,
        entry: LogEntry,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BrowserInputEvent {
    Mouse {
        action: MouseAction,
        /// CSS pixels relative to the page viewport.
        x: f64,
        y: f64,
        button: MouseButton,
        click_count: u32,
        modifiers: Modifiers,
    },
    Wheel {
        x: f64,
        y: f64,
        delta_x: f64,
        delta_y: f64,
        modifiers: Modifiers,
    },
    Key {
        action: KeyAction,
        /// DOM `KeyboardEvent.key`, e.g. "a", "Enter", "ArrowLeft".
        key: String,
        /// DOM `KeyboardEvent.code`, e.g. "KeyA".
        code: String,
        /// Text produced by the key, if any.
        text: Option<String>,
        modifiers: Modifiers,
    },
    /// Committed text from an IME or paste.
    InsertText {
        text: String,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MouseAction {
    Down,
    Up,
    Move,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    None,
    Left,
    Middle,
    Right,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum KeyAction {
    Down,
    Up,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Modifiers {
    #[serde(default)]
    pub alt: bool,
    #[serde(default)]
    pub ctrl: bool,
    #[serde(default)]
    pub meta: bool,
    #[serde(default)]
    pub shift: bool,
}

// ---------------------------------------------------------------------------
// Browser engine ⇄ daemon events
// ---------------------------------------------------------------------------

/// Events the browser engine pushes to the daemon.
///
/// This is not a wire type — the app never sees it, and it carries no serde
/// derives. It lives here because the daemon must not depend on `lmux-browser`
/// (so it builds and tests without Chromium) and the engine must not depend on
/// `lmux-core` (so it stands alone). Sharing one definition keeps those two
/// sides from drifting apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserEvent {
    /// A screencast frame, forwarded to subscribers as a `frame::KIND_BROWSER_FRAME`.
    Frame {
        surface: Ref,
        seq: u32,
        width: u16,
        height: u16,
        jpeg: Vec<u8>,
    },
    /// Page metadata changed.
    Meta {
        surface: Ref,
        url: String,
        title: String,
        loading: bool,
        can_go_back: bool,
        can_go_forward: bool,
    },
    /// The page or its target closed on its own.
    Closed { surface: Ref },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_msg_wire_shape() {
        let msg = ClientMsg::TermInput {
            surface: "surface:3".into(),
            data: "ls\r".into(),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"type": "term_input", "surface": "surface:3", "data": "ls\r"})
        );
    }

    #[test]
    fn surface_kind_is_flattened() {
        let s = Surface {
            id: "surface:1".into(),
            title: "zsh".into(),
            kind: SurfaceKind::Terminal {
                tmux_pane: Some("%1".into()),
                cols: 80,
                rows: 24,
            },
        };
        let json = serde_json::to_value(&s).unwrap();
        assert_eq!(json["kind"], "terminal");
        assert_eq!(json["cols"], 80);
        let back: Surface = serde_json::from_value(json).unwrap();
        assert_eq!(back, s);
    }
}
