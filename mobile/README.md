# lmux mobile

The lmux iPhone app. Scan the QR code from the desktop app's lmux menu > *Show QR…* (or `lmux share`) and the shared sessions open natively: every workspace with its status, each tab's agent, its chat transcript, and its live terminal, with typing when the share allows it. It was forked from the Agent Phone project (`~/Documents/agent-phone`).

## What exists

| Path | What it is |
|---|---|
| `ios/Lmux/` | SwiftUI app. Talks to the `lmux share` server over HTTPS and a WebSocket. Follows the system dark or light appearance with the lmux palette. |
| `build-ios.sh` | Builds with `swiftc` directly (no Xcode project), signs with the team's wildcard profile, and installs over USB with pymobiledevice3. `--no-install` (or `NO_INSTALL=1`) only builds; `SIM=1` builds for the simulator. |
| `tools/share-probe.sh` | Compiles the app's networking files for macOS and runs them against a share link: redeem, socket, tree, open, type, spent-code and bad-cookie checks. |

```sh
mobile/build-ios.sh                # build, sign, install on the phone
mobile/build-ios.sh --no-install   # build and sign only
SIM=1 mobile/build-ios.sh          # simulator build in build/sim/Lmux.app
mobile/tools/share-probe.sh 'https://x.trycloudflare.com/#code' 'echo hi'
```

## How it connects

The share server's protocol is in `docs/handoff-remote-ui.md` (sections 3 and 4) and `crates/lmux/src/share/server.rs`.

1. The QR holds `https://<x>.trycloudflare.com/#<code>`. The code is single use and lasts 5 minutes.
2. The app sends `POST /auth {"code":…}` with `Origin: https://<host>` (the server checks it). A 204 sets the `lmux_session` cookie. The session lasts while it is used: it lapses after 24 hours with no connected socket, and after 30 days in any case. The app keeps it in the Keychain (this device only), never in `UserDefaults`.
3. The app opens `wss://<host>/ws` with the `Cookie` and `Origin` headers. The server streams `tree`, `screen` (the open tab) and `chat` (the open tab's transcript when `chat` is true). The app sends `open`, `close`, `send` and `key`.
4. The socket reopens with backoff after a drop and when the app returns to the foreground, while the cookie is valid. On "The session expired.", or a 401/403 at the upgrade, the app clears the cookie and asks for a new QR code. It does the same when the host is not found three times in a row: the gateway restarted and its Quick Tunnel address changed.

Ways in:

- The in-app scanner (`QRScannerView`, AVFoundation) or a pasted link.
- The `lmux://` URL scheme: `lmux://share?url=<percent-encoded https link>` or `lmux://<host>/#<code>`. Universal Links cannot work, because trycloudflare.com is not our domain.

The iPhone Camera app opens an `https://…trycloudflare.com` QR in Safari, which shows the web page. To land in the app from the Camera app, the desktop QR (or a button on the web page) would have to carry `lmux://share?url=…`. The trade-off: phones without the app cannot open an `lmux://` link at all, so the web page should offer an "Open in the lmux app" link rather than the QR switching outright. This repo's desktop and web code are unchanged.

## Screens

- **Connect:** scanner, paste field, errors such as "This QR code was already used or has expired.", and the saved connections to pick instead.
- **Connections** (tap the name at the top of Sessions): every saved connection with its servers and state (Connected, a last-used time, or Expired). Tap to switch, swipe or long-press to remove, or add one by scanning. They live in the Keychain as one list (`Keychain.swift`); scanning a share already saved refreshes it, and a gateway that comes back under a new address replaces its old entry (matched by host name).
- **Sessions:** an inbox. A server bar (`All` plus one chip per gateway server, with a link dot and a count of sessions that need input) narrows the list. "Needs you" comes first across servers, then each server's other sessions. A row is the agent's avatar with a status badge, the name and state (Needs you, Working, Idle), a preview of what the agent said, server, branch and folder, and a chip per tab when there are several. Tapping opens the agent's tab; a waiting session whose summary holds a numbered menu shows its choices as buttons that answer from the list (`PromptOptions` in `ChatParts.swift`); swipes send Enter or Esc (trailing) or ^C (leading). Summaries lose `<pasted_content>`-style tags, box-drawing and spinner characters (`SummaryText` in `Models.swift`).
- **Tab:** the header shows the session, its state, agent and server; the toolbar button flips between Chat and Terminal. Chat groups the transcript into turns (`ChatItem`): the user's messages in bubbles, the agent's Markdown under its name (`MarkdownView.swift`, with copyable code blocks), and runs of tool calls folded into one "N steps" card with an icon per tool (`ToolStep`). "Working" dots show while the agent runs. When it waits, a card shows its question and numbered choices (read from the live screen, else the summary) as buttons, plus Esc and Terminal. The composer is one bar: attach, model and commands, the field, terminal keys on demand, send. Terminal is JetBrains Mono on the dark terminal ground with the key row always shown. Without typing, a "View only" bar replaces the composer.

The theme lives in `Theme.swift` (palette from `app/src/theme.rs` and `crates/lmux/src/share/app.css`). The icon comes from `app/assets/icon.png` when present, else `deploy/macos/make-icon.swift` renders it at build time.

## Debug hooks

A simulator cannot scan or tap, so launch arguments stand in: `simctl launch <udid> com.vas.lmux -link '<share link>' -tab surface:2` redeems the link and opens that tab. `simctl launch <udid> com.vas.lmux -mock` shows a three-server sample tree (`MockData.swift`) for screenshots; add `-tab surface:1` for a sample chat and terminal screen.

## History

The Python bridge (`bridge/lmux_bridge.py`, a bearer-token HTTP server that shelled out to the `lmux` CLI) and the Agent Phone leftovers (ControlView, FinishAlerts, image upload, model switching, pins, settings) were removed. The share server replaces the bridge.

## To do

- Notifications when an agent finishes or needs input (needs a push path; the share socket only lives while the app is open).
- Browser tabs show as non-openable rows.
- An Xcode project or CI build if this ships beyond one device.
