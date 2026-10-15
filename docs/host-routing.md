# Host-aware CLI access

Each lmux daemon owns its workspaces, panes, and surfaces. A ref such as `surface:70` belongs to one daemon. Two machines can have the same ref.

`lmux tree` queries the local daemon by default. A host displayed in the app's sidebar is a connection target. That host name does not need to appear as a workspace title.

## Find and query another host

```sh
lmux list-hosts
lmux --json list-hosts
lmux --host lint-hqc-01 tree
lmux --host lint-hqc-01 --json list-workspaces
lmux --json tree --all-hosts
```

On `lint-hqc-03`, select `lint-hqc-01` explicitly to see that machine's sessions. Use the IDs returned by that query:

```sh
lmux --host lint-hqc-01 read-screen --surface surface:70
lmux --host lint-hqc-01 --json harness-status --surface surface:70
```

`surface:70` is an example. Replace it with a ref from the selected host's tree.
The remote CLI and daemon must support the requested RPC method. New host routing requires an updated invoking CLI. It uses the existing remote `lmux rpc` command, so routing itself requires no daemon restart.

## SSH and Tailscale

`--host` runs RPC through SSH to the host's existing local lmux socket. It uses the SSH alias, keys, port, proxy, and destination already configured for that user.

An alias can use a Tailscale address or MagicDNS name:

```sshconfig
Host lint-hqc-01
    HostName YOUR-PEER.TAILNET.ts.net
    User YOUR_REMOTE_USER
```

Use the real peer name and account in your own configuration. Tailscale connectivity, SSH authorization, and tailnet policy must already permit the connection.
This implementation changes no network configuration and opens no daemon TCP listener.

SSH runs in batch mode with strict host-key checking. Establish and verify the host key with an ordinary SSH connection first. lmux supplies no password or host-key prompt. Each RPC uses an independent SSH process, preserving the app's existing SSH control connections.

Cloudflare Tunnel continues to serve phone/browser sharing links. Private CLI routing uses SSH and works over Tailscale. Migrating the sharing link to tailnet-only access is a separate configuration decision.

## Discovery scope

`list-hosts` reads configuration on the invoking machine:

- The derived `local` route.
- App `hosts.json`: `~/Library/Application Support/lmux/hosts.json` on macOS, or `$XDG_CONFIG_HOME/lmux/hosts.json` / `~/.config/lmux/hosts.json` on Linux.
- Literal `Host` aliases in `~/.ssh/config`.

Discovery performs no network scan and writes no configuration. Wildcard/negated aliases and aliases declared only through `Include` files stay outside automatic enumeration. Explicit `--host ALIAS` still uses OpenSSH's full configuration, including `Include` and `Match` rules.

Multiple SSH aliases can reach the same physical daemon. Results retain each route's name. App-only direct WebSocket hosts appear as `unsupported`; select their SSH/Tailscale alias to use the CLI.
Malformed host registries and ambiguous labels produce errors. Exact configured names take precedence over labels.

## Target and socket safety

An explicit remote `--host` clears inherited `LMUX_SOCKET`, `LMUX_STATE_DIR`, `LMUX_WORKSPACE_ID`, and `LMUX_SURFACE_ID` before remote execution. Remote RPC also discards `caller_*` context supplied through the invoking CLI. This prevents an identically numbered local surface from selecting an unrelated remote surface.

Supply explicit remote selectors for actions. Untargeted remote commands use that daemon's normal focused/default target. Some commands have broad defaults; inspect their help before use.

```sh
lmux --host lint-hqc-01 --socket /absolute/remote/lmux.sock tree
lmux --host local tree
```

With `--host`, an explicit `--socket` is a path on the selected machine. The inherited `LMUX_SOCKET` applies to local calls. Local calls retain existing caller-context behavior.
Place `--host` before the command or before send/paste text. Text after `--` remains literal:

```sh
lmux --host lint-hqc-01 send --surface surface:70 --paste -- '--host is part of this message'
```

A failed SSH request never falls back to the local daemon and never retries a mutation. A timeout leaves the remote operation's outcome unknown. Inspect state before retrying a change.
The client transfers JSON through SSH stdin and quotes control arguments independently. Prompt text cannot become a shell command through transport quoting.
For compatibility, the existing remote CLI receives the JSON as one quoted argument. Remote requests have a 64 KiB serialized-JSON limit. Larger requests fail before SSH starts; no input is sent. Local paste retains its existing size behavior.
Normal remote RPC calls have a 20-second deadline. Browser wait calls add their requested wait time to that allowance. Remote `wait-for --text --timeout` bounds the complete polling operation, including SSH time.

`server`, hooks, share plumbing, import, and file-copy commands retain machine-local process/filesystem semantics. They reject a remote `--host`. `cp` has its own destination-host syntax. Named `wait-for` signals remain machine-local; `wait-for --text` supports remote surfaces.

## Aggregate JSON

`tree --all-hosts` groups unmodified daemon refs by route:

```json
{
  "scope": "all_hosts",
  "partial": true,
  "hosts": [
    {"name": "local", "state": "connected", "transport": "local", "tree": {"host": "mac", "workspaces": []}},
    {"name": "lint-hqc-01", "state": "unreachable", "transport": "ssh", "tree": null, "error": "SSH failed"}
  ]
}
```

Treat `(host name, ref)` as the identity. Supply `--host HOST` when reusing a ref.
Single-host remote JSON results include `cli_target` with the selected host and SSH target. The tree's `host` field remains the daemon's reported hostname; an older daemon can report `localhost`.

Aggregate queries allow up to 256 discovered routes, with at most eight concurrent calls and a 20-second deadline per host. Explicit single-host routing continues to work with larger registries. Unreachable or unsupported hosts retain explicit error entries. Successful trees remain available. Partial results return exit status 1 and `partial: true`.
`--all-hosts` requires an unfiltered query. Query a particular host/workspace in a separate call.

## Verification

Regression tests use separate scratch daemons and a private SSH stand-in. They cover colliding refs, caller-context isolation, local behavior, stdin paste, shell quoting, remote sockets, invalid destinations, discovery, partial failures, request-size limits, browser flag placement, and wait deadlines.

A temporary CLI copy on `lint-hqc-03` also verified discovery and read-only access to `lint-hqc-01` through the existing private SSH route. Its local tree and the peer's tree stayed separate. The check preserved installed binaries, daemons, and sessions; temporary copies were removed.
No native app or browser UI changes are required for host routing.
