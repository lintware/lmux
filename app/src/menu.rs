//! The macOS menu bar: lmux, Workspace, Edit, Tab, View, Window and Help.
//!
//! Each item's id is a command-palette name, so a pick runs through
//! [`crate::app::run_command`] like the palette does. A menu item's shortcut
//! takes the key before the window sees it, so items only carry shortcuts
//! that do the same thing everywhere; Cmd+R, Cmd+[ ], Cmd+L, Cmd+C and Cmd+V
//! depend on whether a browser or a terminal has focus and stay with the
//! window's own key handling.

use std::sync::Mutex;

/// Picks waiting for the next frame.
static PICKED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Menu ids that are not palette commands.
pub const OPEN_PALETTE: &str = "lmux:palette";
pub const GO_TO_WORKSPACE: &str = "lmux:goto";
pub const WEBSITE: &str = "lmux:website";

/// The installed menu bar; dropping it removes the items.
pub struct MenuBar {
    #[cfg(target_os = "macos")]
    _menu: muda::Menu,
}

/// Install the menu bar. Picks wake `ctx` and come back from [`take_picks`].
#[cfg(target_os = "macos")]
pub fn install(ctx: &egui::Context) -> Option<MenuBar> {
    use muda::{AboutMetadata, Menu, MenuItem, PredefinedMenuItem as P, Submenu};

    let wake = ctx.clone();
    muda::MenuEvent::set_event_handler(Some(move |e: muda::MenuEvent| {
        if let Ok(mut q) = PICKED.lock() {
            q.push(e.id.0);
        }
        wake.request_repaint();
    }));

    let item = |name: &str, accel: Option<&str>| MenuItem::with_id(name, name, true, accel.and_then(|a| a.parse().ok()));
    let named = |id: &str, title: &str, accel: Option<&str>| {
        MenuItem::with_id(id, title, true, accel.and_then(|a| a.parse().ok()))
    };

    let about = AboutMetadata {
        name: Some("lmux".into()),
        version: Some(env!("CARGO_PKG_VERSION").into()),
        website: Some("https://lmux.lintlabs.org".into()),
        ..Default::default()
    };
    let app = Submenu::with_items(
        "lmux",
        true,
        &[
            &P::about(Some("About lmux"), Some(about)),
            &P::separator(),
            &item("Add Remote Host…", None),
            &item("New Cloud Box…", None),
            &item("Import from cmux…", None),
            &item("Show QR…", None),
            &P::separator(),
            &P::services(None),
            &P::separator(),
            &P::hide(None),
            &P::hide_others(None),
            &P::show_all(None),
            &P::separator(),
            &P::quit(Some("Quit lmux")),
        ],
    )
    .ok()?;
    let workspace = Submenu::with_items(
        "Workspace",
        true,
        &[
            &item("New Workspace", Some("Cmd+N")),
            &item("New Folder", None),
            &P::separator(),
            &item("Rename Workspace", Some("Cmd+Shift+R")),
            &item("Close Workspace", Some("Cmd+Shift+W")),
            &P::separator(),
            &item("Next Workspace", Some("Cmd+Ctrl+]")),
            &item("Previous Workspace", Some("Cmd+Ctrl+[")),
            &named(GO_TO_WORKSPACE, "Go to Workspace…", Some("Cmd+P")),
        ],
    )
    .ok()?;
    // A native WebKit page (the in-app browser) gets Cmd+C, Cmd+V, Cmd+A and
    // Cmd+Z only from an Edit menu: AppKit sends `copy:`, `paste:` and the rest
    // down the responder chain. muda turns auto-enabling off, so these items
    // stay enabled whatever has focus and their shortcuts take the key before
    // the window sees it: `webview::sync_focus` keeps the page from being the
    // first responder while a terminal is focused, and `clip::watch_cmd_v`
    // sees Cmd+V first.
    let edit = Submenu::with_items(
        "Edit",
        true,
        &[
            &P::undo(None),
            &P::redo(None),
            &P::separator(),
            &P::cut(None),
            &P::copy(None),
            &P::paste(None),
            &P::select_all(None),
        ],
    )
    .ok()?;
    let tab = Submenu::with_items(
        "Tab",
        true,
        &[
            &item("New Terminal Tab", Some("Cmd+T")),
            &item("New Browser Tab", Some("Cmd+Shift+B")),
            &P::separator(),
            &item("Split Right", Some("Cmd+D")),
            &item("Split Down", Some("Cmd+Shift+D")),
            &item("Toggle Pane Zoom", Some("Cmd+Shift+Enter")),
            &P::separator(),
            &item("Rename Tab", None),
            &item("Next Tab", Some("Cmd+Shift+]")),
            &item("Previous Tab", Some("Cmd+Shift+[")),
            &P::separator(),
            &item("Close Tab", Some("Cmd+W")),
        ],
    )
    .ok()?;
    let view = Submenu::with_items(
        "View",
        true,
        &[
            &named(OPEN_PALETTE, "Command Palette…", Some("Cmd+Shift+P")),
            &P::separator(),
            &item("Toggle Sidebar", Some("Cmd+B")),
            &item("Show Notifications", Some("Cmd+I")),
            &item("Jump to Latest Unread", Some("Cmd+Shift+U")),
            &P::separator(),
            &item("Zoom In", Some("Cmd+=")),
            &item("Zoom Out", Some("Cmd+-")),
            &item("Reset Zoom", Some("Cmd+0")),
            &P::separator(),
            &item("Sidebar Text Larger", None),
            &item("Sidebar Text Smaller", None),
            &item("Reset Sidebar Text", None),
            &P::separator(),
            &P::fullscreen(None),
        ],
    )
    .ok()?;
    let window = Submenu::with_items("Window", true, &[&P::minimize(None), &P::maximize(None)]).ok()?;
    let help = Submenu::with_items("Help", true, &[&named(WEBSITE, "lmux Website", None)]).ok()?;

    let menu = Menu::with_items(&[&app, &workspace, &edit, &tab, &view, &window, &help]).ok()?;
    menu.init_for_nsapp();
    window.set_as_windows_menu_for_nsapp();
    help.set_as_help_menu_for_nsapp();
    Some(MenuBar { _menu: menu })
}

#[cfg(not(target_os = "macos"))]
pub fn install(_ctx: &egui::Context) -> Option<MenuBar> {
    None
}

/// Menu picks since the last call, oldest first.
pub fn take_picks() -> Vec<String> {
    PICKED.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default()
}
