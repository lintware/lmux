#!/usr/bin/env bash
# lmux updater. GitHub is the single source: every host pulls the configured
# ref into its own clone, builds it, installs it and restarts its daemon.
# Nothing is copied from one server to another.
#
#   update.sh               update now (if the ref moved), restart, verify
#   update.sh --check       say whether an update is waiting (exit 10 if so)
#   update.sh --status      what this host runs and tracks
#   update.sh --auto        the timer's run: quiet when current, skipped while held
#   update.sh --install-timer | --remove-timer
#   options: --ref REF  --force (rebuild even when current)  --no-restart
#
# A daemon restart leaves the tmux server and every pane running; clients
# reconnect. Before restarting, the updater records each terminal's tmux pane
# and process; afterwards the new daemon must answer with the new build and
# every one of them must still be there. Otherwise it puts the previous
# binary back, restarts again and holds automatic updates
# (~/.local/state/lmux/update.held) until someone runs it by hand.
#
# Settings, in ~/.config/lmux/update.env (all optional):
#   LMUX_UPDATE_REPO=https://github.com/lintware/lmux.git
#   LMUX_UPDATE_REF=main       branch, tag or commit to follow
#   LMUX_UPDATE_APP=auto       macOS: rebuild lmux.app when it is installed
#                              (auto), always (1) or never (0)
#   LMUX_UPDATE_GATEWAY=auto   restart a running lmux-gateway phone service
#                              (auto) or leave it on its old build (never)
#
# Bootstrap a host (once; it then updates itself):
#   git clone https://github.com/lintware/lmux.git ~/.local/share/lmux/src
#   ~/.local/share/lmux/src/deploy/update.sh --ref main --install-timer
set -euo pipefail

# The checkout below rewrites this file; run from a private copy.
if [ -z "${LMUX_UPDATE_COPY:-}" ]; then
  copy=$(mktemp "${TMPDIR:-/tmp}/lmux-update.XXXXXX")
  cp "$0" "$copy"
  LMUX_UPDATE_COPY=$copy exec bash "$copy" "$@"
fi
trap 'rm -f "$LMUX_UPDATE_COPY"' EXIT

# Run from inside an lmux pane, the caller's LMUX_SOCKET could name some
# other daemon; this script only ever talks to this host's own socket.
unset LMUX_SOCKET LMUX_WORKSPACE_ID LMUX_SURFACE_ID
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin:$PATH"
os=$(uname -s)
conf=${XDG_CONFIG_HOME:-$HOME/.config}/lmux/update.env
# shellcheck disable=SC1090
[ -f "$conf" ] && . "$conf"
repo=${LMUX_UPDATE_REPO:-https://github.com/lintware/lmux.git}
ref=${LMUX_UPDATE_REF:-main}
want_app=${LMUX_UPDATE_APP:-auto}
want_gateway=${LMUX_UPDATE_GATEWAY:-auto}
src=${LMUX_UPDATE_SRC:-$HOME/.local/share/lmux/src}
target_dir=${LMUX_UPDATE_TARGET:-$HOME/.local/share/lmux/target}
bindir=${LMUX_UPDATE_BIN:-$HOME/.local/bin}
state=${LMUX_STATE_DIR:-$HOME/.local/state/lmux}
bin=$bindir/lmux
sock=$state/lmux.sock
held=$state/update.held
log=$state/update.log

mode=update restart=1 force=0 ref_arg=""
while [ $# -gt 0 ]; do
  case $1 in
    --check) mode=check ;;
    --status) mode=status ;;
    --auto) mode=auto ;;
    --install-timer) mode=install-timer ;;
    --remove-timer) mode=remove-timer ;;
    --ref) ref_arg=$2; shift ;;
    --ref=*) ref_arg=${1#--ref=} ;;
    --force) force=1 ;;
    --no-restart) restart=0 ;;
    -h|--help) sed -n '2,32p' "$0"; exit 0 ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
  shift
done
[ -n "$ref_arg" ] && ref=$ref_arg
mkdir -p "$state"

say() { printf '%s\n' "$*"; printf '%s %s\n' "$(date -u +%FT%TZ)" "$*" >>"$log"; }
die() { say "error: $*"; exit 1; }
quiet() { [ "$mode" = auto ]; }

# The commit a binary says it was built from: `lmux 0.1.0 (1a2b3c4d)`.
build_of() { "$1" --version 2>/dev/null | sed -n 's/.*(\([0-9a-f]\{7,\}\)\(-dirty\)\{0,1\}).*/\1/p'; }
# The commit the running daemon says it runs (newer daemons only).
daemon_build() { "$bin" --socket "$sock" rpc system.ping 2>/dev/null | sed -n 's/.*"build": *"\([0-9a-f]\{7,\}\).*/\1/p'; }

# --- timer ---------------------------------------------------------------
self=$src/deploy/update.sh
if [ "$mode" = install-timer ] || [ "$mode" = remove-timer ]; then
  if [ "$os" = Linux ]; then
    unit=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user
    if [ "$mode" = remove-timer ]; then
      systemctl --user disable --now lmux-update.timer 2>/dev/null || true
      rm -f "$unit/lmux-update.service" "$unit/lmux-update.timer"
      systemctl --user daemon-reload; echo "automatic updates off"; exit 0
    fi
    mkdir -p "$unit"
    cat >"$unit/lmux-update.service" <<EOF
[Unit]
Description=lmux updater (pulls $repo)
[Service]
Type=oneshot
ExecStart=/bin/bash $self --auto
Nice=10
EOF
    cat >"$unit/lmux-update.timer" <<EOF
[Unit]
Description=Check GitHub for lmux updates hourly
[Timer]
OnBootSec=10min
OnUnitActiveSec=1h
RandomizedDelaySec=10min
Persistent=true
[Install]
WantedBy=timers.target
EOF
    systemctl --user daemon-reload
    systemctl --user enable --now lmux-update.timer >/dev/null 2>&1
    echo "automatic updates on: $(systemctl --user is-active lmux-update.timer) (systemd timer lmux-update)"
  else
    plist=$HOME/Library/LaunchAgents/dev.lmux.update.plist
    domain=gui/$(id -u); launchctl print "$domain" >/dev/null 2>&1 || domain=user/$(id -u)
    launchctl bootout "$domain/dev.lmux.update" 2>/dev/null || true
    if [ "$mode" = remove-timer ]; then rm -f "$plist"; echo "automatic updates off"; exit 0; fi
    mkdir -p "$HOME/Library/LaunchAgents"
    cat >"$plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>dev.lmux.update</string>
  <key>ProgramArguments</key>
  <array><string>/bin/bash</string><string>$self</string><string>--auto</string></array>
  <key>StartInterval</key><integer>3600</integer>
  <key>RunAtLoad</key><false/>
  <key>LowPriorityIO</key><true/>
  <key>Nice</key><integer>10</integer>
  <key>StandardOutPath</key><string>$state/update.out</string>
  <key>StandardErrorPath</key><string>$state/update.out</string>
</dict>
</plist>
EOF
    launchctl bootstrap "$domain" "$plist"
    echo "automatic updates on (launchd agent dev.lmux.update, hourly)"
  fi
  # Remember the ref, so the timer follows the same one.
  mkdir -p "$(dirname "$conf")"
  touch "$conf"
  grep -v '^LMUX_UPDATE_REF=' "$conf" >"$conf.tmp" || true
  echo "LMUX_UPDATE_REF=$ref" >>"$conf.tmp"
  mv "$conf.tmp" "$conf"
  echo "following $ref from $repo"
  exit 0
fi

if [ "$mode" = auto ] && [ -f "$held" ]; then
  exit 0
fi

# One run at a time (a stale lock from a crash expires after two hours).
lock=$state/update.lock
if ! mkdir "$lock" 2>/dev/null; then
  if [ -n "$(find "$lock" -maxdepth 0 -mmin +120 2>/dev/null)" ]; then
    rm -rf "$lock"; mkdir "$lock"
  else
    quiet && exit 0
    die "another update is running ($lock)"
  fi
fi
trap 'rm -rf "$lock"; rm -f "$LMUX_UPDATE_COPY"' EXIT

# --- fetch ---------------------------------------------------------------
if [ ! -d "$src/.git" ]; then
  quiet || echo "==> cloning $repo"
  mkdir -p "$(dirname "$src")"
  git clone -q --filter=blob:none "$repo" "$src"
fi
git -C "$src" remote set-url origin "$repo"
git -C "$src" fetch -q --prune --tags origin "+refs/heads/*:refs/remotes/origin/*" || die "cannot reach $repo"
commit=$(git -C "$src" rev-parse -q --verify "origin/$ref^{commit}" 2>/dev/null \
  || git -C "$src" rev-parse -q --verify "$ref^{commit}" 2>/dev/null) || die "no ref $ref in $repo"
short=$(git -C "$src" rev-parse --short=8 "$commit")
installed=$( [ -x "$bin" ] && build_of "$bin" || true)
running=$(daemon_build || true)

if [ "$mode" = status ] || [ "$mode" = check ]; then
  echo "follows:   $ref ($repo)"
  echo "latest:    $short $(git -C "$src" log -1 --format=%s "$commit")"
  echo "installed: ${installed:-unknown}"
  echo "running:   ${running:-unknown (older daemon, or not running)}"
  [ -f "$held" ] && echo "held:      $(cat "$held")"
  if [ "$mode" = check ]; then
    [ "$installed" = "$short" ] && [ "${running:-$short}" = "$short" ] && exit 0
    exit 10
  fi
  exit 0
fi

if [ "$installed" = "$short" ] && [ "$force" = 0 ]; then
  if [ "$running" = "$short" ] || [ "$restart" = 0 ]; then
    quiet || say "up to date: $short"
    exit 0
  fi
  say "installed $short, but the daemon runs ${running:-an older build}: restarting it"
  need_build=0
else
  need_build=1
fi

# --- build ---------------------------------------------------------------
app_bundle=""
if [ "$os" = Darwin ]; then
  case $want_app in
    1|yes|always) app_bundle=/Applications/lmux.app ;;
    auto) [ -d /Applications/lmux.app ] && app_bundle=/Applications/lmux.app ;;
  esac
fi

# The app needs a newer Rust than the CLI: the newest installed toolchain
# that is new enough, without changing the user's default.
app_toolchain() {
  local best="" bv=0 tc v
  for tc in $(rustup toolchain list 2>/dev/null | awk '{print $1}'); do
    v=$(rustup run "$tc" rustc --version 2>/dev/null | awk '{split($2,a,"."); print a[1]*10000+a[2]*100+a[3]}')
    [ -n "$v" ] && [ "$v" -gt "$bv" ] && { bv=$v; best=$tc; }
  done
  [ "$bv" -ge 19500 ] && echo "$best"
}

new_bin=$bin
if [ "$need_build" = 1 ]; then
  say "updating ${installed:-unknown} -> $short ($ref)"
  git -C "$src" -c advice.detachedHead=false checkout -q --force --detach "$commit"
  git -C "$src" clean -qfdx -e target
  export LMUX_BUILD_ID=$short
  if [ -n "$app_bundle" ]; then
    tc=$(app_toolchain) || true
    [ -n "$tc" ] || die "lmux.app needs Rust 1.95 or newer (rustup toolchain install stable)"
    say "building lmux.app with $tc"
    # bundle.sh builds the CLI and the app into the clone's own target dirs
    # and signs the bundle; the CLI it installs is the bundle's copy.
    (cd "$src" && RUSTUP_TOOLCHAIN=$tc LMUX_SKIP_INSTALL=1 bash deploy/macos/bundle.sh >>"$log" 2>&1) || die "lmux.app build failed (see $log)"
    new_bin=$src/target/bundle/lmux.app/Contents/MacOS/lmux
  else
    (cd "$src" && CARGO_TARGET_DIR=$target_dir cargo build --release -q -p lmux) || die "build of $short failed"
    new_bin=$target_dir/release/lmux
  fi
  [ "$(build_of "$new_bin")" = "$short" ] || die "the new binary does not report $short"
fi

# --- the daemon ------------------------------------------------------------
# The daemon serving this host's socket: it says its pid (newer builds), or
# it is the process holding the socket.
daemon_pid() {
  local p
  p=$("$bin" --socket "$sock" rpc system.ping 2>/dev/null | sed -n 's/.*"pid": *\([0-9][0-9]*\).*/\1/p')
  if [ -z "$p" ] && command -v lsof >/dev/null; then
    for p in $(lsof -t "$sock" 2>/dev/null); do
      ps -o command= -p "$p" | grep -q ' server' && break
      p=""
    done
  fi
  if [ -z "$p" ] && [ "$os" = Linux ] && [ "$state" = "$HOME/.local/state/lmux" ]; then
    p=$(systemctl --user show -p MainPID --value lmuxd 2>/dev/null)
    [ "$p" = 0 ] && p=""
  fi
  echo "$p"
}
pid=$(daemon_pid)
cmdline=$( [ -n "$pid" ] && ps -o command= -p "$pid" || true)
tmux_socket=$(printf '%s' "$cmdline" | sed -n 's/.*--tmux-socket[ =]\([^ ]*\).*/\1/p')
tmux_socket=${tmux_socket:-lmux}

# --- sessions before ------------------------------------------------------
# Every terminal's tmux pane id and its process, from tmux itself.
panes() { tmux -L "$tmux_socket" list-panes -a -F '#{pane_id} #{pane_pid}' 2>/dev/null | sort; }
surfaces() { "$bin" --socket "$sock" --json tree 2>/dev/null | python3 -c '
import json, sys
t = json.load(sys.stdin)
ids = [w["id"] for w in t.get("workspaces", [])]
for w in t.get("workspaces", []):
    for p in w.get("panes", []):
        ids += [s["id"] for s in p.get("surfaces", [])]
print("\n".join(sorted(ids)))' 2>/dev/null || true; }
before_panes=$(panes)
before_surfaces=$(surfaces)

# --- install -------------------------------------------------------------
prev=$bindir/lmux.prev
prev_app="" have_prev=0
if [ "$need_build" = 1 ]; then
  mkdir -p "$bindir"
  [ -x "$bin" ] && cp -p "$bin" "$prev" && have_prev=1
  if [ -n "$app_bundle" ]; then
    if [ -d "$app_bundle" ]; then
      prev_app=$app_bundle.prev
      rm -rf "$prev_app" && mv "$app_bundle" "$prev_app"
    fi
    cp -R "$src/target/bundle/lmux.app" "$app_bundle"
  fi
  # A rename, so nothing ever runs a half-written binary.
  install -m755 "$new_bin" "$bin.new" && mv -f "$bin.new" "$bin"
  say "installed $short"
fi

# --- restart -------------------------------------------------------------
# Restart exactly the daemon serving this socket, the way it was started:
# its systemd unit, its launchd job, or (started by the app or by hand) the
# same command line again.
restart_daemon() {
  local p domain cmd exe path in_app
  p=$(daemon_pid)
  [ -n "$p" ] || p=$pid
  if [ "$os" = Linux ] && [ -n "$p" ] && [ "$(systemctl --user show -p MainPID --value lmuxd 2>/dev/null)" = "$p" ]; then
    systemctl --user restart lmuxd
    return
  fi
  if [ "$os" = Darwin ]; then
    domain=gui/$(id -u); launchctl print "$domain" >/dev/null 2>&1 || domain=user/$(id -u)
    if [ -n "$p" ] && launchctl print "$domain/dev.lmux.lmuxd" 2>/dev/null | grep -q "pid = $p\$"; then
      launchctl kickstart -k "$domain/dev.lmux.lmuxd"
      return
    fi
  fi
  [ -n "$p" ] || return 0
  cmd=$(ps -o command= -p "$p")
  [ -n "$cmd" ] || cmd=$cmdline
  exe=${cmd%% *}
  # The app runs its daemon with the bundle's own tmux first on PATH.
  path=$PATH
  case $exe in */Contents/MacOS/*) path="$(dirname "$exe"):$PATH" ;; esac
  # Same arguments, on the binary this run installed (unless the daemon is
  # the app's own and the app was rebuilt, which replaced it in place).
  in_app=0
  [ -n "$app_bundle" ] && [ "${exe#"$app_bundle"/}" != "$exe" ] && in_app=1
  [ "$exe" = "$bin" ] || [ "$in_app" = 1 ] || cmd="$bin ${cmd#* }"
  kill -TERM "$p" 2>/dev/null || true
  for _ in $(seq 50); do kill -0 "$p" 2>/dev/null || break; sleep 0.2; done
  # exec: no shell is left holding the caller's output open.
  # shellcheck disable=SC2086
  ( cd "$HOME" && PATH=$path exec nohup $cmd >>"$state/daemon.log" 2>&1 </dev/null ) &
}

verify() {
  local i ok=0
  for i in $(seq 60); do
    [ "$(daemon_build)" = "$short" ] && { ok=1; break; }
    sleep 0.5
  done
  [ "$ok" = 1 ] || { echo "the daemon does not answer with $short"; return 1; }
  sleep 2
  local after_panes after_surfaces lost
  after_panes=$(panes)
  after_surfaces=$(surfaces)
  # Panes and surfaces that existed before must all still exist. New ones
  # may have appeared; one closed on purpose meanwhile would show as lost.
  lost=$(comm -23 <(printf '%s\n' "$before_panes") <(printf '%s\n' "$after_panes") | sed '/^$/d')
  [ -z "$lost" ] || { echo "panes lost: $(echo $lost)"; return 1; }
  lost=$(comm -23 <(printf '%s\n' "$before_surfaces") <(printf '%s\n' "$after_surfaces") | sed '/^$/d')
  [ -z "$lost" ] || { echo "sessions lost: $(echo $lost)"; return 1; }
  echo "$(printf '%s\n' "$before_panes" | sed '/^$/d' | wc -l | tr -d ' ') panes and $(printf '%s\n' "$before_surfaces" | sed '/^$/d' | wc -l | tr -d ' ') workspaces/surfaces kept"
}

if [ "$restart" = 0 ]; then
  say "installed $short; daemon not restarted (--no-restart): new panes keep the old build until it is"
  exit 0
fi

if [ -z "$pid" ]; then
  say "no daemon running here; nothing to restart"
  exit 0
fi

restart_daemon
if result=$(verify); then
  say "daemon restarted on $short: $result"
else
  say "check failed after restart: $result. Rolling back."
  if [ "$have_prev" = 1 ]; then
    mv -f "$prev" "$bin"
    if [ -n "$prev_app" ] && [ -d "$prev_app" ]; then rm -rf "$app_bundle" && mv "$prev_app" "$app_bundle"; fi
    restart_daemon
  fi
  echo "$(date -u +%FT%TZ) $short: $result" >"$held"
  die "update to $short rolled back; automatic updates held (remove $held after looking)"
fi
rm -f "$held"

# --- phone gateway -------------------------------------------------------
if [ "$os" = Linux ] && [ "$want_gateway" != never ] && systemctl --user is-active -q lmux-gateway 2>/dev/null; then
  systemctl --user restart lmux-gateway
  for _ in $(seq 60); do [ -S "$state/share.sock" ] && break; sleep 1; done
  if systemctl --user is-active -q lmux-gateway && [ -S "$state/share.sock" ]; then
    say "phone gateway restarted on $short"
  else
    die "the phone gateway did not come back (journalctl --user -u lmux-gateway)"
  fi
fi
say "done: $short"
