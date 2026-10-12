# Changelog

All notable changes to lmux are listed here, newest first. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and versions follow
[Semantic Versioning](https://semver.org/). Add your change under
**Unreleased** in the pull request that makes it.

## Unreleased

### Added

- **Browser tabs on remote hosts run in the app's own WebKit.** The page is
  drawn and run on the Mac, so it is sharp and as fast as a local tab; only
  the page's network goes through the host. A SOCKS5 relay in the app sends
  the host's own addresses (`localhost`, private IPs, `.local`, `.internal`,
  `.ts.net`) through the host's ssh tunnel and everything else straight from
  the Mac. Each host keeps its own cookies. `lmux browser` commands on the
  host reach the app's page. Needs macOS 14+; set `"stream_browsers": true`
  on a host in hosts.json (or `LMUX_STREAM_BROWSERS=1`) to keep the Chromium
  stream. A host with Chromium moves tabs to it a few seconds after the last
  such app disconnects, and takes them back when one connects.
- **`lmux cp FILE... HOST[:WORKSPACE|/PATH]` copies files and folders to a host.**
  `HOST:WORKSPACE` (a title, a fragment of one, or a ref) copies into that
  workspace's working directory, so a chat running there can read the files.
  `--tell` also pastes the remote paths into its terminal. It uses `scp`
  with your ssh config and prints where each item landed.
- **Share your sessions to a phone (`lmux share`, lmux menu > Show QR…).** Starts
  a Cloudflare Quick Tunnel and shows a QR code and a `trycloudflare.com` link.
  Opened on a phone, it lists every workspace and tab with its agent status,
  shows a tab's live screen, and (when you allow typing) accepts keys and text.
  View only by default. The link carries a single-use code in its `#` fragment
  that expires in 5 minutes; the page swaps it for an `HttpOnly`, `Secure`,
  `SameSite=Strict` cookie, the WebSocket checks `Origin` and that cookie, and
  the page loads no third-party scripts. `cloudflared` is taken from `PATH` or
  downloaded once. Quick Tunnel limits apply: the address changes every start,
  there is no uptime guarantee, and requests above 200 in flight get a 429.
  Sharing ends on Stop sharing, Ctrl-C, or when the app quits.
- **The share page shows agent tabs as a chat.** Tabs running Claude Code,
  Codex or Grok are labelled in the session list, and opening one shows its
  transcript as readable messages (your prompts, the agent's replies with basic
  Markdown, and one-line tool calls), with a Chat | Terminal switch and a
  banner when the agent waits for an answer. Replies go to the tab like typed
  text, so view-only shares stay view only. The server finds each transcript
  from the daemon's own data (the page never sends a path), reads only the end
  of the file, and never logs it. The page works on phones and desktops, with
  light and dark themes. A new daemon call, `term.processes`, reports each
  terminal's process and agent session; with an older daemon the page shows
  terminals only.

- **Terminals look like Ghostty.** Panes are drawn by a new GPU renderer
  ported from Ghostty's: its default font (JetBrains Mono, bundled) at 13pt,
  CoreText glyphs with Ghostty's cell metrics, box drawing, blocks, braille,
  Powerline and legacy-computing characters drawn to the exact cell, all five
  underline styles, colour emoji, and cmux's default colours. Your Ghostty
  config and themes (`~/.config/ghostty/config`) are read too.
  `LMUX_RENDERER=egui` brings back the previous renderer.
- Programs in a pane are told they run in Ghostty (`TERM_PROGRAM=ghostty`,
  an empty `TMUX`), so tools that play safe inside tmux use their full
  settings: Claude Code draws in true colour instead of 256 colours.

- **Real pointer and keyboard input for agents:** `lmux browser mouse click|dblclick|down|up|move|wheel X Y`
  and `lmux browser keyboard --text … | --key …`. They go through the browser
  engine's own input path, so they land inside cross-origin iframes (Google's
  sign-in button, payment frames) that element refs and DOM events cannot
  reach. Not yet available for browsers the app draws itself.

### Fixed

- **Hosts reconnect faster.** The retry waits are shorter (0.3 s up to 5 s, was
  0.5 s up to 15 s), a host that is waiting retries the moment the window
  regains focus, and a dead ssh connection left behind by sleep or a network
  change is replaced instead of reused. The host menu's "Reconnect" is now
  "Refresh Connection": it drops the current attempt and connection and
  starts over at once.
- **Paste, copy and select-all work in the in-app browser.** The app had no
  Edit menu, and a native WebKit page only receives Cmd+V, Cmd+C and Cmd+A
  through it.
- **Files land in streamed browser pages.** Files copied in Finder and
  pasted with Cmd+V, or dropped from Finder onto the page, reach the page as
  real files (paste event, drop on the element under the pointer, or a file
  input), up to 10 MB at a time. Before, only the file name was typed.
- **Streamed browser pages are sharp on Retina.** The stream now uses the
  display's scale (2×), so text is no longer pixelated.
- **Dropping files on a pane of a remote host copies them there.** They go to
  the workspace's directory over the host's ssh connection, and the remote
  paths are pasted into the pane (a Mac path means nothing on another host).
- **Copy IDs** is on terminal panes and sidebar sessions too, and on a remote
  host it starts with the host, so the refs name one machine.
- **Copying in Grok Build on a remote host reaches this Mac's clipboard.**
  Panes on a host now look like an SSH session (`LMUX_REMOTE=1` in the daemon
  service), so Grok copies with OSC 52 instead of the host's own clipboard.
  Reinstall the host (`deploy/install-remote.sh`) and open new tabs.
- **Popups are real windows.** A page's `window.open` and `target=_blank` now
  open a tab beside it that keeps `window.opener`, so sign-ins that finish with
  `postMessage` and `window.close()` complete, and the tab goes away when the
  popup closes. WebKit browsers on a host used to load the popup's URL into
  the same page.
- Typing in a WebKit browser on a host no longer types every character twice.
- A terminal whose program turned on mouse reporting before the app attached
  now gets the mouse wheel, instead of the app scrolling its own history.
- Renaming a session no longer draws the sessions below it over its card.
- `Cmd+A`, `Cmd+Z`, `Cmd+←/→` and Option+arrow/delete edit text in WebKit
  browsers on a Mac host.

## 0.1.0 - 2026-10-10

The first public release.

### Added

- **Terminals that outlive the window.** Every terminal runs in tmux on its
  host, so quitting the app, closing the lid, losing the network or rebooting
  brings each pane back with its directory, scrollback and Claude session.
- **One sidebar for every machine.** This Mac and any number of servers or
  cloud boxes appear together; a workspace bound to a host runs all its
  terminals there, with no manual SSH.
- **Browser panes on the host.** WKWebView on a Mac host, WebKitGTK on Linux,
  driven by agents and watched live. On the local Mac they are native WebKit
  views inside the app.
- **The `lmux` CLI for agents,** with cmux's command names: `send`,
  `read-screen`, `wait-for`, `browser snapshot|click|type|fill|eval`, `notify`
  and `set-status`.
- **Claude resume.** Each tab's Claude session is recorded and resumed with
  `claude --resume <id>` after a tmux or machine restart; Codex sessions are
  resumed the same way.
- **Import from cmux** (`lmux import-cmux`, or from the command palette):
  folders, splits, tabs, directories and browser pages, with idle Claude
  sessions moved across.
- **Remote hosts** are added from the app (*Add Remote Host…*) or with
  `deploy/install-remote.sh`, which builds lmux there and runs it as a user
  service.
- **lmux.app** is signed with a Developer ID and ships its own tmux 3.8, so a
  fresh Mac needs nothing else.

[Unreleased]: https://github.com/lintware/lmux/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/lintware/lmux/releases/tag/v0.1.0
