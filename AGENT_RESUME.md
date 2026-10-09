# Agent resume after reboot (ported from herdr)

Goal: after the server reboots, or the `tmux -L lmux` server dies for any other reason, every pane that was running Claude Code comes back with the same conversation, in the same directory and the same place in the layout. This covers the PLAN.md milestone 5 item "offers `claude --resume` in agent panes".

Reference implementation: [herdrdev/herdr](https://github.com/herdrdev/herdr) at `2563803` (2026-10-08).

## How herdr does it

herdr persists sessions in two layers.

### 1. Live persistence: processes never stop

A background server owns the PTYs, and the client is only a viewer. Closing the client or losing SSH has no effect on Claude. For a server upgrade, herdr passes the PTY master file descriptors to the new server process (`src/handoff_runtime.rs`).

**lmux already has this, and it is stronger.** `tmux -L lmux` owns the PTYs, and `deploy/lmuxd.service` uses `KillMode=process`, so Claude keeps running even if lmuxd crashes or restarts. Nothing in this document changes this layer.

### 2. Resume after a restart: start the agent again with its own resume flag

The README says it directly: "after a server or machine restart, herdr restores the saved layout and can resume supported agent sessions; the original processes do not survive." herdr never saves the process. It saves the agent's **session ID** and runs `claude --resume <id>` again.

#### Capturing the session ID

- herdr installs a single Claude Code hook: `SessionStart` with matcher `^(startup|resume|clear|compact|fork)$` (`src/integration/claude_settings.rs:19`).
  - Including `clear`, `compact` and `fork` matters. `/clear` and fork create a new session ID, and the hook fires again, so the saved ID always matches the conversation currently on screen.
  - Older herdr versions also installed `UserPromptSubmit`, `Stop` and `SessionEnd` hooks. `HOOK_REMOVALS` now deletes them, because state and lifetime come from watching the process instead.
- The hook script (`src/integration/assets/claude/herdr-agent-state.sh`):
  - exits quietly unless `HERDR_ENV=1`, `HERDR_SOCKET_PATH` and `HERDR_PANE_ID` are set, so it is a no-op outside herdr;
  - reads the hook's stdin JSON: `session_id`, `transcript_path`, `source`, `hook_event_name`;
  - **skips subagents**, recognised by `agent_id` being present in the input;
  - sends `{"method":"pane.report_agent_session","params":{pane_id, source:"herdr:claude", agent:"claude", seq, agent_session_id, agent_session_path, session_start_source}}` over the unix socket with a 0.5 s timeout, and ignores every error so it can never block Claude.

#### Saving

- Each pane in the snapshot carries `agent_session: {source, agent, kind: id|path, value}` (`src/persist/snapshot.rs:109`).
- Agents that aren't built in can report a full `agent_resume.argv` instead. Before it is accepted, `validate_resume_argv` (`src/agent_resume.rs:58`) checks that the command is a bare name rather than a path, contains no control characters or apostrophes, and stays within 64 args and 8 KiB.
- The writer (`src/persist/writer.rs`) saves atomically, keeps a recovery copy of a file it failed to load, and keeps a rolling snapshot history every 15 minutes.

#### Forgetting

When herdr sees that the agent process is no longer running in the pane, it clears `persisted_agent_session` (`src/terminal/state.rs`). A pane where you quit Claude therefore does not bring Claude back after a reboot.

#### Restoring

Handled by `src/persist/restore.rs:813` (`pane_restore_startup`) and `src/app/agent_resume.rs`:

1. Turn the saved session into a plan: `["claude", "--resume", <id>]` (`src/agent_resume.rs:204`).
2. **Remove duplicates** using a `source\0agent\0kind\0value` key. If two panes saved the same session, only the first resumes it and the other gets a plain shell.
3. **Don't replay saved scrollback** into a pane that will resume. Claude redraws its own history, and replaying the old screen would show it twice.
4. Wait until the pane size (and terminal theme) is known, so Claude starts at the right size.
5. If the saved directory no longer exists, show "Saved directory is unavailable" instead of starting anything.
6. Start a **normal shell** in the saved directory, then **type** the command into it: `claude --resume <id>\r`. The user's rc files, PATH and environment apply, and when Claude exits the user is back at a shell prompt instead of in a dead pane.
7. **Stagger** the starts (`startup_per_agent_delay`) so ten agents don't all start at the same moment.

## What lmux does today

| | Where | Behaviour |
|---|---|---|
| Hook | `crates/lmux/src/main.rs:1006` | `SessionStart` only sets the workspace to idle. `session_id` is thrown away. |
| Hook config | `crates/lmux/src/main.rs:1094` (`claude_settings_snippet`) | `SessionStart` has no matcher. |
| Tree | `crates/lmux-proto/src/lib.rs:146` | `SurfaceKind::Terminal` has `tmux_pane`, `cols` and `rows`. It has no directory and no agent session. |
| Reconcile | `crates/lmux-core/src/daemon.rs:530` | A terminal whose tmux pane is missing is **removed**. After a reboot, every terminal surface vanishes and the layout falls apart. |
| Env | `crates/lmux-core/src/rpc.rs:185` | Terminals already get `LMUX_SOCKET`, `LMUX_WORKSPACE_ID` and `LMUX_SURFACE_ID`, which is all the hook needs. |

Panes the user closes are already removed by `handle_window_closed` (`daemon.rs:619`). When `reconcile` finds a terminal whose tmux pane is missing, the cause is that the tmux server itself went away, so in that case the right thing is to respawn the terminal, not remove it.

## Implementation in lmux

### 1. Protocol: `crates/lmux-proto`

`src/lib.rs`, add to `SurfaceKind::Terminal`:

```rust
Terminal {
    tmux_pane: Option<String>,
    cols: u16,
    rows: u16,
    /// Last known working directory of the pane (tmux `pane_current_path`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    /// Agent session to resume if the pane has to be respawned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent_session: Option<AgentSession>,
},

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSession {
    pub agent: String,      // "claude"
    pub id: String,         // Claude session_id (UUID)
}
```

`#[serde(default)]` lets existing `state.json` files load unchanged.

`src/methods.rs`:

```rust
pub const AGENT_SESSION: &str = "agent.session"; // {surface, agent, id, cwd?, source?} (called by the SessionStart hook)
```

### 2. Hook: `crates/lmux/src/main.rs`

In `ClaudeHook::SessionStart`, keep the existing `AGENT_STATE` call and add:

```rust
// Subagents report their own SessionStart; only the top-level session resumes.
let is_subagent = payload.get("agent_id").is_some_and(|v| !v.is_null());
if let (Ok(surface), Some(id), false) = (
    std::env::var("LMUX_SURFACE_ID"),
    payload["session_id"].as_str().filter(|s| !s.is_empty()),
    is_subagent,
) {
    let _ = call(socket, methods::AGENT_SESSION, json!({
        "surface": surface,
        "agent": "claude",
        "id": id,
        "cwd": payload["cwd"],
        "source": payload["source"],
    })).await;
}
```

In `claude_settings_snippet`, add the matcher so `/clear`, `/compact` and fork report the new ID:

```json
"SessionStart": [{"matcher": "startup|resume|clear|compact|fork",
                  "hooks": [{"type": "command", "command": "lmux hooks claude session-start"}]}]
```

Like herdr's hook, this must stay quiet and never fail, because Claude waits for SessionStart hooks. Keep using `let _ =`, and give `call` a short connect timeout (500 ms).

### 3. Daemon: `crates/lmux-core`

**RPC (`rpc.rs`).** Handle `agent.session`:

- reject an `id` that is empty, longer than 512 bytes, or contains control characters (herdr's `valid_session_id`);
- set `agent_session` and `cwd` on the surface, and mark state dirty so the debounced persist task writes `state.json`;
- mirror the value into tmux with `set_window_option(window, "@lmux_agent_session", "claude:<id>")`, so the session also survives a lost or stale `state.json` and can be picked up by the "adopt tagged windows" step in `reconcile`.

**tmux (`tmux.rs`).** Extend the `list_windows` format with `#{pane_current_path}`, `#{pane_current_command}` and `#{@lmux_agent_session}`, and add the matching `WinInfo` fields.

**Forgetting.** On every `reconcile`, and on a slow timer (about 30 s) while the daemon runs, for each live terminal:

- set `cwd = pane_current_path`;
- if `agent_session` is set and `pane_current_command` is a shell (`bash`, `zsh`, `fish`, `sh`), clear `agent_session` and the tmux option. Claude has exited, so don't bring it back. Claude's own process shows up as `claude` or `node`, so treat any other command as "still running".

This replaces herdr's process detection. `SessionEnd` is not used, because it may also fire when tmux or the machine shuts down, which is exactly the case where we want to resume. herdr also no longer installs a `SessionEnd` hook.

**Reconcile (`daemon.rs:530`).** Replace the `_ => state.remove_surface(&surf)` branch for terminals. Collect them into a `respawn` list, and after releasing the state lock:

```rust
let mut seen = HashSet::new();
for (surf, ws, title, cols, rows, cwd, session) in respawn {
    let cwd = cwd.filter(|d| Path::new(d).is_dir())
        .unwrap_or_else(|| workspace_cwd_or_home(&ws));
    let (window, pane) = self.tmux.new_terminal(
        &cwd, None, &terminal_env(&ws, &surf), cols, rows, &title, &surf, &ws,
    ).await?;
    state.set_term(&surf, &window, &pane);

    // One live process per Claude session, as herdr's dedupe key does.
    if let Some(s) = session.filter(|s| seen.insert(s.id.clone())) {
        self.tmux.send_bytes(&pane, format!("claude --resume {}\r", s.id).as_bytes()).await?;
        self.tmux.set_window_option(&window, "@lmux_agent_session", &format!("claude:{}", s.id)).await?;
        tokio::time::sleep(Duration::from_millis(750)).await; // stagger
    } else {
        clear_agent_session(&surf); // duplicate or none: plain shell
    }
}
```

Notes:

- Start the shell and **type** the command, as herdr does, instead of passing `claude --resume` as the window command. If Claude exits, the pane stays open as a shell, and the user's PATH from their rc files applies.
- Session IDs are UUIDs. Validate with `[0-9A-Za-z-]+` before typing, so nothing ever needs quoting.
- If the saved directory is gone, fall back to the workspace directory and send an `lmux notify` saying the session could not resume, rather than running `claude --resume` somewhere it will fail.
- Run respawns in a background task so `reconcile` doesn't block the app websocket while ten agents start one after another.
- Respawned panes have no old screen to show. Don't try to restore scrollback for them. Claude redraws its own history.

### 4. App (optional)

Show a small "resumed" badge or toast on a surface whose agent was resumed after a restart. The daemon can send this as an ordinary notification, so no protocol change is needed.

## Testing

Unit tests:

- `state.json` without `cwd` or `agent_session` still loads (serde defaults).
- `agent.session` rejects an empty ID, a too-long ID, and an ID with control characters.
- With two surfaces holding the same session, reconcile resumes only one of them.
- With the shell as the foreground command, `agent_session` is cleared.

End-to-end on `my-server`:

1. Start Claude in two panes and one plain shell pane. Send a message in each Claude pane.
2. In one Claude pane, run `/clear` and send another message. Confirm `state.json` holds the new ID.
3. In a third pane, start Claude and then quit it.
4. `tmux -L lmux kill-server` (or reboot the server).
5. Expected: lmuxd's control client respawns and runs `reconcile`. The layout is unchanged. Both Claude panes show their conversations (the `/clear`ed one shows the post-clear conversation). The plain shell pane and the quit-Claude pane come back as plain shells in their old directories.

## Out of scope

- Other agents (Codex `codex resume <id>`, etc.). The `agent` field leaves room for herdr's per-agent argv table in `src/agent_resume.rs:198` when needed.
- Restoring scrollback for plain shell panes after a reboot. That would need periodic `capture-pane` snapshots on disk. herdr does this, but it is a separate feature.
- Browser surfaces. They already survive through the browser backend and are untouched here.
