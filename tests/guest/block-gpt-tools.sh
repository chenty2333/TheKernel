#!/bin/sh
# Optional inspect-payload acceptance, with a disposable 16MiB NVMe image:
# truncate -s 16M IMAGE; parted -s IMAGE mklabel gpt mkpart primary 2048s 10239s
# thekernel.py run --profile shell --toolchain inspect --nvme-disk IMAGE --commands COMMANDS
# COMMANDS runs: /opt/thekernel-tools/block-gpt-tools.sh; /bin/busybox poweroff -f
set -eu
/opt/thekernel-tests/bin/thekernel-block-inventory-smoke --gpt
output=$(/opt/thekernel-tools/bin/lsblk -b -J -o NAME,SIZE,RO,TYPE)
printf '%s\n' "$output"
# JSON output must contain the real 16MiB parent and 4MiB partition. The nested
# child placement is also checked in the human-readable tree during acceptance.
printf '%s\n' "$output" | grep -q '"children"'
printf '%s\n' "$output" | grep -q '"name": "nvme0n1"'
printf '%s\n' "$output" | grep -q '"name": "nvme0n1p1"'
printf '%s\n' "$output" | grep -q '16777216'
printf '%s\n' "$output" | grep -q '4194304'
pairs=$(/opt/thekernel-tools/bin/lsblk -b -P -o NAME,PKNAME,SIZE,RO,TYPE)
printf '%s\n' "$pairs"
printf '%s\n' "$pairs" | grep -q 'NAME="nvme0n1p1" PKNAME="nvme0n1" SIZE="4194304" RO="1" TYPE="part"'
echo THEKERNEL_BLOCK_GPT_TOOLS_OK
