#!/usr/bin/env bash
# lmux cloud boxes on Google Compute Engine (Debian 12), with the logged-in
# gcloud CLI and its current project.
#
#   gcp.sh create <name> [zone] [machine-type]  -> prints "TARGET lmux-<name>"
#   gcp.sh delete <name> [zone]
#   gcp.sh check  [zone]                          (no changes: project and API)
set -euo pipefail
. "$(dirname "$0")/common.sh"
cmd=${1:?create|delete|check}
if [ "$cmd" = check ]; then name=check; zone=${2:-}; else name=${2:?name}; zone=${3:-}; fi
zone=${zone:-$(gcloud config get-value compute/zone 2>/dev/null || true)}
zone=${zone:-us-central1-a}
mtype=${4:-e2-standard-2}
alias="lmux-$name"
user=$(whoami)

case "$cmd" in
check)
  project=$(gcloud config get-value project 2>/dev/null)
  echo "==> checking Compute Engine in $project ($zone; nothing is created)"
  gcloud compute zones describe "$zone" --format='value(name)' >/dev/null
  echo "ok: Compute Engine is reachable in $project"
  ;;
create)
  echo "==> launching $mtype ($name) in $zone"
  ss=$(mktemp); { echo '#!/bin/sh'; echo "export LMUX_USER=$user"; cat "$(dirname "$0")/bootstrap.sh"; } > "$ss"
  gcloud compute instances create "$alias" --zone "$zone" --machine-type "$mtype" \
    --image-family debian-12 --image-project debian-cloud --boot-disk-size 30GB \
    --labels "lmux=$name" \
    --metadata "ssh-keys=$user:$(cat "$(pubkey)")" --metadata-from-file "startup-script=$ss" \
    --format='value(name)' >/dev/null
  rm -f "$ss"
  ip=$(gcloud compute instances describe "$alias" --zone "$zone" --format='value(networkInterfaces[0].accessConfigs[0].natIP)')
  set_alias "$alias" "$ip" "$user"
  wait_ready "$alias"
  echo "TARGET $alias"
  ;;
delete)
  gcloud compute instances delete "$alias" --zone "$zone" --quiet && echo "==> deleted $alias"
  remove_alias "$alias"
  ;;
esac
