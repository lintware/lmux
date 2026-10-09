# HANDOFF — sidebar drag-and-drop, folder moves, card controls

Written 2026-10-09 across two sessions: the earlier one added the local host and
the create menus (kept below), the later one added dragging sidebar rows between
folders and sessions, `folder.move`, the folder `+`, the card's close button and
menu, and fixed the close deadlock. The tests are green.

## What the sidebar session built

1. **Sidebar rows drag and drop.** A workspace card or a folder row is now a drag
   source (`app/src/ui/sidebar.rs`: `Sense::click_and_drag` →
   `AppState::begin_tab_drag`). `app/src/ui/drag.rs` resolves the drop against
   `AppState::sidebar_rows`, the rows drawing recorded this frame, and paints one
   of two indicators: an accent **line** at the gap, or the **filled folder row**
   when the row goes inside a folder.
2. **What each drop means.** Over a card: in front of it or behind it, in that
   card's folder (top half / bottom half). Over a folder row: a session goes
   inside it; a folder joins it, unless the pointer is in the row's top third,
   where the folder takes that row's own slot beside it. Below every row: the
   top level, appended. Dropping a row beside itself resolves to nothing, so no
   RPC goes out and no line is drawn. A folder cannot be dropped into its own
   subtree — refused in the resolver, in the daemon and in the mock.
3. **`folder.move {folder, parent?, index?}`.** New RPC (`crates/lmux-proto`,
   `SessionState::move_folder` in `crates/lmux-core/src/tree.rs`, handler in
   `rpc.rs`, `lmux move-folder`, mirrored in `app/mock/server.ts`). `parent: null`
   is the top level, `index` counts that parent's child folders *without* the
   moved folder, so a same-parent reorder is not off by one. `workspace.move`
   already existed and the mock now honours its `index` too.
4. **`index` from a slot.** `workspace_move_index` / `folder_move_index` in
   `app/src/state.rs` turn "the k-th row of that folder" into the list index the
   daemon wants; `insertion_slot` is the pure rule that makes a drop beside
   itself a no-op.
5. **Card controls.** A folder row shows a `+` on hover (a session in that
   folder) and a card shows a `×` on hover (close that session) beside its unread
   badge — the previously open item, now done. A card's right-click menu has
   `New Session Here`, `Move to Folder ▸ <trail>` (plus `Top Level`) and
   `Close Session`. The row controls hide while a drag is in flight.
6. **The close deadlock is fixed** (see below).

## Earlier session: local host, host picker, nested folders

1. **Nested folders, end to end.** `Folder.parent` in `crates/lmux-proto`, mirrored in
   `app/mock/proto.ts`. `folder.new {name, parent?}` nests; `folder.close` lifts the
   folder's workspaces *and* child folders to its own parent. The sidebar walks the tree
   recursively (`folder_level` in `app/src/ui/sidebar.rs`) and a new folder opens an
   inline rename editor as soon as the daemon reports its ref. `folder.new` now answers
   with the **ref** (it used to answer with the whole folder object, which also made
   `lmux new-folder` print no ref).
2. **Sidebar and tab gestures.** Empty sidebar area or the `+` → a new session in the
   selected workspace's folder; double click on a workspace card → a session in that
   card's folder; double click on a tab strip's empty run → a new terminal tab in that
   pane; right click on a folder row → `New Session Here`, `New Folder Inside`,
   `Rename Folder`, `Close Folder`.
3. **A local host, and create asks which host.** The app starts (or adopts) a daemon on
   this Mac and makes it the default host; `my-server` stays as a remote connection.
   `net.rs::ensure_local_daemon` spawns `lmux server --ws-addr 127.0.0.1:7879
   --no-browser` with a PATH that finds tmux in `~/.local/bin`; `load_hosts_with_local`
   puts "this mac" first and `Net::new` selects it. `LMUX_WS_URL` still overrides.
   `state.rs::create_targets` lists every host with the folders it knows (the live tree
   for the connected host, a cache per host for the rest), and `create_in` fires at once
   for the connected host, otherwise switches host and fires when that host's tree
   arrives (`pending_create` + `fire_pending_create` in `apply_tree`).
4. **tmux on the Mac.** Homebrew's prefix is not writable by this user
   (`/opt/homebrew` is owned by uid 502), so `brew install tmux` fails. tmux 3.8 is
   built into `~/.local/bin/tmux` against brew's libevent plus a source-built utf8proc
   2.12.0, configured with `--enable-utf8proc --disable-jemalloc` (both are mandatory
   on macOS) and linked against the **static** utf8proc, because its dylib install-name
   points at `/usr/local/lib`, which does not exist here. `sudo chown -R vas /opt/homebrew`
   would fix brew itself.

## Verified

- `cd app && cargo test`: 62 lib, 28 interaction, 16 snapshots, plus the smaller suites.
- `cargo test --workspace`: 45 `lmux-core` (including nested folders and close-promotion).
- Mock over the real websocket (`bun app/mock/server.ts`, ws `127.0.0.1:7900`): nested
  folder create, a session inside a nested folder, close-promotion, and tab creation.
- A local session really runs on this Mac: a daemon on `127.0.0.1:7879` created a
  workspace whose shell answered `echo` (`vas@Vass-MacBook-Air ~ %`).
- The app on the rebuilt binary showed `this mac · connected` and the local tree, with a
  working local shell in a pane. The host menus are covered by the `sidebar_menu` and
  `sidebar_folder_menu` snapshots and by interaction tests that drive real pointer
  events; a live right click was not exercised (no right-click tool in this session).

## FIXED — closing a surface used to deadlock the daemon

The user hit this: the app reconnected every ~5.5 s, the UI was laggy, the CLI hung.

Root cause (now fixed in `crates/lmux-core/src/rpc.rs`, `close_surface`): the state
guard was held across `inner.surface_term(surface).await`, which takes the same lock.
Tokio's Mutex is not reentrant, so the task blocked forever **holding the lock**, and
`handle_ws_conn` awaits `inner.tree().await` before spawning its writer task, so clients
received nothing at all. `close_surface` now reads the term from the guard it already
holds (`state.term_cloned(surface)`) and drops the guard before awaiting.

Verified 2026-10-09: a scratch daemon on `127.0.0.1:7933` created a workspace, answered
`close-surface --surface surface:1` immediately, kept serving `tree`, and `close-workspace`
then reported the (now empty) workspace as not found. This was `docs/review-2026-10-09.md` **N1**; the
entry there is marked fixed.

Health check, if a wedged daemon is suspected (a healthy one answers at once):

```bash
bun -e 'const w=new WebSocket("ws://127.0.0.1:7879");const g=[];w.addEventListener("message",e=>g.push(e.data));await new Promise(r=>w.addEventListener("open",r));await new Promise(r=>setTimeout(r,3000));console.log(g.length?g.map(s=>String(s).slice(0,40)):"(nothing)");w.close();process.exit(0)'
```

## DONE — a close button on the sidebar's workspace cards

Implemented as a hover `×` at the card's top right (`ws_card` in
`app/src/ui/sidebar.rs`), beside the unread badge, issuing `workspace.close`; the same
card's menu also has `Close Session`. Safe now that the deadlock above is fixed, and
covered by `the_cards_close_button_closes_that_session`.

## How to build, run and check

```bash
cd app && cargo build                      # the client
cargo build -p lmux                        # the daemon + CLI (repo root)

# the local daemon (the app starts this itself; by hand:)
PATH="$HOME/.local/bin:$PATH" ./target/debug/lmux server --ws-addr 127.0.0.1:7879 --no-browser
./target/debug/lmux --socket ~/.local/state/lmux/lmux.sock list-workspaces

bun app/mock/server.ts                     # the mock, ws://127.0.0.1:7900
cd app && cargo test                       # 77 lib + 31 interaction + 18 snapshots …
cargo test --workspace
```

Checked 2026-10-09 after the sidebar session: `cd app && cargo test` (77 lib, 16
drag_drop, 31 interaction, 18 snapshots, plus the small suites) and `cargo test
--workspace` are green. The sidebar drags are driven by real pointer events in
`app/tests/drag_drop.rs` (`dragging_a_session_onto_a_folder_row_moves_it_into_that_folder`
and friends); `sidebar_drag_between.png` and `sidebar_drag_into_folder.png` show the two
indicator styles. The mock was driven over the websocket for `folder.move`
(nest/reorder/cycle) and `workspace.move` with an `index`; a scratch daemon on
`127.0.0.1:7933` verified the same two RPCs plus the close paths through `lmux` itself.

- The app logs to `/tmp/lmux-app.log` when started with `nohup … > /tmp/lmux-app.log`.
- The host list is `~/Library/Application Support/lmux/hosts.json`; the local host is
  derived at runtime and is not written there.
- Browser panes are off on the local host (`--no-browser`); there is no Chromium for it.
- Local sessions survive quitting the app: the daemon and its tmux server keep running.
- **Never run `UPDATE_SNAPSHOTS=1 cargo test` for the whole snapshot suite while another
  session is editing the tree** — it rewrites every reference image, including theirs.

## Other writers in this tree

Another agent session (Claude in cmux) worked on `app/` at the same time: folder-row
double-click rename (`AppState::toggle_folder` / `take_back_folder_toggle`, plus the
`harness_with_step` helper in `tests/interaction.rs`) and a terminal-painter
investigation. Its work is in the tree and its tests pass. It also runs its own
benchmark daemon on `127.0.0.1:7990` with tmux socket `lmux-bench` and a scratchpad state
dir — leave that process alone.
