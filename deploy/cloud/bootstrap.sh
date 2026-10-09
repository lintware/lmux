#!/bin/sh
# First boot of a cloud box made for lmux (cloud-init user data / GCP startup
# script, run as root): what install-remote.sh needs on the host.
#   LMUX_USER: the login user (admin on AWS Debian, your name on GCP).
set -eu
U=${LMUX_USER:-admin}
export DEBIAN_FRONTEND=noninteractive
apt-get update -q
apt-get install -yq tmux git curl build-essential pkg-config python3 ca-certificates
id "$U" >/dev/null 2>&1 || useradd -m -s /bin/bash "$U"
loginctl enable-linger "$U" || true
su - "$U" -c 'command -v cargo >/dev/null || curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal'
touch /var/lib/lmux-bootstrap-done
