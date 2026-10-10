# Changelog

All notable changes to lmux are listed here, newest first. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and versions follow
[Semantic Versioning](https://semver.org/). Add your change under
**Unreleased** in the pull request that makes it.

## Unreleased

### Added

- **Real pointer and keyboard input for agents:** `lmux browser mouse click|dblclick|down|up|move|wheel X Y`
  and `lmux browser keyboard --text … | --key …`. They go through the browser
  engine's own input path, so they land inside cross-origin iframes (Google's
  sign-in button, payment frames) that element refs and DOM events cannot
  reach. Not yet available for browsers the app draws itself.

### Fixed

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
