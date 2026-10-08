#!/bin/sh
# Run on an empty disposable disk in the inspect payload guest.
set -eu
disk=${1:?whole-disk node required}
partition=${2:?first-partition node required}
tag=${3:?AHCI or SDHCI marker prefix required}
case "$tag" in AHCI|SDHCI) ;; *) echo "invalid marker tag: $tag" >&2; exit 2 ;; esac

# Create a GPT partition in the guest. sfdisk asks the kernel to re-read the
# table via BLKRRPART; the new partition must appear before mkfs can open it.
printf 'label: gpt\nunit: sectors\n\nstart=2048, size=32768, type=linux\n' |
    /opt/thekernel-tools/bin/sfdisk "$disk"
[ -b "$partition" ]
/opt/thekernel-tools/bin/mkfs.ext4 -F -L TK_GUEST -O '^64bit,^metadata_csum_seed,^orphan_file' "$partition"

mountpoint=/mnt/block-format-smoke
mkdir -p "$mountpoint"
mount -t ext4 "$partition" "$mountpoint"
printf '%s guest partition test\n' "$tag" > "$mountpoint/probe"
sync
[ "$(cat "$mountpoint/probe")" = "$tag guest partition test" ]
umount "$mountpoint"
echo "${tag}_PARTITION_MKFS_RW_OK"
