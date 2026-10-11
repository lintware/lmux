# lmux — plan

lmux is cmux with persistence and remotes built in. Its goal is one cmux-identical window where local, cloud and remote workspaces sit side by side, and every terminal, browser and agent survives restarts and disconnects and resumes exactly where it left off.

## Vision

1. **cmux frontend, unchanged.** The UI looks and behaves exactly like cmux: sidebar, folders, workspaces, splits, browser panes, notifications and the CLI. Users of cmux should notice no difference.
2. **herdr-style persistence.** Nothing closes for good. Quitting the app, losing the network or rebooting brings every pane back at the same spot: same directory, same scrollback, and the same agent session (e.g. `claude --resume`).
3. **Remote work feels local.** Open a folder or workspace that is bound to a remote host (a server or a cloud agent box). Every terminal opened inside it connects to that host automatically and works seamlessly, without manual SSH.
4. **One view for everything.** Like cmux, a single sidebar shows local, cloud and other remote connections together, each labelled by host, and they can be mixed freely.

## The user flow this must support

1. Open lmux on the laptop. The sidebar shows every host at once: **this mac**, `my-server`, and any cloud boxes, each as a labelled group of folders and workspaces. Hosts connect in the background; one being offline never blocks the others.
2. Open a folder that lives on `my-server` (e.g. `webapp`) and a workspace in it. Every terminal opened in that workspace runs on `my-server`, in that workspace's directory, with no manual SSH. A workspace under **this mac** runs locally.
3. Start Claude Code in a terminal pane. Claude opens a browser pane on the same host, navigates, clicks and types, and you watch it live.
4. Quit the app, close the lid, lose the network, or reboot either machine. Remote work keeps running. Nothing is closed for good.
5. Reopen lmux. Every workspace, pane, directory, scrollback and browser is where it was. Agents that were running are resumed (`claude --resume <id>`) if their process did not survive. Unread notifications say what happened while you were away.
6. A remote terminal or browser costs the laptop almost nothing: hidden panes stream nothing, and visible panes only receive screen updates. A local terminal is attached through a real PTY, so typing feels native.

## Hosts

A host is one machine running `lmux server` (lmuxd). Each host owns its own folders, workspaces, terminals and browsers; a workspace belongs to exactly one host, which is what "bound to a remote" means. The app holds a connection to every configured host at the same time.

| Host | How the app reaches it | Notes |
|---|---|---|
| **this mac** | The app starts or adopts a local lmuxd on `127.0.0.1:7879`. Local panes attach to its tmux through a PTY. | Always first in the sidebar. No browser panes yet (no Chromium). |
| **`my-server`** | SSH tunnel over Tailscale to the daemon's websocket. | Debian 12, 8 cores, 31 GB, tmux 3.3a, Chromium 154, claude. Hostname `my-server`, Tailscale `100.x.y.z`, SSH user `me`. systemd user lingering is on. |
| **cloud boxes** (planned) | Same SSH transport. The app can provision a box, install `lmux`, and register it as a host. | Holds cloud agents. Torn down on request, never silently. |

Hosts are listed in `~/Library/Application Support/lmux/hosts.json`; the local host is derived at runtime.

## Architecture

```
 Laptop (macOS)                                         Each host (this mac, my-server, cloud…)
┌───────────────────────────────────────┐            ┌─────────────────────────────────────────┐
│ lmux app (Rust, egui + wgpu)          │  one conn  │ lmuxd  (Rust; systemd --user on Linux)   │
│  sidebar: hosts ▸ folders ▸ workspaces│  per host  │  session tree: folder▸workspace▸pane    │
│  terminal panes: alacritty_terminal   │◀══════════▶│  terminals = tmux -L lmux (control mode) │
│    grid; local panes via a real PTY   │  websocket │  Chromium (headless) via CDP             │
│    running `tmux attach`              │  (SSH tun- │  notifications / status / agent sessions │
│  browser panes: JPEG screencast       │  nel when  │  state.json on disk                      │
│  merged tree, RPCs routed by host     │  remote)   │         ▲ unix socket (JSON-RPC)         │
└───────────────────────────────────────┘            │  `lmux` CLI ── used by you and agents   │
                                                     │  Claude Code hooks ─▶ `lmux hooks`      │
                                                     └─────────────────────────────────────────┘
```

### Why this shape

- **Each daemon owns its host's state**, like a tmux server. The app is a viewer that can come and go, which is what makes "quit or close the laptop, keep going" work.
- **The app merges hosts, the daemons don't know about each other.** The app keeps one connection and one tree per host and shows them side by side. Every ref (`workspace:3`, `surface:12`) is scoped by its host, and every RPC is sent to the host that owns its target. This is how local, remote and cloud work share one view.
- **tmux is the terminal backbone.** lmuxd runs a private tmux server (`tmux -L lmux`) and talks to it in control mode (`tmux -C`). tmux owns the PTYs, scrollback and screen state, so shells and agents survive an lmuxd crash, upgrade or app quit. Every terminal surface is its own tmux window, because lmux's layout mixes terminals and browsers, which tmux can't. `read-screen` comes from `capture-pane`, and remote input goes through `send-keys`.
- **Local panes skip the daemon for input and output.** For a pane on this mac, the app spawns `tmux attach` in a real PTY on its own grouped view session, so keystroke-to-echo is ~40 µs instead of a websocket round trip. Remote panes stream through the daemon.
- **Persistence has three layers.** (1) tmux keeps processes alive across app quits and disconnects. (2) lmuxd's `state.json` records the tree, each pane's directory and its agent session (from Claude's SessionStart hook), so when tmux itself is lost, by a crash or reboot, panes are respawned in their directory and agents resumed with `claude --resume <id>`. (3) Scrollback is saved to disk periodically so a respawned pane shows its old screen too (planned).
- **Browsers are headless Chromium on the host, driven over CDP.** The agent's commands and your view use the same page, so what you see is what the agent is doing.
- **The laptop only pays for what's on screen.** The app subscribes only to visible remote panes. Hidden terminals send nothing; hidden browsers stop their screencast. Frames are paced by CDP acknowledgements, so a slow link drops frames instead of building a backlog.
- **One Rust binary per host:** `lmux server` runs the daemon, and every other `lmux …` command is the CLI.

### Transport and security

- Remote hosts: the app opens an SSH connection (your existing keys, over Tailscale) and forwards the daemon's websocket port, which listens only on `127.0.0.1`. One SSH master per host is shared by all of its traffic.
- Local host: the daemon listens on `127.0.0.1:7879` and its unix socket.
- The unix socket is mode `0600`. Anyone who can reach a daemon can run shell commands as you, so it is never exposed beyond SSH or loopback.
- Later: a direct Tailscale connection that authenticates with the Tailscale peer identity, to drop the SSH hop.

## Feature scope

### Kept from cmux (v1)

Command names match cmux wherever possible, so Claude's existing cmux habits and skills carry over with `cmux` replaced by `lmux`.

**Organize**
- Folders containing workspaces, as in the cmux sidebar. Each workspace card shows title, a summary of the last agent message, status (Running, Idle, Needs input), git branch and cwd.
- `new-workspace`, `rename-workspace`, `close-workspace`, `select-workspace`, `reorder-workspace`, `list-workspaces`, `tree`, `identify`.

**Panes and surfaces**
- Splits with terminal and browser panes, plus tabs (surfaces) inside a pane.
- `new-split`, `new-pane --type terminal|browser`, `new-surface`, `close-surface`, `focus-pane`, `move-surface`, `resize-pane`, `swap-pane`, `list-panes`, `list-pane-surfaces`.

**Terminal control for agents**
- `send`, `send-key`, `paste`, `read-screen [--scrollback] [--lines n]`, `capture-pane`, `wait-for`, `clear-history`.

**Browser control for agents**
- `browser open|open-split|goto|back|forward|reload|url`
- `browser snapshot [-i]` (accessibility tree with element refs), `click`, `dblclick`, `hover`, `type`, `fill`, `press`, `select`, `scroll`, `wait`, `eval`
- `browser screenshot`, `get`, `is`, `find`, `console list`, `errors list`, `tab new|list|switch|close`, `cookies`, `storage`

**Agent awareness**
- `notify`, `list-notifications`, `clear-notifications`, `jump-to-unread`
- `set-status`, `clear-status`, `set-progress`, `clear-progress`, `log`, `list-log`
- `lmux hooks setup` installs Claude Code hooks (session start, prompt submit, stop, notification) so workspace status and summaries update without the agent doing anything.
- Each pane's environment includes `LMUX_SOCKET`, `LMUX_WORKSPACE_ID` and `LMUX_SURFACE_ID`, so an agent can address "my own workspace" or "the pane next to me."

### Added beyond cmux

- herdr-style persistence: terminals, browsers and agent sessions survive app quits, disconnects and reboots, and resume at the same spot.
- Browsers that keep running and stay watchable while the laptop is away.
- Workspaces bound to a remote or cloud host, with every terminal in them running there.
- One sidebar showing local, remote and cloud hosts together.

### Left out to stay lean

cmux has a lot that lmux deliberately skips in v1: cmux's own cloud VM service (lmux uses its own hosts instead), the iOS simulator, vault and checkpoints, the diff viewer, recordings, the markdown viewer, automations, sidebar extensions, team features, and browser import and profiles beyond a default profile. Any of these can be added later if you miss them.

## Repo layout

```
lmux/
  Cargo.toml                 workspace
  crates/
    lmux-proto/              shared types: session tree, RPC requests/events, frame headers
    lmux-core/               session tree, tmux control-mode backend, persistence
    lmux-browser/            Chromium lifecycle, CDP sessions, screencast, agent browser ops
    lmux/                    the binary: `lmux server` + all CLI commands
  app/                       native client (egui + wgpu)
    src/net.rs               host list, SSH tunnel, websocket client, local daemon startup
    src/state.rs             app state: tree, layout, subscriptions, local PTYs
    src/pty.rs               local panes: a real PTY running `tmux attach`
    src/term.rs              terminal grid (alacritty_terminal)
    src/ui/                  sidebar, panes, tab strip, terminal, browser, drag and drop
    mock/                    bun mock daemon for UI tests
    tests/                   interaction, drag-and-drop and snapshot tests (egui_kittest)
  deploy/
    lmuxd.service            systemd --user unit
  PLAN.md
```

## Protocol

The source of truth is `crates/lmux-proto` (`lib.rs` for messages, `methods.rs` for RPC names, `frame.rs` for binary frames). The summary below is a guide. Two protocols carry these types.

**CLI ⇄ daemon:** JSON-RPC over the unix socket, one request per connection. For example, `{"method":"surface.send","params":{"surface":"surface:3","text":"npm test\n"}}`.

**App ⇄ daemon:** one websocket per host carrying JSON control messages and binary frames. The app holds one of these for every host at once. Local panes on this mac also bypass the websocket for terminal I/O, through a PTY running `tmux attach`.

| Direction | Message |
|---|---|
| app → daemon | `hello{client_version}`, `subscribe{surface}`, `unsubscribe{surface}`, `input{surface, bytes}`, `resize{surface, cols, rows}` or `{w, h}`, `browser_input{surface, mouse/key/wheel event}`, any RPC method |
| daemon → app | `tree{…}` (full tree on connect, then diffs), `term_snapshot{surface, screen}`, `term_output{surface, bytes}`, `browser_frame{surface, jpeg, metadata}` (binary), `browser_meta{url, title, loading}`, `notification{…}`, `status{…}` |

The browser canvas in the app is sized to its pane. The daemon starts the screencast at that size, so frames never carry more pixels than are displayed.

## Milestones

Each milestone ends with something you can run.

**Done so far (branch `dev`, 2026-10-09)**
- **M1 Daemon, terminals, CLI.** lmuxd with the session tree, tmux control mode, unix socket and the core cmux-named CLI.
- **M2 App attach.** Native app with sidebar (nested folders, drag and drop, card controls), splits, tabs, terminal panes, subscribe on view, snapshot on attach.
- **M3 Browser.** Headless Chromium over CDP, screencast to the pane, input forwarding, `lmux browser …` for agents (remote hosts only).
- **Local host.** The app starts a daemon on this mac; local panes attach through a real PTY (~40 µs keystroke to echo).
- **Agent resume.** Claude's session ID and each pane's directory are kept in `state.json`; when tmux is lost, panes are respawned and Claude resumed.

**M4 One view for all hosts** — *first cut done on `feat/multi-host`: one connection and one state per host, grouped sidebar with host headers (fold, New Session/Folder, Reconnect), per-host egui ids, a host comes on screen when you act on it, only the host on screen streams. Still to do: notifications panel per host, cross-host drag, a host picker for split/new-tab shortcuts.*
The app connects to every host at once, keeps a tree per host, and the sidebar shows them as labelled groups (this mac first). Refs are host-scoped, and every RPC, subscription and input goes to the host that owns its target. Create menus ("New Session Here", "New Folder") act on the host of the row they were opened from. A host that is offline shows as such without blocking the rest.
*Done when:* a local workspace and a `my-server` workspace are both in the sidebar, you switch between them with one click, and typing in each runs on its own machine.

**M5 Full persistence.**
Periodic scrollback capture to disk, restored into respawned panes; resume for agents beyond Claude (e.g. Codex); browser tabs restored to their URL after a Chromium restart; the "terminal exited while the daemon was down" case handled.
*Done when:* rebooting either machine brings every pane back with its directory, its last screen, and its agent resumed.

**M6 Agent awareness.**
Notifications in the app and as macOS notifications, workspace status and summaries on sidebar cards, `set-status`, `set-progress` and `log`, across all hosts.
*Done when:* Claude finishing on `my-server` while the laptop is closed shows as an unread notification and an "Idle" card on reopen.

**M7 Cloud hosts.**
Add a cloud box as a host from the app: provision (or adopt) a VM, install `lmux`, start the service, and register it in `hosts.json`. Workspaces created under it run cloud agents.
*Done when:* "New cloud host" yields a working host in the sidebar whose terminals run on the new box.

**M8 cmux parity pass.**
Side-by-side comparison with cmux (`~/Documents/cmux`): layout, spacing, colours, keybindings, menus and command names. Every difference is fixed or recorded as intended.

**Later: mobile app.**
A phone client for the same hosts: the sidebar of workspaces with agent status and notifications, terminals to read and type into, and browser panes as the frame stream the desktop app already gets. Everything runs on the hosts, so the phone only views and drives them, the way the laptop does for remote hosts. The daemons need a way to reach a phone without the laptop's SSH tunnel (for example over Tailscale), and push notifications when an agent needs input.

**Later: share a tab to your phone (QR).**
Right-click a tab, choose *Show QR*, scan it with the phone, and that one session opens on the phone (and optionally in a browser). The user runs no command and installs nothing besides the phone app. A prototype of the phone app and a Python bridge is in `mobile/` (see `mobile/README.md`); this feature replaces the bridge with a built-in server.

- **Scope.** A QR shares one tab, never the whole machine. The phone sees that tab's live screen and can type into it; it does not see other tabs. The tab can be local or on a remote host, because the app already holds the connection to every host and forwards for the phone. Sharing ends on *Stop sharing*, when the tab closes, when the QR expires, or when the app quits.
- **Server.** Built into the app (or `lmux server`), with no separate process to start. It serves a small WebSocket API: screen frames out, keystrokes in. A WebSocket is required, because Cloudflare Quick Tunnels do not support Server-Sent Events.
- **Reaching the laptop, in order of preference.**
  1. Same network or Tailscale: the QR lists the laptop's addresses and the phone tries each. Free, no third party.
  2. Cloudflare Quick Tunnel: *Show QR* starts `cloudflared tunnel --url http://localhost:<port>` for the user and puts the printed `trycloudflare.com` URL in the QR. Free, needs no Cloudflare account or login, and traffic never passes through our infrastructure. Limits (from Cloudflare's docs): the URL changes on every start, there is no uptime guarantee, Cloudflare calls the feature test and development only, and requests above 200 in flight get a 429. `cloudflared` has to be bundled or downloaded on first use; check its licence. Test a WebSocket through a Quick Tunnel before committing to this.
  3. Relay on `lmux.lintlabs.org` (a Worker plus a Durable Object per pairing): works from any network with no tunnel. We pay for it and carry abuse risk, so encrypt end to end with a key carried only in the QR, and use WebSocket hibernation to keep idle cost near zero. Build only if Quick Tunnels prove too fragile.
- **Pairing and tokens.** The QR holds the addresses and a single-use code that expires in a few minutes. The phone redeems the code for a token tied to that tab and stored in the Keychain; the Mac keeps a list of paired devices with revoke. No permanent token ever goes in a QR.
- **Browser access.** After scanning, the same session can open in a mobile browser, served by the laptop itself over the tunnel's HTTPS. Rules, because a terminal is remote code execution:
  - The token travels in the URL fragment (`#…`), which browsers never send to a server, and the page trades it for a short-lived `HttpOnly`, `Secure`, `SameSite=Strict` cookie, then clears the fragment.
  - The WebSocket checks its `Origin` header and the cookie, to block cross-site hijacking.
  - View-only by default; typing is a toggle in the *Show QR* dialog.
  - The page loads no third-party scripts, and the server sends a strict `Content-Security-Policy`.
  - Plain-HTTP LAN addresses cannot use `Secure` cookies, so browser access needs HTTPS (a tunnel or Tailscale Serve).
- **Phone view.** First the terminal screen. For Claude tabs the recorded session ID could allow a chat view from the transcript, with the screen as the fallback.
- **Remote sessions appear automatically** once paired through the laptop, because the app already merges every host into its sidebar.
- **Risks.** Anyone holding a live share link can read the tab and, if allowed, type into it, so QR codes are secrets: short expiry, one tab each, and a visible *Sharing* badge on the tab. Screens show keys and tokens, so never log frames on a relay.

## Risks to test early

- **Ref collisions across hosts.** Two daemons both have `workspace:1`. Every place the app stores a ref (selection, drag state, layout, subscriptions, notifications) must carry the host too. Do this as a type (`HostRef`), not by convention.
- **Several SSH tunnels at once.** Reconnect, backoff and ping per host, and make sure one hung host can't stall the UI thread.
- **Screencast latency and bandwidth over Tailscale.** If JPEG frames are too heavy: a lower frame rate when idle, lower quality while scrolling, or later a WebRTC video track.
- **Chromium memory** on `my-server` (31 GB, ~11 GB already used). Cap live tabs per workspace and suspend hidden ones, as cmux's memory saver does.
- **Headless detection.** If sites misbehave in headless Chrome, run headful Chromium under Xvfb. Nothing else changes.
- **Clipboard and file drops across machines.** The clipboard is on the laptop and the files on the host, so the app has to copy across explicitly.

## Decisions taken in this plan (open to change)

- **tmux as the terminal backbone**, with lmuxd layered on top for layout, browsers, persistence and agent features.
- **Rust everywhere**, with a native **egui + wgpu** client and `alacritty_terminal` for the grid. This replaced the original Tauri + xterm.js plan.
- **One daemon per host; the app merges them.** Daemons don't federate.
- **Local panes attach through a real PTY**; remote panes stream through the daemon.
- CLI command names that **match cmux**.
- **SSH-tunnelled** transport for remote hosts, not a listening network port.
- **Headless Chromium** first, with headful Chromium under Xvfb as the fallback.
