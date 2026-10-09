#!/usr/bin/env bash
# Install or update lmux on a remote Linux host, so the app can add it as a
# host ("Add Remote Host…" in the sidebar menu, or hosts.json).
#
#   deploy/install-remote.sh <ssh-target> [git-ref]
#
# Ships the source of <git-ref> (default: HEAD) over SSH, builds a release
# binary there, installs it to ~/.local/bin/lmux, runs it as the systemd user
# service `lmuxd` (websocket on 127.0.0.1:7878, reached through the app's SSH
# tunnel), and adds the Claude Code hooks that let panes resume their Claude
# session after a reboot. Re-running it updates in place; tmux sessions survive
# the daemon restart.
#
# Needs on the host: tmux, a Rust toolchain (rustup), and systemd user services
# with lingering (`loginctl enable-linger $USER`) so the daemon outlives logout.
set -euo pipefail

target=${1:?usage: install-remote.sh <ssh-target> [git-ref]}
ref=${2:-HEAD}
repo=$(git -C "$(dirname "$0")/.." rev-parse --show-toplevel)

echo "==> checking $target"
ssh -o BatchMode=yes "$target" 'set -e
  command -v tmux >/dev/null || { echo "tmux is missing on this host (apt install tmux)"; exit 1; }
  [ -x ~/.cargo/bin/cargo ] || command -v cargo >/dev/null || {
    echo "Rust is missing on this host: curl https://sh.rustup.rs -sSf | sh -s -- -y"; exit 1; }
  if [ "$(loginctl show-user "$USER" -p Linger --value 2>/dev/null)" != yes ]; then
    echo "warning: lingering is off, so lmuxd stops at logout (loginctl enable-linger $USER)"
  fi'

echo "==> shipping $(git -C "$repo" rev-parse --short "$ref")"
git -C "$repo" archive --format=tar "$ref" | ssh -o BatchMode=yes "$target" \
  'rm -rf ~/lmux.new && mkdir -p ~/lmux.new && tar -x -C ~/lmux.new'

echo "==> building (first build takes a minute or two)"
ssh -o BatchMode=yes "$target" 'set -e
  cargo=$(command -v cargo || echo ~/.cargo/bin/cargo)
  cd ~/lmux.new && "$cargo" build --release -p lmux 2>&1 | tail -1
  rm -rf ~/lmux.old; [ -d ~/lmux ] && mv ~/lmux ~/lmux.old; mv ~/lmux.new ~/lmux
  mkdir -p ~/.local/bin && install -m755 ~/lmux/target/release/lmux ~/.local/bin/lmux'

echo "==> starting lmuxd"
ssh -o BatchMode=yes "$target" 'set -e
  install -Dm644 ~/lmux/deploy/lmuxd.service ~/.config/systemd/user/lmuxd.service
  systemctl --user daemon-reload
  systemctl --user enable lmuxd >/dev/null 2>&1
  systemctl --user restart lmuxd
  sleep 1
  systemctl --user is-active lmuxd'

echo "==> Claude Code hooks"
ssh -o BatchMode=yes "$target" 'python3 - <<"PY"
import json, os, shutil, time
p = os.path.expanduser("~/.claude/settings.json")
s = json.load(open(p)) if os.path.exists(p) else {}
if os.path.exists(p):
    shutil.copy(p, p + ".bak-lmux-" + time.strftime("%Y%m%d%H%M%S"))
hooks = s.setdefault("hooks", {})
def cmd(sub):
    # Only inside an lmux pane, and never fail or block Claude.
    return f"[ -n \"$LMUX_SURFACE_ID\" ] && \"$HOME/.local/bin/lmux\" hooks claude {sub} 2>/dev/null; true #lmux-hook"
want = {
    "SessionStart": ("startup|resume|clear|compact|fork", "session-start"),
    "UserPromptSubmit": (None, "prompt-submit"),
    "Stop": (None, "stop"),
    "Notification": (None, "notification"),
}
for event, (matcher, sub) in want.items():
    groups = hooks.setdefault(event, [])
    groups[:] = [g for g in groups if not any("#lmux-hook" in h.get("command", "") for h in g.get("hooks", []))]
    g = {"hooks": [{"type": "command", "command": cmd(sub), "timeout": 5}]}
    if matcher:
        g["matcher"] = matcher
    groups.append(g)
os.makedirs(os.path.dirname(p), exist_ok=True)
json.dump(s, open(p, "w"), indent=2)
print("hooks installed")
PY'

echo "==> done. In the app: right click the sidebar > Add Remote Host… > $target"
