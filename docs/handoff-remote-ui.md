# Handoff: remote web and mobile UI for `lmux share` (issue #8)

Written 2026-10-11 by the UI session on branch `dev`. Updated the same day when the chat view shipped; sections 2, 6, 7 and 9 give the current status.

## 1. Goal

Build the browser UI that `lmux share` serves through its Cloudflare Quick Tunnel link. The link comes from the app's *Show QR* dialog or from `lmux share`.

- Issue: https://github.com/lintware/lmux/issues/8. Read the body and the server model's comment first (`gh issue view 8 --comments`).
- Plan: `PLAN.md`, section "Later: share a tab to your phone (QR)", around line 178.

The page must:

- List the sessions (workspaces and their tabs) with agent status for Claude, Codex, Grok, plain shells and other agents.
- Open a terminal tab to read its live screen and type into it.
- Show a readable chat view when the session has an agent transcript. Fall back to the terminal view when it does not.
- Work on a phone (16 px side gutter, no horizontal scroll) and on desktop: a sidebar on wide screens and stacked navigation on phones.
- Use a clear hierarchy and readable type, mark agents that need input, and offer a light and a dark theme.
- Use no dependencies and no third-party scripts. The server sends `Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; ...`, so the page cannot use inline `<script>`, inline `<style>` or `style=""` attributes. JS and CSS go in the separate served files.

Security rules from issue #8. Keep all of them.

- The code comes from the URL fragment (`#<code>`). The page sends it in `POST /auth` and then clears the fragment with `history.replaceState`.
- The page never stores the code or a token in `localStorage` or `sessionStorage`. The session cookie is `HttpOnly`, so JS never sees it. Storing the theme choice in `localStorage` is fine.
- View-only unless the share was started with `--allow-typing`. The tree message's `typing` flag tells the page which mode applies. The server enforces it as well.

The other model owns the server side: auth, tunnel and the WebSocket protocol. Do not change the existing protocol without a note in issue #8. The page files are ours to replace.

## 2. Decision on the transcript chat view: option 1, all three agents

The user chose the additive protocol extension in section 4, with Claude, Codex and Grok all supported from the start. The protocol note is posted in issue #8 (the last comment). Sections 3 and 4 together describe the protocol as built.

## 3. Current protocol (from the server model; do not break)

Files: `crates/lmux/src/share/{page.html,app.js,app.css}` are embedded with `include_str!` in `crates/lmux/src/share/server.rs` and served at `/`, `/app.js` and `/app.css`. A new static file needs its own route in `handle()` in `server.rs`.

Auth:

- The page sends `POST /auth` with the body `{"code":"<fragment>"}`, same-origin. The body limit is 1 KB.
- `204` sets the `lmux_session` cookie (`Max-Age` 30 days). The server ends the session after 24 hours with no connected socket; each check of a live socket extends it.
- `401` means the code is expired or already used. Codes are single use and last 5 minutes.
- `403` means a bad `Origin`.
- After auth, the page opens `wss://<host>/ws` (`ws:` on plain HTTP). The cookie rides along. A page reload with no fragment can connect directly while the cookie is still valid; the current placeholder already does this.

Server to page:

- `{"t":"tree","typing":bool,"host":str,"workspaces":[{"id","title","agent","summary","tabs":[{"id","title","kind"}]}]}`. Sent on connect, and when it changes (polled about every 2 s). `agent` is `none|running|idle|needs_input`. `summary` may be null. `kind` comes from serde's tag, so expect the string `"terminal"` or `"browser"`. Handle anything else as not openable. Only terminal tabs can be opened.
- `{"t":"screen","surface":"surface:N","text":str}`. Plain text with no colours, polled about every 0.5 s, sent only when it changes.
- `{"t":"error","msg":str}`. When the session expires, the server sends `"The session expired."` and closes the socket.

Page to server:

- `{"t":"open","surface":"surface:N"}` opens one tab at a time. `{"t":"close"}` closes it.
- `{"t":"send","surface","text","enter":bool}`. The text is at most 4096 bytes, and the server turns newlines into spaces. Only for the open tab.
- `{"t":"key","surface","key"}` with one of `enter escape esc tab up down left right backspace ctrl+c ctrl+d`.

Limits: at most 8 sockets (the 9th gets 429), plus the Quick Tunnel limits (changing URL, no uptime guarantee, 429 above 200 requests in flight).

## 4. Protocol additions (posted in issue #8, built)

All additions are additive. Existing messages keep their fields.

**a. Per-tab agent info in `tree`.** Each tab gets extra fields:

```json
{"id":"surface:1375","title":"remote-ui","kind":"terminal",
 "agent":"claude|codex|grok|shell|other|null",
 "command":"grok-1.0.50",
 "chat":true}
```

- `agent` is the detected program. `command` is tmux `pane_current_command`. `chat` is true when the server found a transcript.
- Optional, nice for the sidebar: add `cwd` and `git_branch` per workspace. Both are already in the daemon's `Workspace` struct (`crates/lmux-proto/src/lib.rs`). `tree_to_message` in `server.rs` currently drops them.

**b. A new `chat` frame**, sent after `open` for a tab with `chat:true`, and again when the transcript file changes (poll its mtime about every 1 s):

```json
{"t":"chat","surface":"surface:N","agent":"claude",
 "messages":[{"role":"user|assistant|tool","text":"...","ts":"2026-10-11T01:02:03Z"}]}
```

- Each frame is a full snapshot of the last 200 messages, so the page just re-renders it.
- Read only the tail of the file (about the last 2 MB). Cap each message's text at about 8 KB.
- Typing into chat reuses the existing `send` message with `enter:true`. No new input message is needed.
- Security: the server finds transcript paths from the daemon's data alone. The page never sends a path, and the server never logs transcript contents, the same rule as screens. Transcripts can contain secrets, just like screens can.

**Where the code would go:**

- New `crates/lmux/src/share/agents.rs` (declare `mod agents;` in `crates/lmux/src/share/mod.rs`). It detects the agent and finds and parses transcripts. Add unit tests in the same file, using small JSONL fixtures written inline.
- `crates/lmux/src/share/server.rs`:
  - Extend `tree_to_message`. Its unit test `tree_is_flattened` must keep passing; add assertions for the new fields.
  - Call the new daemon RPC `term.processes` (section 6) in `tree_message`.
  - Add chat polling to the `tick` branch of `handle_ws`, next to the screen polling.

**Agent detection (verified on this machine):**

- tmux `pane_current_command` is not the plain program name. Claude shows as its version (`2.1.296`) and Grok shows as `grok-1.0.50`. So use this order:
  1. The agent session tag `claude:<id>` or `codex:<id>`, from the daemon's resume map or the tmux window option `@lmux_agent_session`.
  2. A command name starting with `claude`, `codex` or `grok`.
  3. The names of the pane's child processes, from `ps -axo pid=,ppid=,comm=`. `crates/lmux-core/src/probe.rs` (`codex_sessions`) already walks the process tree this way.
  4. A shell name (`bash zsh fish sh ...`, see `is_shell` in `crates/lmux-core/src/daemon.rs`) means `shell`.
  5. Anything else is `other`.

**Transcript locations and parsing (formats checked against real files):**

- **Claude:** `~/.claude/projects/*/<session-id>.jsonl`. The session ID comes from the `claude:<id>` tag, which the SessionStart hook records through `agent.session`.
  - Keep lines with `type` `user` or `assistant`. `message.content` is either a string or a list of blocks.
  - Blocks: `text` gives a message. `tool_use` gives a short `tool` line with `name` plus a hint from the input. Skip `tool_result` (or show a one-line summary) and skip `thinking`.
  - Skip lines with `isMeta` or `isSidechain`, and strings starting with `<command-name>`, `<local-command-` or `<system-reminder>`. Skip all other `type`s (`attachment`, `system`, `mode`, and so on).
- **Codex:** `~/.codex/sessions/YYYY/MM/DD/rollout-<timestamp>-<session-id>.jsonl`. The ID comes from the `codex:<id>` tag, which the daemon finds with `probe::codex_sessions`.
  - Use only `type:"event_msg"` lines whose `payload.type` is `item_completed`, and read `payload.item`:
    - `UserMessage` gives `content[].text`.
    - `AgentMessage` gives `content[].text` (block type `"Text"`).
    - `CommandExecution` gives a tool line from `command` and `exit_code`.
    - `FileChange` gives a tool line from the keys of `changes`.
  - Skip `Reasoning`. Skip `response_item` lines, which duplicate the items, and skip developer and environment-context messages.
- **Grok:** `~/.grok/sessions/<url-encoded cwd>/<session-id>/chat_history.jsonl`, for example `~/.grok/sessions/%2FUsers%2Fvas%2FDocuments%2Flmux/01a126ce-.../`.
  - Line `type`s are `system`, `user`, `assistant`, `tool_result` and `reasoning`.
  - User `content` is a list of `{type:"text",text}`. When a text contains `<user_query>...</user_query>`, show only the part inside. Skip texts that start with `<system-reminder>`, `<user_info>` or `<rules>`.
  - Assistant `content` is a string plus `tool_calls[].name`. Skip `system`, `reasoning` and `tool_result`.
  - `summary.json` in the session directory has `session_summary`, a good subtitle.
  - **Problem:** the daemon records no Grok session ID. One `grok` process can hold several sessions' `events.jsonl` open (on this machine, pid 49632 had 3 open). A workable rule: list the files the pane's `grok` process has open (`lsof -n -P -p <pid> -F n`, as `probe::open_files` does on macOS), keep `.../sessions/<cwd>/<id>/events.jsonl` paths, and choose the session whose `chat_history.jsonl` changed most recently. This is a heuristic; see section 9.

**c. Gateway mode (`lmux share --server <ssh-target>`, repeatable).** The share also controls the sessions on other servers. It runs `lmux share-backend` on each one over SSH and speaks one JSON object per line with it. Agent detection, transcripts and uploads run on the server that owns the session.

- Every workspace in `tree` gets `"server":"<name>"`. This machine's ids stay bare (`surface:3`). A remote server's workspace and tab ids carry its name in front (`vps1/surface:3`), and clients send those ids back unchanged in `open`, `send` and `key`.
- In gateway mode, `tree` also carries `"servers":[{"name","status":"connected|connecting|unreachable","error":str|null,"local":bool}]`. A remote server that drops is tried again with a growing pause (1 s up to 30 s).
- An upload goes to the server of the open tab, so the path in `uploaded` is a path on that server.
- `LMUX_REMOTE_BIN` names another lmux binary on the servers (for tests). Each server needs a current lmux (`deploy/install-remote.sh`); an older one shows as unreachable with "lmux on this server is too old".

## 5. Page design (built)

`crates/lmux/src/share/page.html`, `app.js` and `app.css` implement the design below. The auth and connect flow is unchanged from the placeholder.

- **Palette:** match the app (`app/src/theme.rs`) and the site (`deploy/site/index.html`).
  - Dark: background `#12110E`, sidebar `#1A1814`, hairline `#2E2A23`, text `#F3F0E7`, dim `#A39E90`, accent `#D94320`, flame `#F07A3C`, green `#3DE07A`, amber `#FFC27A`, red `#F0563C`.
  - Light: background `#f3f0e7`, panel `#eae6dc`, text `#24241f`, dim `#64635a`, accent `#d94320`.
  - Follow `prefers-color-scheme` by default, plus a toggle in the header that remembers the choice in `localStorage` (theme only).
- **Wide layout (900 px and up):** a 300 px sidebar with workspaces as cards (status dot, title, two-line summary, tab rows), and the open session on the right.
- **Phone layout:** a full-screen session list. Tapping a tab pushes a session view with a back button, and the browser back gesture should work too (`history.pushState` with no fragment). Use a 16 px gutter, and allow no horizontal scroll: the terminal `<pre>` scrolls inside its own box. Handle the safe-area insets.
- **Status:**
  - `needs_input`: an amber dot with a pulse, a "Needs input" pill, and the card sorted to the top. Also show a count in the header.
  - `running`: green. `idle`: dim. `none`: hollow.
  - Agent kind (option 1): a small label per tab (Claude, Codex, Grok, Shell).
- **Session view:**
  - Header: workspace and tab title, the agent pill, and a Chat | Terminal segmented control when `chat` is true.
  - Chat view: user bubbles on the right; assistant messages as readable prose with basic Markdown (paragraphs, code blocks, inline code, lists). Build the DOM with `textContent` and never `innerHTML` on transcript text. Tool lines are compact and muted. Keep the view at the bottom unless the user scrolls up.
  - Terminal view: a monospace `<pre>`, font size about 12 to 13 px, with a key row (Esc, Tab, ^C, arrows, Enter) and a text line.
  - Composer: shown only when `typing` is true. Otherwise show a quiet "View only" banner.
- **States:** connecting, link expired (401), bad origin (403), session expired (error message plus close), disconnected with automatic reconnect while the cookie lasts, no sessions, and browser tabs shown as not openable. Show server `error` messages as a short toast.

## 6. What was built

- `crates/lmux-proto/src/methods.rs` and `crates/lmux-core/src/rpc.rs`: the daemon RPC `term.processes` (`{}` returns `{terminals: [{surface, pid, command, cwd, agent_session}]}`), from tmux windows, pane pids, the resume map, and the window option `@lmux_agent_session`.
- `crates/lmux/src/share/agents.rs`: agent detection and the Claude, Codex and Grok transcript parsers, with unit tests. Detection order: an agent-named command, then the session tag (only for the detected agent), then child processes, then shell, then other. Claude without a tag falls back to the newest transcript for the pane's directory. Codex and Grok use the files their process has open (one `lsof` for all of them, cached for 10 s). Grok prefers main sessions over subagent sessions (`summary.json` `session_kind`).
- `crates/lmux/src/share/server.rs`: one shared tree and detection cache for every socket (refreshed at most every 1.5 s), `chat` frames polled about once a second for the open tab, and blocking file work in `spawn_blocking`.
- `crates/lmux/tests/cli.rs`: `share_shows_an_agent_transcript_as_chat`, including a check that a view-only share refuses `send`.
- The page (section 5), with incremental chat rendering, a "Latest" button when scrolled up, a folded key row in Chat view, and reconnect that reopens the tab.
- An ignored test reads the real transcripts on this machine and prints only counts: `cargo test -p lmux real_transcripts -- --ignored --nocapture`.

Test note: the daemon clears a `claude:` tag when a shell is the foreground command (`observe_live` in `crates/lmux-core/src/daemon.rs`). A fake transcript in a test therefore stays a chat tab only while a non-shell process runs in the pane.

## 7. Status

Done on 2026-10-11:

- `cargo test -p lmux-core -p lmux` passes.
- A Playwright harness made 40 browser checks against a scratch daemon with fake Claude, Codex and Grok agents, and all passed. It covers phone (390 and 320 px) and desktop (1280 px) widths, both themes, view-only and typing shares, an expired link, back navigation, and no CSP errors. The harness lived in `/tmp/lmux-e2e` and is not in the repo.
- A real `trycloudflare.com` link at phone size: login, live screen, and typing round trip all worked.

## 8. How to test

Build and run the unit and integration tests:

```sh
cd /Users/vas/Documents/lmux
cargo build -p lmux                       # -> target/debug/lmux
cargo test -p lmux-core
cargo test -p lmux                        # includes share::server unit tests
cargo test -p lmux --test cli share_serves_sessions_with_the_security_rules
```

The integration test starts its own scratch daemon and tmux server, and needs `tmux`. It must keep passing.

**Daemon caveat:** the live daemon on this machine is the installed build (`~/.local/bin/lmux`, socket `~/.local/state/lmux/lmux.sock`), and it runs the user's real sessions, including this one. It does not have `term.processes` until it is restarted with a new build. Do not restart it without asking the user. To test option 1 end to end, use a scratch daemon instead:

```sh
D=/tmp/lmuxui; mkdir -p $D
target/debug/lmux --socket $D/s server --state-dir $D/state --tmux-socket lmux-ui-test --ws-addr 127.0.0.1:0 --no-browser &
target/debug/lmux --socket $D/s new-workspace --name demo --command /bin/zsh
target/debug/lmux --socket $D/s new-workspace --name agent --cwd /Users/vas/Documents/lmux --command claude
```

When you finish, clean up with `tmux -L lmux-ui-test kill-server`, kill the daemon, and `rm -rf /tmp/lmuxui`.

**Local page loop (no tunnel):**

```sh
target/debug/lmux --socket $D/s share --no-tunnel --allow-typing --port 8787
```

It prints `http://127.0.0.1:8787/#<code>`. The cookie is `Secure`, so use a Chromium browser, which treats localhost as secure. Each code works once and only for 5 minutes, so restart `share` to get a fresh link. Page edits need `cargo build -p lmux` because the files are embedded.

**Real tunnel check at phone size:**

```sh
target/debug/lmux --socket $D/s share --allow-typing
```

It starts `cloudflared` (`/Users/vas/.local/bin/cloudflared`) and prints `https://<x>.trycloudflare.com/#<code>`. You can use `--json` to get the URL as one JSON line; stdin must stay open, or the share ends.

Open the link once in a phone-sized browser:

- Brave: `/Applications/Brave Browser.app` with `--window-size=390,844`, or device emulation in DevTools.
- Playwright's Chromium is cached at `~/Library/Caches/ms-playwright/chromium-1243`. The `playwright` Node module is not installed globally, so use `npx -y playwright@latest` or a temporary `npm i` in `/tmp`.

Checks to run:

- `document.documentElement.scrollWidth <= innerWidth` holds at 390 px and 320 px.
- The fragment is gone after load (`location.hash === ""`).
- `localStorage` holds nothing besides the theme.
- Opening a tab shows live screen updates.
- Typing reaches the terminal. Confirm with `target/debug/lmux --socket $D/s read-screen --surface surface:N`.
- With a view-only share (no `--allow-typing`), there is no composer, and a forced `send` gets an error toast.
- Reusing the same link shows the expired message.
- Back navigation works.
- Both themes work.
- The page also works at desktop width (1280 px).

Stop the share with Ctrl-C, which kills `cloudflared`.

## 9. Open questions

- Grok session matching is still a heuristic. When one `grok` process has two main sessions open, the most recently written one wins. A Grok hook that records its session ID, like Claude's SessionStart hook, would make it exact. That is daemon and hook work.
- The Claude fallback with no tag shows the newest transcript for the pane's directory. It can show another Claude's chat in the same directory until the SessionStart hook tags the pane.
- Remote-host sessions are not in the share tree yet.
- The native iPhone app in `mobile/` is being moved from the Python bridge to this share protocol, so scanning the QR opens the session in the app. See `mobile/README.md`.
