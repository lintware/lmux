# lmux — lean mux

**cmux + herdr + tmux, with the paid and annoying parts taken out.**

lmux is a terminal workspace app for running coding agents:

- **cmux's window.** It has the same sidebar of folders and workspaces, splits, tabs, browser panes, notifications and agent-control CLI. Command names match cmux, so agents that know `cmux` already know `lmux`.
- **herdr's persistence.** Nothing closes for good. Quit the app, close the lid, lose the network or reboot, and every pane comes back where it was: its directory, its scrollback and its Claude session.
- **tmux underneath.** Every terminal lives in tmux on the machine it runs on, so it keeps running with no window attached.

lmux leaves out cmux's paid and heavy parts: its cloud VM service, team features, vault and checkpoints, recordings, the iOS simulator and the many background helpers. It uses your own machines instead. That's the "l" in lmux: lean.

## What it does

- **One sidebar for every machine.** This Mac, your servers and cloud boxes appear together, each labelled. A workspace bound to a server runs every terminal there, with no manual SSH.
- **Remote work costs the laptop almost nothing.** Remote terminals and browsers run on the server and keep going while the laptop sleeps. The app only draws what's on screen.
- **Browsers run on the host, in WebKit.** On a Mac host, pages run in a WKWebView helper; on Linux, in WebKitGTK. Agents on that host drive them, and the app shows them live. On this Mac, browser panes are native WebKit views inside the app.
- **Agents drive everything.** `lmux send`, `read-screen`, `wait-for`, `browser snapshot/click/type/fill/eval`, and `notify` and `set-status` for the sidebar. Claude Code hooks keep each workspace's status and summary up to date.
- **Agents resume.** Each tab's Claude session is recorded. If tmux or the machine restarts, the tab reopens in its directory and runs `claude --resume <id>`.
- **Import from cmux.** `lmux import-cmux --move`, or *Import from cmux…* in the command palette, brings your cmux workspaces over:
  - folders, splits, tabs, directories and browser pages come with them;
  - each idle Claude session closes in cmux and resumes in lmux with its conversation;
  - a Claude that's in the middle of a task stays in cmux until it's idle.

## How it's built

```
lmux.app (laptop) ── one websocket per host, over SSH ──▶  lmux server (each host)
  egui + wgpu UI                                             session tree, RPC, persistence
  local panes: a PTY on tmux                                 tmux (control mode)
  browser panes: WebKit                                      WebKit helper for pages on the host
```

- `crates/lmux-proto`: shared wire types, plus the cmux export reader.
- `crates/lmux-core`: the daemon, with the session tree, tmux backend, RPC, persistence and browsers.
- `crates/lmux`: the binary. `lmux server` runs the daemon; every other subcommand is a CLI client.
- `app/`: the native client (Rust, egui).
- `helpers/webkit-mac`, `helpers/webkit-linux`: the host-side WebKit page runners.
- `deploy/`: the macOS app bundle, the remote installer, systemd and launchd units, and cloud box scripts.

The design and milestones are in [PLAN.md](PLAN.md).

## Install

On the Mac:

```sh
deploy/macos/bundle.sh          # builds and installs /Applications/lmux.app and ~/.local/bin/lmux
open -a lmux
```

On a server (Linux or macOS, with tmux and a Rust toolchain):

```sh
deploy/install-remote.sh <ssh-host> [git-ref]
```

You can also do this from the app: right-click the sidebar and choose *Add Remote Host…*. The installer starts the daemon as a user service and adds the Claude Code hooks.

## Use

```sh
lmux new-workspace --name api --cwd ~/code/api
lmux new-split right
lmux send "npm test\n"
lmux read-screen --lines 20
lmux browser open https://example.com
lmux browser snapshot -i
lmux import-cmux --dry-run      # see what would come over from cmux
lmux import-cmux --move         # move it
lmux --help                     # everything else
```

## Status

lmux is in active development on the `dev` branch. Done so far:

- terminals, the CLI and the app;
- several hosts in one sidebar;
- local and remote WebKit browsers;
- Claude resume;
- cloud boxes;
- importing from cmux.

Next: full scrollback persistence across reboots, notifications from every host, and a cmux parity pass.

**Later: a mobile app** for the same hosts, to watch and steer agents from a phone.
