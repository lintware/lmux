# Shared by the cloud scripts: the SSH key, an ssh config alias per box, and
# waiting for a new box to be ready.

pubkey() {
  for k in ~/.ssh/id_ed25519.pub ~/.ssh/id_rsa.pub ~/.ssh/id_ecdsa.pub; do
    [ -f "$k" ] && { echo "$k"; return; }
  done
  echo "no SSH public key in ~/.ssh (run ssh-keygen -t ed25519)" >&2; exit 1
}

# `ssh lmux-<name>` reaches the box: one Host block per box in
# ~/.ssh/lmux_config, included from the top of ~/.ssh/config.
set_alias() { # alias host user
  local cfg=~/.ssh/lmux_config
  touch "$cfg"; chmod 600 "$cfg"
  if ! grep -q '^Include lmux_config' ~/.ssh/config 2>/dev/null; then
    { echo "Include lmux_config"; echo; cat ~/.ssh/config 2>/dev/null; } > ~/.ssh/config.lmux-tmp
    mv ~/.ssh/config.lmux-tmp ~/.ssh/config; chmod 600 ~/.ssh/config
  fi
  remove_alias "$1"
  printf 'Host %s\n  HostName %s\n  User %s\n  StrictHostKeyChecking accept-new\n  ServerAliveInterval 15\n\n' "$1" "$2" "$3" >> "$cfg"
}

remove_alias() { # alias
  local cfg=~/.ssh/lmux_config
  [ -f "$cfg" ] || return 0
  awk -v a="$1" '/^Host /{skip=($2==a)} !skip' "$cfg" > "$cfg.tmp" && mv "$cfg.tmp" "$cfg"
}

wait_ready() { # alias
  echo "==> waiting for SSH"
  for i in $(seq 1 60); do
    ssh -o BatchMode=yes -o ConnectTimeout=5 "$1" true 2>/dev/null && break
    sleep 5
  done
  echo "==> waiting for first-boot setup (tmux, Rust)"
  for i in $(seq 1 120); do
    ssh -o BatchMode=yes "$1" 'test -f /var/lib/lmux-bootstrap-done' 2>/dev/null && return 0
    sleep 5
  done
  echo "first-boot setup did not finish" >&2; return 1
}
