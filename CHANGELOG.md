# Changelog

All notable changes to lmux are listed here, newest first. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and versions follow
[Semantic Versioning](https://semver.org/). Add your change under
**Unreleased** in the pull request that makes it.

## Unreleased

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
