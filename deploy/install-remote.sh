#!/usr/bin/env bash
# Install or update lmux on a remote Linux or macOS host, so the app can add
# it as a host ("Add Remote Host…" in the sidebar menu, or hosts.json).
#
#   deploy/install-remote.sh <ssh-target> [ref]
#
# The host pulls <ref> (default: main) from GitHub into
# ~/.local/share/lmux/src and builds it with deploy/update.sh: GitHub is the
# single source, nothing is built here and copied there. It installs
# ~/.local/bin/lmux, runs it as the systemd user
# service `lmuxd` on Linux or the launchd agent `dev.lmux.lmuxd` on macOS
# (websocket on 127.0.0.1:7878, reached through the app's SSH tunnel), and adds the Claude Code hooks that let panes resume their Claude
# session after a reboot, and turns on hourly automatic updates from the same
# ref (deploy/update.sh --install-timer). Re-running it updates in place; tmux
# sessions survive the daemon restart. The ref must contain deploy/update.sh.
# LMUX_SRC_TAR (set by the app) ships a source tarball instead, for hosts that
# cannot reach GitHub; those get no automatic updates.
#
# Needs on the host: tmux, a Rust toolchain (rustup), and on Linux systemd user
# services with lingering (`loginctl enable-linger $USER`) so the daemon
# outlives logout. On macOS a missing tmux is installed with Homebrew.
set -euo pipefail

# SSH runs a non-login shell: on macOS that PATH has neither Homebrew nor cargo.
env_path='export PATH="$HOME/.local/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH";'

target=${1:?usage: install-remote.sh <ssh-target> [ref]}
ref=${2:-main}
repo_url=${LMUX_UPDATE_REPO:-https://github.com/lintware/lmux.git}
case $ref in *[!A-Za-z0-9._/-]*) echo "bad ref $ref" >&2; exit 2 ;; esac
case $repo_url in *[!A-Za-z0-9._/:@-]*) echo "bad repo URL $repo_url" >&2; exit 2 ;; esac
# Where the host's source lives: its own GitHub clone, or (tarball) ~/lmux.
if [ -n "${LMUX_SRC_TAR:-}" ]; then src='~/lmux'; else src='~/.local/share/lmux/src'; fi

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
  ssh -o BatchMode=yes "$target" \
    'rm -rf ~/lmux.new && mkdir -p ~/lmux.new && tar -x -C ~/lmux.new' <"$LMUX_SRC_TAR"
  echo "==> building (first build takes a minute or two)"
  ssh -o BatchMode=yes "$target" "$env_path"'set -e
    cd ~/lmux.new && "$(command -v cargo)" build --release -p lmux 2>&1 | tail -1
    rm -rf ~/lmux.old; [ -d ~/lmux ] && mv ~/lmux ~/lmux.old; mv ~/lmux.new ~/lmux
    mkdir -p ~/.local/bin && install -m755 ~/lmux/target/release/lmux ~/.local/bin/lmux'
else
  echo "==> $target pulls $ref from $repo_url and builds it (the first build takes a few minutes)"
  ssh -o BatchMode=yes "$target" "$env_path"'set -e
    src=~/.local/share/lmux/src
    [ -d "$src/.git" ] || git clone -q '"$repo_url"' "$src"
    git -C "$src" fetch -q origin "+refs/heads/*:refs/remotes/origin/*"
    git -C "$src" -c advice.detachedHead=false checkout -q --force --detach "origin/'"$ref"'" 2>/dev/null \
      || git -C "$src" -c advice.detachedHead=false checkout -q --force --detach '"$ref"'
    [ -f "$src/deploy/update.sh" ] || { echo "'"$ref"' has no deploy/update.sh"; exit 1; }
    LMUX_UPDATE_REPO='"$repo_url"' bash "$src/deploy/update.sh" --ref '"$ref"' --no-restart'
fi

echo "==> helpers"
ssh -o BatchMode=yes "$target" "$env_path"'set -e
  src='"$src"'
  # Browser pages run in WebKit on the host when its helper is installed
  # (else the daemon uses Chromium).
  mkdir -p ~/.local/libexec/lmux
  helper=~/.local/libexec/lmux/lmux-webkit
  if [ "$(uname)" = Darwin ]; then
    rm -f "$helper.new"
    command -v swiftc >/dev/null && swiftc -O -o "$helper.new" "$src/helpers/webkit-mac/main.swift" 2>&1 | tail -3
    if [ -x "$helper.new" ]; then
      mv "$helper.new" "$helper" && echo "WebKit helper built"
    else
      echo "no WebKit helper (swiftc missing or failed: xcode-select --install); browsers use Chromium"
    fi
  elif python3 -c "import gi; gi.require_version(\"Gtk\", \"3.0\"); gi.require_version(\"WebKit2\", \"4.1\")" 2>/dev/null \
      && command -v Xvfb >/dev/null; then
    install -m755 "$src/helpers/webkit-linux/lmux-webkit" "$helper" && echo "WebKit helper installed"
  else
    rm -f "$helper"
    echo "no WebKit helper (sudo apt install python3-gi gir1.2-webkit2-4.1 xvfb); browsers use Chromium"
  fi'

echo "==> starting lmuxd"
ssh -o BatchMode=yes "$target" "$env_path"'set -e
  src='"$src"'
  if [ "$(uname)" = Darwin ]; then
    # A launchd agent: in the GUI session when someone is logged in at the
    # Mac, else in the background user session SSH has.
    plist=~/Library/LaunchAgents/dev.lmux.lmuxd.plist
    mkdir -p ~/Library/LaunchAgents ~/.local/state/lmux
    sed -e "s|@HOME@|$HOME|g" -e "s|@PATH@|$HOME/.local/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin|g" \
      "$src/deploy/lmuxd.plist" > "$plist"
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
    install -m644 "$src/deploy/lmuxd.service" ~/.config/systemd/user/lmuxd.service
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

if [ -z "${LMUX_SRC_TAR:-}" ]; then
  echo "==> automatic updates"
  ssh -o BatchMode=yes "$target" "$env_path"'LMUX_UPDATE_REPO='"$repo_url"' bash ~/.local/share/lmux/src/deploy/update.sh --ref '"$ref"' --install-timer'
fi

echo "==> done. In the app: right click the sidebar > Add Remote Host… > $target"
