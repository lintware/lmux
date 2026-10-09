#!/usr/bin/env bash
# Install or update lmux on a remote Linux or macOS host, so the app can add
# it as a host ("Add Remote Host…" in the sidebar menu, or hosts.json).
#
#   deploy/install-remote.sh <ssh-target> [git-ref]
#
# Ships the source of <git-ref> (default: HEAD) over SSH, builds a release
# binary there, installs it to ~/.local/bin/lmux, runs it as the systemd user
# service `lmuxd` on Linux or the launchd agent `dev.lmux.lmuxd` on macOS
# (websocket on 127.0.0.1:7878, reached through the app's SSH tunnel), and adds the Claude Code hooks that let panes resume their Claude
# session after a reboot. Re-running it updates in place; tmux sessions survive
# the daemon restart.
#
# Needs on the host: tmux, a Rust toolchain (rustup), and on Linux systemd user
# services with lingering (`loginctl enable-linger $USER`) so the daemon
# outlives logout. On macOS a missing tmux is installed with Homebrew.
set -euo pipefail

# SSH runs a non-login shell: on macOS that PATH has neither Homebrew nor cargo.
env_path='export PATH="$HOME/.local/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH";'

target=${1:?usage: install-remote.sh <ssh-target> [git-ref]}
ref=${2:-HEAD}
# The app runs this from its bundle with LMUX_SRC_TAR pointing at the source
# it ships; from a checkout, the source comes from git.
if [ -z "${LMUX_SRC_TAR:-}" ]; then
  repo=$(git -C "$(dirname "$0")/.." rev-parse --show-toplevel)
fi

echo "==> checking $target"
ssh -o BatchMode=yes "$target" "$env_path"'set -e
  if [ "$(uname)" = Darwin ]; then
    if ! command -v tmux >/dev/null; then
      command -v brew >/dev/null || { echo "tmux is missing on this Mac, and so is Homebrew (https://brew.sh)"; exit 1; }
      echo "installing tmux with Homebrew"
      brew install tmux 2>&1 | tail -1
    fi
  else
    command -v tmux >/dev/null || { echo "tmux is missing on this host (apt install tmux)"; exit 1; }
    if [ "$(loginctl show-user "$USER" -p Linger --value 2>/dev/null)" != yes ]; then
      echo "warning: lingering is off, so lmuxd stops at logout (loginctl enable-linger $USER)"
    fi
  fi
  command -v cargo >/dev/null || {
    echo "Rust is missing on this host: curl https://sh.rustup.rs -sSf | sh -s -- -y"; exit 1; }'

if [ -n "${LMUX_SRC_TAR:-}" ]; then
  echo "==> shipping $(basename "$LMUX_SRC_TAR")"
  ship() { cat "$LMUX_SRC_TAR"; }
else
  echo "==> shipping $(git -C "$repo" rev-parse --short "$ref")"
  ship() { git -C "$repo" archive --format=tar "$ref"; }
fi
ship | ssh -o BatchMode=yes "$target" \
  'rm -rf ~/lmux.new && mkdir -p ~/lmux.new && tar -x -C ~/lmux.new'

echo "==> building (first build takes a minute or two)"
ssh -o BatchMode=yes "$target" "$env_path"'set -e
  cargo=$(command -v cargo)
  cd ~/lmux.new && "$cargo" build --release -p lmux 2>&1 | tail -1
  rm -rf ~/lmux.old; [ -d ~/lmux ] && mv ~/lmux ~/lmux.old; mv ~/lmux.new ~/lmux
  mkdir -p ~/.local/bin && install -m755 ~/lmux/target/release/lmux ~/.local/bin/lmux
  # Browser pages run in WebKit on the host when its helper is installed
  # (else the daemon uses Chromium).
  mkdir -p ~/.local/libexec/lmux
  helper=~/.local/libexec/lmux/lmux-webkit
  if [ "$(uname)" = Darwin ]; then
    rm -f "$helper.new"
    command -v swiftc >/dev/null && swiftc -O -o "$helper.new" ~/lmux/helpers/webkit-mac/main.swift 2>&1 | tail -3
    if [ -x "$helper.new" ]; then
      mv "$helper.new" "$helper" && echo "WebKit helper built"
    else
      echo "no WebKit helper (swiftc missing or failed: xcode-select --install); browsers use Chromium"
    fi
  fi'

echo "==> starting lmuxd"
ssh -o BatchMode=yes "$target" "$env_path"'set -e
  if [ "$(uname)" = Darwin ]; then
    # A launchd agent: in the GUI session when someone is logged in at the
    # Mac, else in the background user session SSH has.
    plist=~/Library/LaunchAgents/dev.lmux.lmuxd.plist
    mkdir -p ~/Library/LaunchAgents ~/.local/state/lmux
    sed -e "s|@HOME@|$HOME|g" -e "s|@PATH@|$HOME/.local/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin|g" \
      ~/lmux/deploy/lmuxd.plist > "$plist"
    uid=$(id -u)
    domain=gui/$uid
    launchctl print "$domain" >/dev/null 2>&1 || domain=user/$uid
    launchctl bootout "$domain/dev.lmux.lmuxd" 2>/dev/null || true
    # bootout returns before the old daemon has let go of its port.
    for i in $(seq 1 50); do lsof -nP -iTCP:7878 -sTCP:LISTEN >/dev/null 2>&1 || break; sleep 0.2; done
    launchctl bootstrap "$domain" "$plist"
    for i in $(seq 1 50); do lsof -nP -iTCP:7878 -sTCP:LISTEN >/dev/null 2>&1 && break; sleep 0.2; done
    lsof -nP -iTCP:7878 -sTCP:LISTEN >/dev/null 2>&1 && echo "active ($domain)" || {
      echo "lmuxd did not start; see ~/.local/state/lmux/daemon.log"; tail -5 ~/.local/state/lmux/daemon.log; exit 1; }
  else
    mkdir -p ~/.config/systemd/user
    install -m644 ~/lmux/deploy/lmuxd.service ~/.config/systemd/user/lmuxd.service
    systemctl --user daemon-reload
    systemctl --user enable lmuxd >/dev/null 2>&1
    systemctl --user restart lmuxd
    sleep 1
    systemctl --user is-active lmuxd
  fi'

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
