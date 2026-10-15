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
- Measured on hqc3 (60 fps recording of the test window, same page and pane
  size in both modes, `/tmp/lmuxprobe`):

| Measure | Streamed (Chromium on host, JPEG) | In-app WebKit through the host |
| --- | --- | --- |
| Host `lmux browser eval` flips a colour: time from issuing the command (ssh + CLI ≈ 90 ms of this) to the pixel changing | flips seen: 4 of 12 within 2 s, 2 of 10 within 8 s; when seen, 7–1650 ms and once 6.6 s; others arrived later or never in the window | 11 of 12 seen, median 95 ms (81–172) |
| Distinct frames per second in the pane while HN scrolls (3 px per 16 ms timer on the host, 8 s) | 4.0 | 22.1 |
| HN text sharpness | JPEG, visibly soft (`/tmp/lmux-shots/hn_compare.png`) | native Retina |
| Keypress to screen (CGEvent key, page flips on keydown) | not measurable: keys did not reach the streamed page (focus bug, see handoff-paste.md) | 59–85 ms, median 79; page's own keydown to frame 11–75 ms |

  What each number is: the eval row is a host-side command to a visible
  change, so it includes the ssh round trip (bare ssh ≈ 50 ms, eval CLI
  ≈ 90 ms). The scroll row is changed frames in a 400×600 pt crop, so it
  counts what the user actually sees. The streamed values were bad enough
  that a median is not meaningful: the stream stalls for seconds between
  frames on this link.
- Per host, in-app mode (test daemons on port 7898):

| Host | HN | x.com | Page on host's `127.0.0.1` | goto/url/snapshot/eval | Tab moves to Chromium when the app leaves |
| --- | --- | --- | --- | --- | --- |
| lint-hqc-03 (macOS) | ok | ok | ok | ok | ok (tested in unit test and live earlier) |
| lint-hqc-01 (Linux, Chromium) | ok | ok | ok | ok | ok, and back to the app when it returns; the Chromium copy of x.com showed a chrome-error page because the page was moved while loading, reloading a host URL worked |
| lint-cloud-noverse01 (Linux, no Chromium) | ok | ok | ok | ok | no Chromium to move to: `lmux browser url` says "this browser runs in the lmux app, and no lmux app is connected" |

- Edge cases (hqc3): a cookie set by a page on hqc3's localhost survives an
  app restart and is not visible to the same URL on hqc1 (per-host data
  store); `target=_blank` opens a new tab beside the opener; after the
  tunnel dropped and the app reconnected on a new port, pages loaded
  through the host again.
- Found and fixed: a tab in the app vanished when the daemon restarted.
  Closing the Chromium page for a hand-over made Chromium report `Closed`,
  and the daemon removed the tab. The daemon now ignores `Closed` for app
  tabs, and a test covers it.
- Left:
  - A host-only page opened while the SOCKS forward is down gets no error
    page: the old page stays and `goto` still answers OK. It should show a
    clear error (wry has no navigation-failed hook, so this needs a
    WKNavigationDelegate or a probe of the relay).
  - `lmux browser eval` with a statement script fails on pages whose CSP
    forbids `eval` (Hacker News); expression scripts work.
  - Cmd+C, file drop and paste into a native remote page were not tested
    here; the paste agent owns the focus fix.
  - The streamed-mode keypress number needs that focus fix first.
