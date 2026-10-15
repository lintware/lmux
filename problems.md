# lmux problems

Problems found while using lmux with remote hosts (hqc3 = `lint-hqc-03`,
plus `lint-hqc-01` and `lint-cloud-noverse01`). Status as of 2026-10-12.
"Installed" means built and installed, but not yet confirmed in daily use.

## Files

### 1. No way to send a file to another host
lmux had no command or gesture to move a file from this Mac to a remote
host's session.
- Status: fixed. `lmux cp SOURCE... HOST[:WORKSPACE|/PATH] [--tell]` copies
  files with scp into a workspace's folder. Tested against hqc3.

### 2. Dragging a file onto a remote pane did nothing useful
A drop pasted the Mac's local path, which does not exist on the remote host.
- Status: installed. A drop on a remote terminal pane copies the files into
  the workspace's folder on that host, then pastes the remote paths.

### 3. Pasting or dropping files into a browser tab did not land
In a streamed browser tab (the page runs in Chromium on the host), Cmd+V
with a file copied in Finder typed only the file name, and a drop did
nothing.
- Status: installed. The file's bytes are sent to the page as a paste event,
  a drop on the element under the pointer, or a file input. Limit: 10 MB
  per paste or drop.

## Clipboard

### 4. Copy from Grok Build on a remote tab did not reach the Mac
Grok Build saw a local desktop and used `pbcopy` on the remote machine.
- Status: installed on all three hosts. Remote daemons set `LMUX_REMOTE=1`,
  so panes get `SSH_CONNECTION`/`SSH_TTY` and Grok copies with OSC 52.
  Only tabs opened after the daemon restart get the fix.

### 5. Paste did not work in the in-app browser
The app had no Edit menu, so a native WebKit page never got Cmd+V/C/A.
- Status: installed. Added an Edit menu.

## Connections

### 6. Servers reconnect slowly
After a drop, hosts took up to 15 s between retries, and a dead SSH control
socket could block reconnects.
- Status: installed. Retries back off from 0.3 s to 5 s, stale control
  sockets are removed, the app retries when the window regains focus, and
  the host's right-click menu has "Refresh Connection".

## IDs

### 7. No way to copy a remote surface's ID
- Status: installed. "Copy IDs" in the terminal, tab and sidebar right-click
  menus copies e.g. `lint-hqc-03 workspace:19 pane:32 surface:43`.

## Browser tabs on remote hosts

### 8. Browser pages look pixelated
The stream was sent at 1× resolution and stretched onto the Retina screen.
- Status: fixed in the new mode. A remote browser tab now runs in the Mac
  app's own WebKit, so it renders at the display's native scale. The
  Chromium stream (2× JPEG) stays as a fallback.

### 9. Browser tabs have a very low frame rate
Measured on hqc3 (surface:48, 604×778 CSS px, scrolling page), streamed:

| Scale | Frame size | Frames per second | Throughput |
| --- | --- | --- | --- |
| 1× | ~220 KB | 1.6 | ~2.8 Mbps |
| 2× | ~510 KB | 0.8 | ~3.4 Mbps |

The link carries about 3 Mbps, and frames are large JPEGs, so the frame
rate was limited by bytes per frame.
- Status: fixed by moving the page to the Mac. The app's WebKit loads the
  page itself and sends only the page's network traffic through the host:
  a SOCKS5 relay in the app (`app/src/socks.rs`) sends host-only targets
  (loopback, private and CGNAT addresses, single-label names, `.local`,
  `.internal`, `.ts.net`…) through the ssh tunnel's `-D` port and
  everything else straight from the Mac. A remote page's `localhost` is
  loaded as `localhost.lmux` (WebKit never proxies loopback) and shown as
  `localhost` in the address bar, the daemon and agent results. Each host
  has its own `WKWebsiteDataStore`, so cookies persist per host.
  Needs macOS 14+ and an ssh host. Opt out with `"stream_browsers": true`
  in hosts.json or `LMUX_STREAM_BROWSERS=1`.
- The daemon follows who can draw: while an app that sends
  `draws_browsers` is connected, a Chromium host's tabs are the app's
  (their Chromium pages close); 5 s after the last such app leaves, they
  move to Chromium at their saved URL so agents keep working. Older apps
  keep the old behaviour.
- Measured keypress-to-screen latency on hqc3 (60 fps screen recording):

| Mode | Keypress to screen | Frames per second scrolling HN | HN text sharpness |
| --- | --- | --- | --- |
| Streamed (Chromium on host) | not measured (see below) | not measured | not measured |
| In-app WebKit through the host | 59–85 ms (median 79, 8 of 16 presses; the rest fell in the window activating) | not measured | native Retina |

  In-page keydown-to-frame (`window.lat`) in the new mode: 11–75 ms.
- Left: the streamed-mode numbers, the frame-rate and crop comparison, the
  Linux hosts (`lint-hqc-01`, `lint-cloud-noverse01`), and the edge cases
  (Cmd+C, file drop, tunnel reconnect, tunnel down, new-tab links, cookies
  across restarts) have not been verified yet. Daemon hand-off is covered
  by tests only.
