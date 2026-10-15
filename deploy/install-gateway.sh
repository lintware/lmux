#!/usr/bin/env bash
# Make a Linux server the always-on phone gateway.
#
#   deploy/install-gateway.sh <ssh-target> [--view-only] [--server <ssh-target-as-seen-from-the-gateway>]...
#
# Needs lmux installed there first (deploy/install-remote.sh), and SSH from the
# gateway to every --server without a password prompt. Installs the systemd
# user service `lmux-gateway`, writes its server list, and restarts it. Get a
# pairing QR from the app's REMOTE button, or by hand:
#   ssh <ssh-target> '~/.local/bin/lmux share-code'
set -euo pipefail

target=${1:?usage: install-gateway.sh <ssh-target> [--view-only] [--server <target>]...}
shift
args="--allow-typing"
servers=""
while [ $# -gt 0 ]; do
  case $1 in
    --view-only) args="" ;;
    --server) servers="$servers --server $2"; shift ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
  shift
done
here=$(cd "$(dirname "$0")" && pwd)

echo "==> checking $target"
ssh -o BatchMode=yes "$target" 'set -e
  [ "$(uname)" = Linux ] || { echo "the gateway service needs Linux with systemd"; exit 1; }
  ~/.local/bin/lmux share-backend </dev/null >/dev/null 2>&1 || {
    echo "lmux on this server has no gateway support yet; run deploy/install-remote.sh first"; exit 1; }'

for s in $servers; do
  [ "$s" = --server ] && continue
  printf '==> %s can reach %s: ' "$target" "$s"
  ssh -o BatchMode=yes "$target" "ssh -o BatchMode=yes -o ConnectTimeout=8 $s true" && echo yes || {
    echo "no (fix SSH from the gateway to $s, then rerun)"; exit 1; }
done

echo "==> installing the lmux-gateway service"
ssh -o BatchMode=yes "$target" "mkdir -p ~/.config/systemd/user ~/.config/lmux && cat > ~/.config/systemd/user/lmux-gateway.service" < "$here/lmux-gateway.service"
ssh -o BatchMode=yes "$target" "umask 077; echo 'LMUX_GATEWAY_ARGS=$args$servers' > ~/.config/lmux/gateway.env
  systemctl --user daemon-reload
  systemctl --user enable lmux-gateway >/dev/null 2>&1
  rm -f ~/.local/state/lmux/share.sock
  systemctl --user restart lmux-gateway
  for i in \$(seq 60); do [ -S ~/.local/state/lmux/share.sock ] && break; sleep 1; done
  cat ~/.local/state/lmux/share.json 2>/dev/null || { journalctl --user -u lmux-gateway -n 20 --no-pager; exit 1; }"
echo "==> done. Pair a phone from the app's REMOTE button."
