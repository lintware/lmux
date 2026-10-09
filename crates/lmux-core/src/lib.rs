//! lmux-core: the daemon's brain.
//!
//! - [`tree`]: folders / workspaces / panes / surfaces and the pure layout ops.
//! - [`tmux`]: the control-mode backend (one window per terminal surface).
//! - [`keys`]: tmux key-name mapping and `%output` octal decoding.
//! - [`browser`]: the [`browser::BrowserBackend`] trait boundary.
//! - [`scrollback`]: saved terminal history, replayed into respawned panes.
//! - [`rpc`]: RPC dispatch over the shared `lmux_proto` methods.
//! - [`daemon`]: the daemon itself — unix socket, websocket server, persistence.

pub mod browser;
pub mod daemon;
pub mod keys;
pub mod probe;
pub mod rpc;
pub mod scrollback;
pub mod tmux;
pub mod tree;

pub use browser::{BrowserBackend, BrowserEvent};
pub use daemon::{run, Config, Inner};
pub use tmux::{Tmux, TmuxConfig, TmuxEvent};
pub use tree::SessionState;

use std::path::PathBuf;

/// Default state directory: `$LMUX_STATE_DIR`, else `$XDG_STATE_HOME/lmux`, else
/// `~/.local/state/lmux`.
pub fn default_state_dir() -> PathBuf {
    if let Ok(d) = std::env::var("LMUX_STATE_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    if let Ok(d) = std::env::var("XDG_STATE_HOME") {
        if !d.is_empty() {
            return PathBuf::from(d).join("lmux");
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".local/state/lmux")
}

pub fn default_ws_addr() -> String {
    "127.0.0.1:7878".to_string()
}
