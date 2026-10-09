#!/usr/bin/env bash
# lmux cloud boxes on AWS EC2 (Debian 12), with the logged-in aws CLI.
#
#   aws.sh create <name> [region] [type]   -> prints "TARGET lmux-<name>"
#   aws.sh delete <name> [region]
#   aws.sh check  [region]                  (dry run: permissions only)
set -euo pipefail
. "$(dirname "$0")/common.sh"
cmd=${1:?create|delete|check}
if [ "$cmd" = check ]; then name=check; region=${2:-}; else name=${2:?name}; region=${3:-}; fi
region=${region:-$(aws configure get region 2>/dev/null || true)}
region=${region:-us-east-1}
type=${4:-t3.medium}
alias="lmux-$name"
A() { aws --region "$region" --output text "$@"; }

ami() { A ssm get-parameter --name /aws/service/debian/release/12/latest/amd64 --query Parameter.Value; }

case "$cmd" in
check)
  echo "==> checking EC2 permissions in $region (nothing is created)"
  out=$(A ec2 run-instances --dry-run --image-id "$(ami)" --instance-type "$type" 2>&1 || true)
  case "$out" in *DryRunOperation*) echo "ok: this account can launch $type in $region";; *) echo "$out" >&2; exit 1;; esac
  ;;
create)
  echo "==> key pair and SSH security group in $region"
  key="lmux-$(whoami)"
  A ec2 describe-key-pairs --key-names "$key" >/dev/null 2>&1 \
    || A ec2 import-key-pair --key-name "$key" --public-key-material "fileb://$(pubkey)" >/dev/null
  sg=$(A ec2 describe-security-groups --filters Name=group-name,Values=lmux-ssh --query 'SecurityGroups[0].GroupId' 2>/dev/null || true)
  if [ -z "$sg" ] || [ "$sg" = None ]; then
    sg=$(A ec2 create-security-group --group-name lmux-ssh --description "SSH to lmux boxes" --query GroupId)
    A ec2 authorize-security-group-ingress --group-id "$sg" --protocol tcp --port 22 --cidr 0.0.0.0/0 >/dev/null
  fi
  echo "==> launching $type ($name)"
  ud=$(mktemp); { echo '#!/bin/sh'; echo 'export LMUX_USER=admin'; cat "$(dirname "$0")/bootstrap.sh"; } > "$ud"
  id=$(A ec2 run-instances --image-id "$(ami)" --instance-type "$type" --key-name "$key" \
    --security-group-ids "$sg" --user-data "file://$ud" \
    --block-device-mappings 'DeviceName=/dev/xvda,Ebs={VolumeSize=30,VolumeType=gp3}' \
    --tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=$alias},{Key=lmux,Value=$name}]" \
    --query 'Instances[0].InstanceId')
  rm -f "$ud"
  A ec2 wait instance-running --instance-ids "$id"
  ip=$(A ec2 describe-instances --instance-ids "$id" --query 'Reservations[0].Instances[0].PublicIpAddress')
  set_alias "$alias" "$ip" admin
  wait_ready "$alias"
  echo "TARGET $alias"
  ;;
delete)
  ids=$(A ec2 describe-instances --filters "Name=tag:lmux,Values=$name" "Name=instance-state-name,Values=pending,running,stopping,stopped" --query 'Reservations[].Instances[].InstanceId')
  if [ -n "$ids" ]; then
    A ec2 terminate-instances --instance-ids $ids >/dev/null
    echo "==> terminated $ids"
  fi
  remove_alias "$alias"
  ;;
esac
