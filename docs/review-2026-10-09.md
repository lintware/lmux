# lmux code review — second pass

Reviewed 2026-10-09. This pass covers the daemon and protocol (`crates/lmux-core`, `crates/lmux-proto`), the browser engine, CLI and deploy files (`crates/lmux-browser`, `crates/lmux`, `deploy/`), and the egui app (`app/`).

How the findings were checked:
- tmux behaviour was tested with tmux 3.3a on my-server.
- The CLI flag issue (C1) was reproduced.
- The app's tests pass: 55 unit tests and 46 integration tests.
- Everything else comes from reading the code.

## Status of the first review

| # | Item | Status |
|---|---|---|
| 1 | Non-UTF-8 output kills terminal streaming | Fixed (raw `read_until` and a supervisor), but see N3 |
| 2 | State saves can race | Fixed (one debounced task), but see N6 |
| 3 | SSH connection can stall | Fixed |
| 4 | Terminal text drifts out of its columns | Fixed |
| 5 | Browser scrolling uses the wrong position | Fixed |
| 6 | Client queues can grow without limit | Partly fixed: the queue is capped at 512, but recovery is broken (see N5). `pause-after` is still not used. |
| 7 | Extra work per terminal output chunk | Fixed |
| 8 | Workspace tree resent too often | Fixed on the server. `workspace.new` still runs `git` inline (`rpc.rs:345`). |
| 9 | Browser streams start at Retina resolution | Fixed |
| 10 | Chrome encodes frames nobody will see | Fixed (`everyNthFrame`), but undercut by B2 |
| 11 | Creating a terminal takes six tmux round trips | Fixed, but chaining caused N2 |
| 12 | JPEG decoding blocks the UI | Fixed. The decode queue is unbounded, which is minor. |
| 13 | Terminal painter redoes everything every frame | Mostly fixed. The reused buffer caused A2. |
| 14 | Reconnecting is slow | Partly fixed: a stale ControlMaster socket file is trusted (`net.rs:23`), and the 3 s ping timeout can fire falsely behind browser frames. |
| 15 | `BrowserEvent` defined twice | Mostly fixed. `browser_adapter.rs:87-146` still has an identity `convert()` and a forwarding task. |
| 16 | Daemon has five separate locks | Fixed |
| 17 | Bun mock duplicates the protocol | Still present |
| 18 | Dead code | Mostly fixed. New dead path: the fallback in `shutdown` at `lmux-browser/src/lib.rs:1085`. |

## Fix first

These freeze the daemon, corrupt terminals, or let someone run commands.

- **N1. Closing a surface deadlocks the whole daemon.** ✅ **Fixed 2026-10-09.**
  `rpc.rs` `close_surface` held the state lock and then called `inner.surface_term()`,
  which takes the same lock again; tokio locks are not re-entrant, so any tab close hung
  every later request. It now reads the term from the guard it already holds
  (`state.term_cloned(surface)`) and drops the guard before awaiting. Verified against a
  live daemon: `surface.close` and `workspace.close` both answer and the daemon keeps
  serving.
- **N2. tmux replies go to the wrong caller.** tmux sends one reply block for each command in a `;`-chained line. `new_terminal` (`tmux.rs:187-208`) chains 6 commands but registers one waiter, so 5 extra replies go to whichever commands are queued next, and every later reply shifts along.
  - **Fix:** register one waiter per command.
- **N3. Partial tmux lines are dropped.** The select loop clears `line` at the start of every pass (`tmux.rs:641`). If a command arrives while `read_until` has read half a line, that half is lost. Output gets corrupted, or a split `%end` line shifts every later reply.
  - **Fix:** clear the buffer only after a full line has been handled.
- **S1. The websocket has no authentication** (`daemon.rs:991`). Three groups can reach `127.0.0.1:7878` and send `TermInput`, which runs a shell as the daemon user:
  - any local user on the server;
  - any page loaded in the managed Chromium;
  - anything on the laptop that can reach the `ssh -L` port.

  The comment at `deploy/lmuxd.service:8-11` says nothing is exposed, which is wrong.
  - **Fix:** tunnel to a 0600 unix socket, or require a token, and reject requests that carry an `Origin` header.
- **S2. Chromium's DevTools port is open to local users** (`chromium.rs:48`, `--remote-debugging-port=0`). Another local user can find the port and take over the browser and its cookies. The profile directory is also created 0755.
  - **Fix:** use `--remote-debugging-pipe`, and create the profile directory 0700.
- **N8. Newlines and `#` in user input are interpreted by tmux** (`tmux.rs:491`, `:173-176`).
  - `shell_quote` doesn't escape newlines, so text after a newline in a title, cwd or command runs as a separate tmux command. This was tested, and it also feeds N2.
  - `-n` and `-c` values are expanded as tmux formats, so `#(cmd)` in a name or cwd runs a shell command.
  - **Fix:** reject newlines, and replace `#` with `##`.
- **H1. A JavaScript dialog hangs the browser and that client's terminals.** CDP calls have no timeout (`cdp.rs:114`), and `Page.javascriptDialogOpening` is never handled. After a page calls `alert()`, `confirm()` or shows a `beforeunload` prompt, these calls never return:
  - `Runtime.evaluate` and `Input.*` calls;
  - every `browser.*` RPC;
  - that client's message loop (`daemon.rs:1149`), which also blocks its terminal input.
  - **Fix:** dismiss dialogs automatically, and put a timeout on `call`.
- **H2. One large CDP message kills browser control for good.** The connection keeps tungstenite's default 16 MiB message limit (`cdp.rs:72`), so a big screenshot or HTML dump ends `read_loop`. The supervisor only watches for the Chromium process exiting (`lib.rs:1249`), so nothing reconnects.
  - **Fix:** raise the limit, and kill Chromium when `read_loop` exits so the supervisor restarts it.
- **A1. Browser clicks land in the wrong place when the stream isn't 1:1 with CSS pixels** (`app/src/ui/browser.rs:115-119`). This happens at scale 2, or when the quality ladder drops to 0.6×.
  - **Fix:** map the pointer through `image_rect` to the CSS size last sent in `BrowserResize`.

## Daemon (`lmux-core`)

- **N5. Recovering a lagging client breaks its other terminals** (`daemon.rs:335-371`).
  - `clear()` drops queued output for all of the client's surfaces, and its pending RPC results, but only one surface is resynced.
  - The resync clears again after taking its capture, which throws away output newer than the snapshot.
  - Subscribe (`daemon.rs:1066-1083`) registers before capturing, so newer output can arrive before the older snapshot.
- **N6. Saves can be postponed forever.** The debounce restarts on every change and has no maximum wait (`daemon.rs:682-688`). Only Ctrl-C triggers the final save; systemd stops the service with SIGTERM (`daemon.rs:765`).
  - **Fix:** cap the wait at about 2 s, and handle SIGTERM.
- **N7. A dead tmux server is never recreated.** `ensure_session` runs only at startup (`tmux.rs:96`), so the supervisor keeps retrying `attach` against a dead server. Each retry triggers a reconcile that can block for up to 30 s (`daemon.rs:812`).
- **N9. Screencasts keep running after a client disconnects** (`daemon.rs:266-272`). Only an explicit Unsubscribe stops them.
- **N10. Browser surfaces don't survive a daemon restart.** `reconcile` keeps them in the tree (`daemon.rs:534`), but nothing re-creates their pages, so every `browser.*` call fails.
- **N11. A terminal that exits immediately can leave a dead surface.** This happens if its window closes before `set_term` runs (`rpc.rs:191-198`). It is plausible, not confirmed.
- **Smaller items:**
  - Panics from `unwrap()` if a workspace is closed mid-request (`rpc.rs:382`, `:1074`, `:1102`).
  - A pane named from another workspace creates an orphan pane (`rpc.rs:1073`, `:1101`).
  - `shift+a` maps to `"Sa"`, which is typed literally (`keys.rs:65`).
  - Calling `mark_read` twice lowers the unread count twice (`rpc.rs:771`).
  - Notifications are never capped (`tree.rs:626`).
  - A single `accept()` error stops the websocket or unix server (`daemon.rs:927`, `:976`).
  - The Hello protocol version is never checked (`daemon.rs:1060`).
  - `pane.resize` sends `path`, but the daemon only resolves by `pane` (`rpc.rs:480-492`).

## Browser (`lmux-browser`)

- **B1. A lost frame freezes the stream.** If a delivered frame is dropped (no subscribers, or `sink.clear()`), it is never acked, so `in_flight` never clears.
  - **Fix:** expire `in_flight` after about 1 s.
- **B2. A resize doesn't apply new screencast settings.** `start_screencast` re-issues `Page.startScreencast` without stopping first (`lib.rs:307`), so the new size and quality are ignored. Pacing is also reset while Chrome keeps the old settings.
- **B3. Acks aren't matched to their frame.** `ack_frame` clears `in_flight` whatever `seq` was acked (`lib.rs:344`).
- **B4. Switching tabs doesn't move the screencast** (`lib.rs:993-1052`). The viewer keeps showing the old tab while input goes to the new one.
- **P1. Chromium is orphaned when the daemon exits with an error** (`main.rs:457`, `KillMode=process`).
  - On restart, `clear_stale_lock` removes the profile lock, so two Chromiums can share one profile and corrupt it.
  - If port 7878 is taken, every restart attempt leaves another Chromium behind.
- **P2. A Chromium restart leaves viewers on a frozen frame.** `rebuild_surfaces` never restarts the screencast (`lib.rs:1307`).
- **P3. A renderer crash removes the whole pane** (`lib.rs:1349`). It should reload the page instead.
- **L1. Each closed surface leaks its event-loop task**, along with its buffers and last frame (`surface.rs:242`).
- **L2. A failed `create_tab` leaves a hidden tab behind** (`lib.rs:1151`). Bare URLs like `example.com` get no scheme added, so they hit this.
- **L3. `goto` ignores `errorText`** (`lib.rs:760`), so it reports success on DNS errors.
- **L4. `target=_blank` links and `window.open` create invisible, untracked tabs.**
- **Smaller items:**
  - Mouse moves always send `buttons: 0` (`lib.rs:444`), so drag-select doesn't work.
  - Snapshot text doesn't escape newlines (`js.rs:121`).

## CLI and deploy

- **C1. `lmux send ls --enter` types `--enter` as text.** `trailing_var_arg` turns every flag after the first word into text (`main.rs:158`, `:172`). Reproduced.
- **C2. The flag is `--kind`, not `--type`** (`main.rs:89`, `:99`, `:114`), so cmux-style `--type browser` fails.
- **C3. `browser screenshot --path rel.png` resolves relative to the daemon's working directory** (`main.rs:865`).
  - **Fix:** make the path absolute in the CLI.
- **C4. Output and options are missing.** `browser tab list` and `scroll` print only "OK" without `--json`, and `scroll` and `press` have no `--target`.
- **D1. The unit file has an ordering cycle.** `After=default.target` plus `WantedBy=default.target` (`lmuxd.service:16`, `:31`) can make systemd skip lmuxd at boot.
  - **Fix:** remove the `After=` line.

## App (`app/`)

- **A2. Ghost text from other panes.** The shared cell buffer is never cleared (`ui/terminal.rs:146-149`), so a grid smaller than its pane shows the previous pane's cells.
- **A3. The browser streams at 1280×800 after every re-subscribe.** `sent_size` is never reset (`state.rs:71`), so switching tabs or reconnecting leaves the stream at the wrong size.
- **A4. A failed JPEG decode freezes the stream for good.** The frame is never acked (`decode.rs:51`).
- **A5. Switching hosts while "connecting…" hangs, or connects to the old host.** `begin_connect` returns early while connecting (`net.rs:382`, also `app.rs:111`).
- **A6. The macOS Option key prepends ESC to the composed character** (`keys.rs:125`). Option+f sends `ESC ƒ`, and German Option+L sends `ESC @`.
  - **Fix:** add an "Option as Meta" setting.
- **A7. Space may be sent twice:** once from `Key::Space` and once from `Event::Text(" ")` (`keys.rs:29`). Plausible, not confirmed.
- **A8. Terminal input modes are ignored.**
  - Arrow keys never send the application-cursor form (`SS3`) (`keys.rs:30-35`).
  - Pastes are never bracketed (`app.rs:381`), so a multi-line paste runs each line immediately.
  - The snapshot doesn't restore these modes.
- **A9. Scrolling problems.**
  - Typing doesn't jump back to the bottom.
  - The wheel does nothing useful in vim, less or htop (`ui/terminal.rs:127-135`).
- **A10. Dropping a chip on another pane's edge splits the wrong pane.** `surface.split` has no target pane (`ui/drag.rs:80`, `methods.rs:38`).
- **A11. Browser panes have no copy or paste.** Shifted letters also send a lowercase key, and key-up events are never sent (`app.rs:396-414`).
- **A12. A divider drag can stay stuck** if the divider isn't drawn on the frame the mouse is released (`ui/pane.rs:30`, `:135`).
- **Smaller items:**
  - Decoded frames re-create views for surfaces that were already closed (`state.rs:623`).
  - A daemon without `system.ping` makes an error banner appear every 2 s.
  - Box-drawing characters are drawn one shape per cell.

## Further performance work

Nothing here has been benchmarked. Items 1–7 were found in the code. Items 8–10 are ideas to measure before committing to.

1. **Browser frames can delay typing.** Terminal output, keystroke echo and browser JPEGs share one websocket over one SSH channel. A large frame on a slow link makes the next terminal echo wait behind it, which can also make the 3 s ping time out falsely.
   - **Fix:** give each client two send queues, and always drain terminal output and RPC replies before browser frames.
2. **Fix B2.** A resize doesn't apply new screencast settings, so Chrome keeps its old size, quality and frame rate. Fixing it improves both correctness and speed.
3. **Drop old frames before decoding.** The app's decode queue is unbounded. Keep only the newest frame for each surface.
4. **Stop copying frames.**
   - The CDP event loop clones each base64 frame (`cdp.rs:178`); move it instead.
   - The daemon copies each frame into a `Vec` per send (`daemon.rs:1019`). tungstenite 0.26+ takes `Bytes`, which removes that copy.
5. **Batch box-drawing characters.** They are drawn one shape per cell, which makes tmux and htop borders expensive. Draw them as normal text runs.
6. **Use tmux flow control.** Without it, a client that can't keep up makes the daemon buffer output and then throw it away; with flow control, tmux pauses the pane. `pause-after` was tried and makes tmux 3.3a stop sending `%output` (`daemon.rs:657`). Look for another approach, or retest on a newer tmux.
7. **Small server items.**
   - `workspace.new` runs `git` inline (`rpc.rs:345`).
   - Finding which surface a browser target belongs to scans every surface under nested locks; add a targetId-to-surface map.
8. **Keystroke batching (measure first).** Each keystroke is probably its own `send-keys -H` command. Merging keys that arrive within about 2 ms would help pastes and fast typing.
   - Add a test that measures the time from a keystroke to its echo first.
9. **Compress terminal output (measure first).** Use websocket permessage-deflate on terminal output only, not on JPEGs. Avoid SSH `-C`, which wastes CPU compressing JPEGs.
10. **Idle repaints.** Check that the app only repaints when something changes; see H-2 below.

## Resources used by sessions nobody is looking at

### What already works

- **Only drawn panes are streamed.** Each frame, the app records which panes it drew, and unsubscribes every terminal or browser it didn't draw (`app/src/state.rs:650-703`). That covers other workspaces, inactive tabs inside a pane, and anything off screen.
- **Hidden panes cost nothing to draw.** The app does no layout, painting or decoding for them.
- **Minimising the window stops all streaming** (`app.rs:101-105`, and `logic()` at `app.rs:123`).
- **The server stops work for panes nobody watches.**
  - Browser: when the last viewer unsubscribes, the daemon stops the screencast, so Chrome stops encoding (`daemon.rs:1097-1108`).
  - Terminal: output for a pane with no subscribers is dropped before a frame is built.

### Gaps

- **H-1. A window covered by another window, or on another macOS Space, keeps streaming.**
  - What happens: the app only checks `minimized` (`app.rs:62`, `:123`). Every visible pane keeps streaming and redrawing; a browser pane can send up to 60 JPEGs a second over SSH, all decoded for nobody.
  - **Fix:** use `viewport().visible()`, which egui 0.36 sets to false when the window is minimised or fully covered (`egui-0.36.2/src/data/input/viewport_info.rs:92`). This is a 2-line change.
- **H-2. The app redraws the whole window about every 2 s while idle.**
  - What happens: the ping timer (`net.rs:568`) requests a full repaint each time it fires. It is a small cost, but a steady GPU wake-up while lmux is in the background.
  - **Fix:** run the timer on the network thread, and wake the UI only when the connection state changes.
- **H-3. Chromium runs every page at full speed, even pages nobody watches.**
  - What happens: `chromium.rs:57-59` turns off all background throttling, on purpose, so screencasts keep animating. As a result, a hidden pane with a heavy page (ads, video, a dashboard that polls) uses full CPU on the server indefinitely.
  - **Option:** freeze or throttle a page once it has had no viewers for about 30 s (for example, with `Page.setWebLifecycleState`), and restore it on subscribe.
  - **Catch:** skip the freeze for surfaces that received a `browser.*` command recently, so a page an agent is driving keeps working.
- **H-4. A screencast keeps running after the app disconnects without unsubscribing,** for example on laptop sleep or a network drop. This is the same bug as N9. Chrome keeps encoding for nobody until the daemon restarts.
  - **Fix:** stop screencasts for every surface the client was subscribed to when it disconnects.

H-1 and H-4 are the cheap, clearly wasteful ones to fix first.

## Suggested order

1. N2, N3, S1 and N8: the corrupted replies and the command-execution paths (N1, the
   deadlock, is fixed).
2. H1, H2, A1, A4, B1 and P1: browser hangs, freezes and orphaned processes.
3. N5, N6, A2, A3 and C1.
4. Everything else.
