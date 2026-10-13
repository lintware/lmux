# Handoff: typed text lands on the input box's top border

## 1. The task

The user sent a screenshot of an lmux terminal tab:
`/Users/vas/.grok/sessions/%2FUsers%2Fvas%2FDocuments%2Flmux/01a130bd-c0bb-7183-b990-e1e03081cf1d/assets/image-a1cf74d8-f708-40d4-a380-f84d31179c50.png`.
The typed text (`dsfjksdflkjsd` plus the block cursor) is drawn on the input box's top border (`──dsfjksdflkjsd`). That is one row above the `❯` prompt line. The `❯` line still shows the dim placeholder `! scripts/secret-intake/deploy.sh`. Typed or pasted text should appear on the `❯` line. Find the cause, fix it in the terminal code paths with a small change, add a unit test that fails without the fix, and check whether local panes, remote panes, or both are affected.

## 2. Root cause: what is known

### Confirmed

- **The tab is a remote pane, and it runs Claude Code, not Grok.**
  - Host `lint-hqc-01`, tmux pane `%3`, `surface:63`, `workspace:33`.
  - It runs Claude Code 2.1.296. The screen text matches it: "Cooked for", "⏵⏵ auto mode on".
  - I found it by grepping `capture-pane` output on every host for `secret-intake/deploy.sh`. No local pane matched.
- **tmux has the right screen. The app's emulator is wrong.**
  - On lint-hqc-01, `tmux -L lmux capture-pane -p -t %3` shows `❯ dsfjksdflkjsd` on the `❯` line.
  - tmux reports cursor 15,34, pane 131x38, `alternate_on=0`, `history_size=1877`.
  - tmux is 3.3a. The window has `window-size manual` and size 131x38. The tree also says 131x38.
  - The only client is the daemon's control client.
- **Claude Code on that host uses its default inline renderer.** It draws on the main screen, not the alternate screen.
  - The remote `~/.claude/settings.json` has no `tui` key.
  - Locally, `~/.claude/settings.json` has `"tui": "fullscreen"`, so local Claude panes use the alternate screen (`alt=1`).
  - To reproduce locally, you must start Claude with `claude --settings '{"tui":"default"}'`.
- **The inline renderer draws a keystroke with cursor-relative moves only.** This is the byte sequence recorded from the stream for one key:
  ```
  ESC[?2026h ESC[?25l ESC[15D ESC[3B \r ESC[15C ESC[3A x \r\r\n \r\n \r\n ESC[16C ESC[3A ESC[?25h ESC[?2026l
  ```
  So the app's cursor row decides where the text lands. If the app's cursor is one row higher than tmux's cursor, the text goes on the border above `❯`.
- **A fresh snapshot is correct.**
  - I took a `term_snapshot` of `surface:63` through an ssh tunnel. Watching only, no input.
  - I replayed it into alacritty_terminal 0.26 at 131x38.
  - The result matches tmux's capture exactly: cursor row 34, col 15.
  - So the bad state built up in the app's emulator over time. Re-subscribing (switching tabs away for more than the hide TTL, or reconnecting) should repair the screen.
- **A grid one row shorter than tmux gives exactly the screenshot.**
  - I replayed the remote snapshot into a 37-row grid while tmux has 38 rows, then fed the keystroke bytes above.
  - The `x` landed on the border row (row 33), and the `❯` line kept its old text.
  - At 38 rows it landed on the `❯` line.
- **There is a real race between subscribe and resize.** It produces a broken screen (recording `/tmp/lmuxrec3.json`).
  - The app sends `subscribe` and `term_resize` in the same pass (`AppState::sync_streamed_terms` in `app/src/state.rs`).
  - The daemon answers with a snapshot at the old size, 131x37. Then tmux resizes the window, and the program redraws for 38 rows.
  - `apply_snapshot` (`app/src/term.rs`) sets the grid to the snapshot's size (37).
  - If the redraw bytes are fed before the next `sync_streamed_terms` resizes the grid to 38, they are drawn into a 37-row grid.
  - Replaying in that order ("order A", `/tmp/rec3_A.json`) breaks the footer: `──⏵⏵ auto mode on…` on one row.
  - Replaying with the resize first ("order B") matches tmux.
- **There is a code-level gap.**
  - The app never checks its grid size against the size tmux actually has. The tree carries `cols`/`rows` per surface, but nothing compares them.
  - Since `window-size manual` means the last `resize-window` wins, a different-sized viewer can leave this app's grid out of step with tmux. The app only sends `TermResize` when its own desired size changes.
  - Example: the same host viewed from a second app window or a second Mac.
  - The app would then keep a grid that does not match tmux, with no repair.
- **Snapshots leave one blank line in alacritty history.**
  - A main-screen snapshot starts with `ESC[2J ESC[H` (`snapshot_with_history` in `crates/lmux-core/src/tmux.rs`).
  - In alacritty, `ESC[2J` calls `clear_viewport`, which scrolls the screen into history even on a fresh, empty grid. After the remote snapshot the replay showed `hist=1`.
  - On a later shrink-then-grow, alacritty pulls that line back from history (`grow_lines`). Content and cursor move down one row. tmux instead adds blanks at the bottom when `hscrolled=0`.
  - `/tmp/lmuxreplay/src/bin/growprobe.rs` shows it: a 10-row grid with the prompt at row 7, resized 10→9→10, ends with the prompt at row 8. Without the `ESC[2J` prefix it stays at row 7.

### Suspected, not proven

- **What exactly caused the user's one-row shift is not known.** Candidates, most likely first:
  - (a) The app's grid height differs from tmux's by one row. Causes could be the subscribe/resize race above, or the size-reconcile gap.
  - (b) The extra history line from `ESC[2J` in the snapshot. Content shifts one row on grow, and later absolute moves (`ESC[H` in Claude's full redraws) disagree with relative ones.
  - Both give a one-row error. I could not catch the bad state live. The user's real app view was not inspected, and I did not touch it.
- **Width disagreement (cause b in the brief) is ruled out for this pane.**
  - `/tmp/lmuxreplay/src/bin/widthprobe.rs` fed all 2000 lines of the pane's history into a 131-column alacritty grid. None wrapped.
  - Characters present: `❯ ⎿ ⏵ ● ✻ ─ ✅`, NBSP and others.
- **Wrong `TERM` / sync output (cause e) does not look like the cause.**
  - Panes get `TERM=screen-256color`, `TERM_PROGRAM=ghostty` and `TMUX=` empty.
  - Claude emits `ESC[?2026h/l` (synchronized output). vte 0.15 buffers it, and the replay handles it correctly.
- **Renderer cursor bug (cause d) does not look like the cause.** The grid content itself is wrong, not just where the cursor is drawn.
- **Local panes are probably not affected, but this was not tested live.**
  - Local panes are fed by a real `tmux attach` client.
  - tmux redraws its client with absolute positions at the PTY size, and the PTY and grid are resized together (`sync_local_terms`).
  - All local agent panes run full-screen (alternate screen).

## 3. Changes made

None. No file in the repo was edited, except this handoff file. All the work was investigation with scratch tools in `/tmp`.

## 4. Tests and live checks

- I did not run `cargo test` because there is no code change yet.
- Replay comparisons were done with the tools in section 6. The recording files are in `/tmp`:
  - Fullscreen Claude, typing, bash-mode output, resizes 38→37→38→36→38, grow-then-shrink: replay **matches** tmux (`lmuxrec1/2/4/5.json`).
  - Fullscreen Claude, subscribe at 37 then resize to 38, replayed in "order A": **broken** footer (`rec3_A.json`). In "order B": matches.
  - Inline Claude (`tui: default`), resize 38→34→38, 38→37→38, fast 37→38, hidden-tab resize then re-show, `/clear` then grow 30→38: the `❯` row **matches** tmux. Only the top rows differ, because the app had no history yet (`inl*.json`, `g1.json`, `c1/c2.json`, `f1.json`).
  - Remote `surface:63` snapshot replay: matches tmux (`remote63.json`).
  - Remote snapshot in a 37-row grid plus one keystroke: **reproduces the screenshot** (`hyp_37.json`).
- No screenshots were taken. No test app bundle was built or run.

## 5. What is left (in order)

1. **Decide the fix.** Suggested, smallest first. All of these touch the terminal path only.
   - **Order the resize before the snapshot.** In `AppState::sync_streamed_terms` (`app/src/state.rs` ~line 1734), when subscribing, push `TermResize` with the desired size *before* `Subscribe`. Also `t.resize` the grid then. The daemon handles one client's messages in order (`handle_client_msg`), so the snapshot is then taken at the new size. Check that `ClientMsg::TermResize` in `crates/lmux-core/src/daemon.rs` (~line 1925) waits for `resize-window` to finish before the next message is handled. It does: it awaits `self.tmux.resize(...)`. This is app-only; remote hosts need nothing.
   - **Do not feed stream bytes into a grid of the wrong size.** In `ServerMsg::TermSnapshot` handling (`state.rs` ~line 1454), if the snapshot size differs from `desired_sizes[surface]`, send a `TermResize` right away. Optionally mark the view so it re-subscribes once the tree shows the new size.
   - **Reconcile with the tree.** When `apply_tree` gets a terminal surface whose `cols/rows` differ from a subscribed `TermView`'s size and from the desired size, resend `TermResize`. Another viewer may have resized the window.
   - **Avoid the extra history line** (daemon side, `snapshot_with_history` in `crates/lmux-core/src/tmux.rs`): a fresh `Term` needs no `ESC[2J`. You could also handle it app-side in `TermView::apply_snapshot` by calling `self.term.grid_mut().clear_history()` after feeding when `history_scroll` is `None`. The app-side version needs no remote install.
2. **Add unit tests in `app/src/term.rs` tests** (pure `TermView`, no tmux):
   - Feed a main-screen snapshot string starting with `\x1b[2J\x1b[H` into `apply_snapshot`. Resize rows −1 then +1, then feed the keystroke sequence from section 2. Assert the `x` is on the `❯` row (`row_chars`) and the cursor row is unchanged.
   - For the race fix, add a test in `state.rs` tests (see `resize_debounce_collapses_a_storm` and the test near line 2342). It should assert that `pending` holds `TermResize` before `Subscribe` for a newly visible streamed term.
3. Run `(cd app && cargo test)`. If `crates/` is touched, also run `cargo test --workspace`.
4. **Live check.** Build with `LMUX_SKIP_INSTALL=1 deploy/macos/bundle.sh`. Start a dev daemon (section 6) and run `claude --settings '{"tui":"default"}'` in it. Point the bundle at it with `LMUX_WS_URL=ws://127.0.0.1:7911`. Resize the window by a row and switch tabs, type, and screenshot with `screencapture -l <id>` (`scratchpad/winid.swift`).
5. **Remote hosts.** If only `app/` changes, no `deploy/install-remote.sh` is needed. If `crates/lmux-core` changes (the snapshot `ESC[2J`), run `deploy/install-remote.sh lint-hqc-01`, `lint-hqc-03` and `lint-cloud-noverse01`.
6. Delete the scratch files listed below when done.

## 6. Test setup

- **Dev daemon** (stopped now). Start it again with:
  ```sh
  cd /Users/vas/Documents/lmux && cargo build -p lmux
  export LMUX_SOCKET=/tmp/lmux-dev.sock
  target/debug/lmux server --socket "$LMUX_SOCKET" --tmux-socket lmux-dev \
    --state-dir /tmp/lmux-dev-state --ws-addr 127.0.0.1:7911 --no-browser &
  target/debug/lmux new-workspace --name inline --cwd /tmp --command "claude --settings '{\"tui\":\"default\"}'"
  ```
  Use port 7911. Port 7899 belongs to another agent's daemon (`--tmux-socket lmux-pd`, pid 76200). Leave it alone.
- **Recorder:** `/tmp/lmuxrec.py`, run with `/tmp/lmuxrec-venv/bin/python` (it has `websockets`).
  - Args: `URL SURFACE OUT.json COLS ROWS STEPS_JSON [TAIL_SECONDS]`.
  - `COLS=0` sends no resize, which makes it a pure watcher.
  - A step is `[delay, "text"]` or `[delay, {"resize":[c,r]}]`.
  - It logs the snapshot, output bytes (hex), inputs and resizes with timestamps.
  - **Careful: steps with text type into the pane.** Only use text steps on dev panes.
- **Replay tools:** Rust crate `/tmp/lmuxreplay`. It pins `alacritty_terminal = "=0.26.0"`, the same as the app. Build with `cargo build`.
  - `target/debug/lmuxreplay REC.json [cols rows]` prints the final screen and cursor. Optional `cols rows` resizes after the snapshot like the app does; `0 0` means no resize.
  - `target/debug/steps REC.json` prints rows, history, cursor and `❯` row after each event.
  - `snapprobe`: which snapshot line pushes history.
  - `widthprobe CAPTURE.ans COLS`: lines that wrap in alacritty.
  - `growprobe`: the `ESC[2J` + resize shift demo.
- `/tmp/cmp.sh NAME SURFACE PANE COLS ROWS STEPS` records, captures tmux (`-L lmux-dev`), replays and diffs.
- **Remote, watch only:** `ssh -f -N -L 7922:127.0.0.1:7878 lint-hqc-01`, then run the recorder with `COLS=0` and `STEPS='[]'` on `surface:63`. The tunnel is closed now.
- **What still runs:** nothing of mine. The dev daemon, the `lmux-dev` tmux server and the ssh tunnel are stopped, and `/tmp/lmux-dev-state` is removed.
- Scratch files still on disk, kept for reuse; delete them at the end:
  - `/tmp/lmuxrec.py`, `/tmp/lmuxrec-venv/`, `/tmp/lmuxreplay/`, `/tmp/cmp.sh`
  - `/tmp/*.json` recordings (`lmuxrec*`, `rec3_*`, `inl*`, `g1`, `c0-2`, `f0/f1*`, `fill`, `hyp_*`, `remote63`)
  - `/tmp/*_tmux.txt`, `/tmp/*_replay.txt`, `/tmp/remote3*.{ans,txt}`, `/tmp/hqc01_tree.txt`, `/tmp/lmux-dev.log`

## 7. Traps

- **The screenshot is Claude Code on lint-hqc-01, not Grok.** It is easy to chase grok locally for nothing.
- **Local Claude uses `tui: fullscreen` (alternate screen) and does not show the problem.** Use `claude --settings '{"tui":"default"}'` to match the remote.
- **The recorder's text steps type for real.** My first run went to another agent's daemon on port 7899 (my daemon failed to bind it: "Address already in use" in the log). It typed `dsfjksdflkjsd` into that daemon's zsh pane. I erased it with Ctrl+U. Check `lsof -iTCP:<port> -sTCP:LISTEN` before you send input.
- **Leftover input breaks commands.** Text left in Claude's input from an earlier run turned `/clear` into `dsf/clear`. Send `\u0015` (Ctrl+U) first.
- **`lmuxreplay` with forced size `0 0` panicked** inside alacritty's resize. It is now treated as "no resize".
- **`lmux tree --json` surfaces have `kind` as a string** (`"terminal"`), with `cols`/`rows`/`tmux_pane` as sibling fields.
- **Single-client replays of one recording all matched tmux.** The bug only shows when the app's grid height and tmux's differ, or when bytes and resizes are applied in a different order than tmux saw them. A test must construct that order on purpose.
- **Other agents are editing** `app/src/app.rs`, `clip.rs`, `menu.rs`, `webview.rs`, `net.rs`, `ui/browser.rs` and `crates/lmux-browser/`. `state.rs`, `term.rs` and `ui/terminal.rs` carry the user's uncommitted work (file-drop copy, Copy IDs, an OSC 52 test). Edit around it and do not reformat.
