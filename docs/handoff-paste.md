# Handoff: "I can't paste text into the lmux browser anymore"

Written 2026-10-13 by the agent that was stopped partway. Nothing is fixed
yet. The only edits in the tree are temporary debug prints (see section 3).

## 1. The task

The user cannot paste text into the lmux browser. Find the cause, fix it,
and verify it live. Then check that problems.md items 3, 4 and 5 (file and
image paste into a browser tab, OSC 52 copy from remote panes, Edit menu
paste into the in-app WebKit browser) still work, and fix any regression.
Only touch paste, copy and clipboard input. Another agent is working on
browser streaming (`crates/lmux-browser/`, `app/src/net.rs`,
`app/src/webview.rs` proxy setup, `app/src/ui/browser.rs` frame drawing,
daemon browser routing). Keep edits small and in the input and clipboard
code. Do not commit. Do not revert or reformat the user's ~60 modified files.

## 2. Root cause: what is known

### 2a. In-app WebKit browser (local host, daemon with `--app-browser`)

- CONFIRMED: once a WKWebView exists, it is the window's first responder,
  even when a terminal pane has lmux focus. Debug log from the instrumented
  app, on a Cmd+V keydown:
  `[dbg] cmd keydown Some("v") first_responder="..NSKVONotifying_wry::wkwebview::class::wry_web_view::WryWebView0.57.0"`
  The cause is wry 0.57 `wkwebview/mod.rs:689`
  (`ns_window.makeFirstResponder(Some(&webview))` on build, `focused`
  defaults to true). Nothing in lmux calls `WebView::focus_parent()` or
  `WebView::focus()`. `grep -rn "focus_parent\|makeFirstResponder" app/src`
  finds nothing.
- CONFIRMED (live, unfixed build, screenshots
  `/tmp/lmux-pd-2-term-before.png` and `/tmp/lmux-pd-3.png`): with
  pane:1 (a terminal) focused through `lmux focus-pane pane:1`, Cmd+V pasted
  into the page's `<input>` and not into the terminal. `lmux browser eval`
  showed the input value `PASTE_TERM_OKTERM2` and the terminal stayed empty.
  So a terminal next to an in-app browser loses paste. Typed keys also went
  to the page; the terminal got nothing.
- CONFIRMED: after the browser tab was closed (`lmux close-surface`), typing
  reached the terminal again (`echo A` appeared).
- SUSPECTED, NOT CONFIRMED: Cmd+V into the in-app page itself. In the first
  run (webview first responder) paste into the page DID work. So the
  in-app tab is not broken when it is the only thing focused. The bug the user
  sees may be the reverse case: after clicking a terminal or egui, the winit
  view becomes first responder (winit's `set_style_mask` and mouse clicks can
  do this), and a click back in the page may not give the WKWebView first
  responder again. The page's click only posts `Event::Clicked` over IPC
  (`CLICK_SCRIPT` in webview.rs). It never calls `webview.focus()`.
  Not tested yet.
- Edit menu state read with System Events (identical in the user's
  installed app and in the test build):
  Undo, Redo, Cut, Copy, Paste, Select All all `enabled=true`. muda sets
  `setAutoenablesItems(false)` on every NSMenu
  (`muda-0.20.0/src/platform_impl/macos/mod.rs:123,223,652`). So the comment
  in `app/src/menu.rs` ("AppKit disables these items, and the key reaches the
  window as before") is WRONG: the items are never auto-disabled. Whether
  the menu's `paste:` then eats Cmd+V before winit sees it with a terminal as
  first responder is SUSPECTED, not confirmed. With no webview, a test Cmd+V
  into the terminal (`PASTED6`) did NOT appear, while typed text did. That
  run also had focus-stealing trouble (see traps), so repeat it.

### 2b. Streamed Chromium browser (remote hosts, or a local daemon without `--app-browser`)

- CONFIRMED: Cmd+V into a streamed tab leaves the field empty, and right
  after it the browser hangs. `lmux browser surface:2 eval ...` never
  returns. Chromium's own DevTools HTTP endpoint
  (`curl -m5 http://127.0.0.1:<DevToolsActivePort>/json/list`) timed out
  (rc=28). Chromium's main process ran at ~85% CPU.
- `sample` of Chromium's main thread (`/tmp/lmux-pc-chrome.txt`, may be
  gone): it sits in
  `-[NSApplication sendEvent:] -> routeKeyEquivalent -> -[NSWindow _commonPerformKeyEquivalent:] -> -[NSView performKeyEquivalent:] -> ChromeMain...`.
  So headless Chromium (`--headless=new`) on macOS is busy in its own
  key-equivalent path. SUSPECTED trigger: the app sends a raw
  `Key{ key:"v", modifiers.meta:true }` (from `keys::dom_key`) next to the
  `InsertText`. With Meta held, Chromium runs its Cmd+V key equivalent
  (paste from its own pasteboard, which is the Mac's pasteboard when the daemon
  runs locally) and stalls. NOT confirmed which event triggers it, and not
  confirmed on a remote Linux host. On Linux, Meta+V does not mean paste, so
  remote behaviour may differ.
- CONFIRMED: the daemon side of `InsertText` works. With no app attached,
  `lmux browser surface:2 keyboard --text hello` put `hello` in the field.
- CONFIRMED: single typed characters from the app work (`x`, `yz` landed).
  One early run with `keystroke "abc"` gave only `ab` and then hung, but that
  run may have had a Cmd still held. Treat it as unclear.
- Pasteboard types for a `pbcopy` text copy (`osascript -e 'clipboard info'`):
  `«class utf8», «class ut16», string, Unicode text`. There is no file URL,
  so `clipboard_files()` should be empty for plain text. The "files suppress
  InsertText" lead is probably not the cause. It is not proven by a debug line
  yet, because the instrumented streamed run never happened.

## 3. Changes in the tree (mine)

All of them are TEMPORARY DEBUG code. Remove them before finishing:

- `app/src/clip.rs`, in `watch_cmd_v`'s block: an `if std::env::var("LMUX_DEBUG_INPUT")...`
  block that prints `[dbg] cmd keydown ... first_responder=...`. It uses
  `e.window(objc2::MainThreadMarker::new().unwrap())`.
- `app/src/app.rs`, `route_input`, terminal branch (~line 1114): an
  `LMUX_DEBUG_INPUT` block printing `[dbg] term ev ...`.
- `app/src/app.rs`, `route_input`, browser branch (~line 1211): an
  `LMUX_DEBUG_INPUT` block printing `[dbg] browser cmd_v=... files=... events=...`.

Remove them by hand (search for `LMUX_DEBUG_INPUT`). Do NOT restore from
`/tmp/lmux-pd-app.rs.orig` or `/tmp/lmux-pd-clip.rs.orig`: the parent agent
may have edited app.rs since those copies were taken.

No real fix has been written. No test has been added.

Overlap with the parent's remote-WebKit work (`app/src/socks.rs`,
`app/src/webview.rs` set_network/proxied_store/page_url, `app/src/net.rs`
socks_port/draws_browsers, `app/src/app.rs` set_network calls): my debug
edits in app.rs are only inside `route_input`, not near the `set_network`
calls (~lines 277 and 393). No overlap. NOTE: the parent's change makes ssh
hosts' browser tabs in-app WebKit by default (`Net::draws_browsers`, unless
`stream_browsers: true` in hosts.json or `LMUX_STREAM_BROWSERS=1`). So the
first-responder bug in 2a now hits remote hosts too, and the fix will touch
`webview.rs` (focus calls). Keep that edit small and away from the proxy code.

## 4. Tests and live checks so far

- No `cargo test` run yet.
- `LMUX_SKIP_INSTALL=1 deploy/macos/bundle.sh` built
  `target/bundle/lmux.app` (unfixed code) and passed. Log `/tmp/lmux-paste-bundle1.log`.
- `cd app && cargo build --release` with the debug prints passed.
- Screenshots (in /tmp, may be cleaned by the OS):
  - `/tmp/lmux-pd-1.png`: test app, terminal + in-app browser, nothing typed.
  - `/tmp/lmux-pd-2-term-before.png`: Cmd+V meant for the terminal landed in the page input.
  - `/tmp/lmux-pd-3.png`: same, after `lmux focus-pane pane:1`; the paste still went to the page.
  - `/tmp/lmux-pd-4.png`: after a synthetic click on the terminal (the click had no effect).
  - `/tmp/lmux-pc-1.png`, `/tmp/lmux-pc-2.png`: streamed Chromium tab with an empty field after Cmd+V.

## 5. What is left (ordered)

1. Remove nothing yet. Rebuild the instrumented app if needed:
   `cd /Users/vas/Documents/lmux/app && cargo build --release`.
2. Start an in-app test setup: `/tmp/lmux-pd-start.sh pd` (recreate it from
   section 6 if /tmp was cleaned). Prints the test app pid.
3. Confirm 2a cases with `LMUX_DEBUG_INPUT=1` logs (`grep dbg /tmp/lmux-pd-app.log`):
   a) page focused, Cmd+V -> lands in page? b) terminal focused -> lands in terminal?
   c) after (b), focus the page again (`lmux focus-pane pane:2`) -> lands in page?
4. Fix focus so it follows the pane (suggested, small):
   - In `webview.rs` `build`, add `.with_focused(false)` so a new or parked
     view does not take first responder.
   - In `Webviews::place` (or a small new `Webviews::sync_focus(host, focused_surface)`
     called from app.rs next to `place`), when the focused pane's active surface is
     an in-app browser on screen, call `v.webview.focus()` once on change. When it
     is not, call `focus_parent()` once on change. Track the last focused key so it
     runs only on change. Also call `focus()` on `Event::Clicked`.
   - Fix the wrong comment in `menu.rs` about AppKit disabling the items.
   - Check Cmd+V in a terminal with a webview present. If the Edit menu's `paste:`
     still swallows it when winit is first responder, look at `watch_cmd_v`
     (`clip.rs`): a local monitor sees the key before the menu. Do NOT drop the
     Edit menu; item 5 needs it.
5. Streamed Chromium (2b): run `/tmp/lmux-pd-start.sh pc` (port 7898, Chromium
   from `~/Library/Caches/ms-playwright`; pass `LMUX_STREAM_BROWSERS=1` if the
   host is treated as draws_browsers). Read the `[dbg] browser ...` line on
   Cmd+V. Likely fix: in `route_input`'s browser branch, do not send the raw
   `Key` event for V (and C/X) when `modifiers.command` is set and a
   Paste/Copy/Cut event is handled this frame. `InsertText` does the paste.
   Put the choice in a small pure function and add a unit test that fails
   without the fix (for example in `app/tests/browser_input.rs`, pushing
   `Event::Key{V, command}` + `Event::Paste("x")` and asserting `pending` has
   one `InsertText("x")` and no Meta+V `Key`). Check
   `crates/lmux-browser/src/lib.rs` ~573 only to read; it belongs to the other agent.
6. Remove all `LMUX_DEBUG_INPUT` code. Run `(cd app && cargo test)` and
   `cargo test --workspace`.
7. `LMUX_SKIP_INSTALL=1 deploy/macos/bundle.sh`, then live checks with
   `LMUX_APP=/Users/vas/Documents/lmux/target/bundle/lmux.app/Contents/MacOS/lmux-app /tmp/lmux-pd-start.sh pd`:
   paste into page, paste into terminal, Cmd+C of page text reaches `pbpaste`
   (select with `lmux browser surface:2 eval` + Cmd+A, Cmd+C), Finder-file
   paste and image paste into a streamed tab (item 3), OSC 52 copy
   (`printf '\e]52;c;%s\a' $(printf hi | base64)` in a test terminal, then
   `pbpaste`; item 4). Screenshot with `screencapture -x -l <main window id>`.
8. Stop every test process (section 6), delete /tmp scratch files, report.

## 6. Test setup (nothing of mine is running now)

- Dev daemons: `target/bundle/lmux.app/Contents/MacOS/lmux server --socket /tmp/lmux-<pd|pc>.sock --tmux-socket lmux-<pd|pc> --state-dir /tmp/lmux-<pd|pc>-state --ws-addr 127.0.0.1:<7899|7898>`, with `--app-browser` for pd only. Put the bundle's MacOS dir first on PATH for its tmux.
- Test app: `LMUX_WS_URL=ws://127.0.0.1:<port> <lmux-app>`. With `LMUX_WS_URL` set
  the app starts no local daemon and shows no other hosts. It never touches 7879.
- Drive pages: `LMUX_SOCKET=/tmp/lmux-pd.sock lmux browser surface:2 eval '...'`
  (wrap in `perl -e 'alarm 8; exec @ARGV' ...` because evals hang once Chromium wedges).
  Read terminals: `tmux -L lmux-pd capture-pane -p -t lmux:2`.
- Scripts I left in /tmp (recreate if gone):
  - `/tmp/lmux-pd-start.sh pd|pc`: starts a daemon, a workspace, a page with an
    `<input autofocus>`, and the app (`LMUX_APP` overrides the binary). Writes the pid
    to `/tmp/lmux-<kind>-app.pid`.
  - `/tmp/lmux-pd-pkey PID KEYCODE [cmd]` / `PID text STR`: activates PID, aborts
    if it is not frontmost, then posts keys ONLY to that pid (`CGEvent.postToPid`).
    Cmd+V is `9 cmd`.
  - `/tmp/lmux-pd-activate PID`, `/tmp/lmux-pd-winid PID` (lists window ids; the main
    window is the layer-0 one with height ~760-858, not the 500x500 or 33-high ones),
    `/tmp/lmux-pd-click X Y` (did not work, see traps).
- Stop: `kill $(cat /tmp/lmux-pd-app.pid)`; `kill $(pgrep -f "lmux server --socket /tmp/lmux-pd.sock")`;
  `tmux -L lmux-pd kill-server`; same for pc.
- Still running and NOT mine: the user's `/Applications/lmux.app` (pid 26726), its
  daemon on 7879 (pid 54601), and `/tmp/lmux-testhome-app/lmux-testhome-app` (pid 51098,
  started by someone else; it steals frontmost).
- `~/Library/Application Support/lmux/app.ron` was backed up to
  `/tmp/lmux-paste-app.ron.bak` before my test apps ran. Test apps share that prefs
  file (eframe app name "lmux"). The real app rewrites it on quit; do not restore it
  while the real app runs.

## 7. Traps

- DANGER: `osascript ... keystroke` goes to whatever app is frontmost. Two
  processes are named `lmux-app`, so `first process whose unix id is X` + `set
  frontmost` did not reliably switch. Early on I sent `echo hi` and a Cmd+V
  unguarded. A read-only scan of every `tmux -L lmux` pane, local and on
  lint-hqc-01/03/noverse01, found no stray text. Use only `/tmp/lmux-pd-pkey`.
- The user is active (HID idle 0 s) and keeps bringing their own lmux to the
  front, so `pkey` often aborts. Retry in a loop with sleeps.
- `screencapture -l` of the first window id gives a white 500x500 image. Use the big window.
- Synthetic mouse clicks (CGEvent at the HID tap) did not reach the test window, so
  focus was moved with `lmux focus-pane` instead. That changes lmux's focused pane,
  but not AppKit's first responder, which is exactly the bug.
- A wedged Chromium looks like a daemon hang: `lmux tree` still answers, browser
  evals do not, and the app reconnects its websocket every ~5 s. Check Chromium's
  DevTools `/json/list` to tell them apart. Restart the pc daemon after each wedge.
- `timeout` is not installed; use `perl -e 'alarm N; exec @ARGV'`.
- `NSEvent::window` needs a `MainThreadMarker` argument in objc2-app-kit 0.3.
- Wrong lead so far: `clipboard_files()` returning files for plain text. A pbcopy
  text copy has no file URL type.
