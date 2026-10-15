# Grok delivery, steering, and session configuration

`lmux send` acknowledges terminal input acceptance. Grok can hold that input in its follow-up queue during a turn or compaction.
Use `harness-status` to query the available per-surface evidence.

These commands require a daemon built with this change. Deploying or restarting a live daemon remains a separate operation.

## Query delivery

```sh
lmux --json send --surface 1455 --paste --enter 'Run the focused tests'
lmux --json harness-status --surface 1455
lmux harness-status --workspace workspace:2
lmux --json harness-status --all --cached
```

Human `send`, `paste`, and `send-key` output stays `OK`. Their terminal input behavior stays unchanged.
JSON send results add `message_id` and a transport-scoped `delivery` object.
An empty Enter-only send has no message ID.

`harness-status` returns `surfaces`, with these fields:

| Field | Meaning |
| --- | --- |
| `lifecycle` | `compacting`, `running`, `waiting`, `idle`, or `unknown`, with evidence and age. |
| `queue.count` | Observed visible count, or `null` when unavailable or stale. |
| `queue.lower_bound` | True when the count covers identifiable visible rows. A rendered explicit total can clear this flag. |
| `messages[].id` | Stable lmux receipt ID for this daemon's retained record. |
| `terminal_accepted` | tmux accepted the input write. |
| `state` | `queued`, `submitted`, `in_progress`, or `unknown`, according to available evidence. |
| `harness_session_id`, `harness_prompt_id` | Grok IDs associated through a unique content fingerprint, when observable. |
| `submitted_at_ms` | The prompt submission decision gate ran. Another hook can still reject the prompt. |
| `consumed_at_ms` | Historical time when a status-line payload reported the associated prompt as active. Model ingestion remains unconfirmed. |
| `consumption_evidence` | `active_prompt_observed` when that historical evidence exists; otherwise `unknown`. |
| `model_receipt` | Currently `unknown`. The integration has no inference-level input acknowledgement. |
| `actual_steering` | Currently `null`. The supported integration cannot establish whether a message changed the agent's work. |
| `completed_at_ms` | Currently `null`. A Stop hook is a decision gate and can continue the turn. |
| `configuration` | Requested and observed model/effort, plus available options and control support. |

Every observation carries a `source`, `confidence`, and daemon receipt time in Unix milliseconds.
Prompt-to-send content correlation remains heuristic, even when Grok supplies an authoritative active prompt ID.
A manually entered identical prompt can be indistinguishable from terminal-injected input.

Observations expire after 15 seconds. Stale current fields become `unknown` or `null`.
`last_observed_state`, `last_observed_count`, evidence, and historical timestamps remain available.
A failed screen capture invalidates current screen claims.
Session end and a non-Grok foreground process invalidate current configuration values and options.
Delayed reports for known older prompts cannot reactivate them or replace the newer configuration.
`--cached` performs no screen capture. Refreshing a screen observes the rendered UI; that UI can lag the harness.

### Multiple queued messages

Send each message in its own call and retain each returned `message_id`.
Query `harness-status` after each relevant transition.
For three visible rows, a count of three followed by two means the visible queue changed.
Inspect each message's submission and active-turn evidence to determine which input Grok processed.
A missing queue row becomes `unknown` until independent evidence identifies receipt.
Deletion, queue editing, prompt combining, clipping, and consumption can all remove rows.

The screen fallback recognizes numbered rows above Grok's `Enter to send now` footer.
It reads the bottom 24 lines of the current screen and requires a Grok foreground command (`grok` or `grok-*`).
It uses an explicit `N queued` total when visible. Otherwise it reports a visible-row lower bound.
It never treats an invisible queue as empty. Ordinary shell output does not activate this parser.
This is a conservative, version-sensitive fallback. Unrecognized layouts produce unknown values.

### Enter and submission safety

Grok documents Enter on an empty focused composer as **send the top queued follow-up now**.
That operation can cancel the current turn. A selected queue row or permission card can also change what Enter does.
`queue.send_now_available` reports the visible hint; `queue.enter_required` stays unknown because queued prompts can also run automatically.

lmux performs no automatic flush, extra Enter, Escape, cancellation, or retry.
A targeted, non-interruptive flush remains unsupported by the documented Grok integration.
Use `send-key --surface S enter` only after deciding to perform that terminal gesture and checking Grok's focus.
Re-query status afterward. Terminal acceptance alone leaves agent receipt unknown.

## Opt-in supported Grok observations

Print the proposed configuration:

```sh
lmux hooks grok print-config
```

The command edits no files. After reviewing it, install its JSON as a new file under `~/.grok/hooks/`.
Preserve existing hook files and settings. Review the hooks in Grok's `/hooks` interface and reload them deliberately.
Hook commands must resolve the matching `lmux` binary and daemon socket on that host.

The adapter observes native Grok events: SessionStart/End, UserPromptSubmit, Pre/PostCompact,
tool activity, Stop/StopFailure/StopCancelled, and Notification.
It reads Grok's camelCase payload and ignores child events carrying `subagentType`.
It prints no hook decisions and uses a short local RPC timeout.
Session and prompt IDs keep older turn-end reports from settling a newer turn.

Grok's supported status-line payload supplies the active `prompt_id`, model, and effort.
To forward it, add this call to an existing status-line script that already receives the JSON on stdin:

```sh
input=$(cat)
printf '%s\n' "$input" | lmux hooks grok status-line >/dev/null 2>/dev/null
# Continue the existing status-line rendering with the same input.
```

Retain the existing renderer and configuration. The forwarding command emits no visible status line itself.
This repository does not install or replace your status-line configuration.
A `refresh_interval` invocation repeats Grok's previous payload and does not refresh lmux evidence.
Use state-triggered payloads for active-turn and configuration observations.

The hook adapter passes prompt text transiently for fingerprint matching.
It discards tool inputs, tool outputs, answers, and notification bodies before the daemon call.
The daemon retains keyed fingerprints, IDs, states, and timestamps in memory.
It retains at most 128 messages per live surface and reports evictions in `dropped_messages`.
It persists no harness records, fingerprints, prompt bodies, or control credentials.
Receipts expire on daemon restart. Existing tmux history and Grok's own files retain their existing behavior.

## Query and request model/effort

```sh
lmux --json harness-config --surface 1455
lmux --json harness-config --surface 1455 --options
lmux --json harness-config --surface 1455 --model gpt-6.1-sol --effort high
```

The first two commands query observations. `applied.model` and `applied.effort` come from state-triggered Grok status-line payloads or ACP observations.
They describe the reported session configuration. The configuration of an inference request already in flight remains unknown.
Absent or stale evidence returns null current values.

**Live setting is currently unsupported for an arbitrary existing Grok TUI.**
The third command records a bounded request and returns `requested.state: "unsupported"` with exit status 1.
It preserves the observed applied values. It sends no terminal input and changes no global default.
A syntactically invalid ID or a value outside a fresh advertised menu returns `requested.state: "failed"`.
JSON stdout remains a structured result on these failures; stderr explains the reason.
`pending` is false and `applied_acknowledgement` is null because lmux dispatched no control operation.
This implementation creates no deferred switch that could surprise a later turn.

Grok 1.0.52 documents `session/set_config_option` on the Agent Client Protocol (ACP) connection that owns the session.
Its response includes updated `configOptions`, and `config_option_update` notifications mirror them.
The installed CLI exposes no documented external attachment for an arbitrary running terminal session.
`session/load` can load a session and is unsuitable as a passive query.
Starting another agent or resuming the same session would violate the existing-session safety requirement.

### Enumerate choices from an existing ACP integration

An already owning ACP client can forward its supported `configOptions` response or notification through `harness.observe`:

```sh
lmux rpc harness.observe '{"surface":"surface:1455","event":"config_options","session_id":"SESSION_ID","config_options":[{"id":"model","type":"select","currentValue":"gpt-6.1-sol","options":[{"value":"gpt-6.1-sol","name":"GPT 6.1 Sol"}]},{"id":"reasoning_effort","type":"select","currentValue":"high","options":[{"value":"low"},{"value":"high"}]}]}'
```

Forward actual payload data in an adapter; the example illustrates the schema.
The daemon retains option IDs and current values, including grouped ACP select options.
It labels their source `grok_acp`. It does not open, load, or control a session.
Local RPC callers remain trusted to provide genuine observations, as with the existing Claude hooks.

Fresh advertised choices validate exact IDs. Unknown menus remain null.
A reported model change invalidates previously advertised menus.
The current model's effort menu validates requests that retain that model.
A request for another model leaves effort-choice validation unavailable until that model's menu is observed.
Effort menus are model-specific, so lmux supplies no fabricated global model or effort list.
A future supported control adapter must validate against the owning session's choices and verify returned current values.
It must handle errors, pending acknowledgements, model-dependent effort changes, and busy-session guarantees before enabling writes.

## JSON RPC

The same interface is available through the local daemon RPC:

```sh
lmux rpc harness.status '{"surface":"surface:1455","cached":true}'
lmux rpc harness.config '{"surface":"surface:1455"}'
lmux rpc harness.config '{"surface":"surface:1455","model":"gpt-6.1-sol","effort":"high"}'
```

Raw RPC returns the configuration result even when a requested change is unsupported.
Check `configuration.requested.state` and `applied_acknowledgement` in RPC clients.
The `harness-config` CLI additionally returns a nonzero exit status for failed or unsupported requests.

## Verification and limits

Regression tests use synthetic Grok payloads and isolated tmux daemons.
They cover compaction, three queued inputs, one-at-a-time active-turn association, disappearing rows,
stale observations, delayed turn-end reports, child isolation, ordinary shells, privacy, and unsupported configuration writes.

The integration preserves the existing workspace-wide `agent` field and native app behavior.
Use per-surface `harness-status` for detailed automation.
No browser, phone, or native app UI changed.
The fallback parser has synthetic rendering fixtures; it has no live Grok queue verification in this change.
Undocumented binary queue extensions and host-server internals remain outside the supported adapter.
