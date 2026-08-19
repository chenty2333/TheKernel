#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
OUTPUT=""
SIZE_MB=96

usage() {
    cat <<'EOF'
Usage: scripts/build-data-image.sh --output IMAGE [--size-mb N]

Create the independent ext4 data disk used by the formal io_uring physical
performance lane.  The image is attached as QEMU /dev/vdb and is deliberately
not the boot/rootfs image.
EOF
}

while (($#)); do
    case "$1" in
        --output) OUTPUT=${2:-}; shift 2 ;;
        --size-mb) SIZE_MB=${2:-}; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) printf 'unknown argument: %s\n' "$1" >&2; exit 2 ;;
    esac
done

[ -n "$OUTPUT" ] || { printf '%s\n' '--output is required' >&2; exit 2; }
case "$SIZE_MB" in
    ''|*[!0-9]*) printf 'invalid --size-mb: %s\n' "$SIZE_MB" >&2; exit 2 ;;
esac
[ "$SIZE_MB" -ge 32 ] || { printf '%s\n' '--size-mb must be at least 32' >&2; exit 2; }

for command in mke2fs mktemp realpath truncate; do
    command -v "$command" >/dev/null 2>&1 || {
        printf 'required command not found: %s\n' "$command" >&2
        exit 1
    }
done

OUTPUT=$(realpath -m "$OUTPUT")
mkdir -p "$(dirname -- "$OUTPUT")"
temporary="$OUTPUT.tmp.$$"
WORK_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/thekernel-data-image.XXXXXX")
trap 'rm -f "$temporary"; rm -rf "$WORK_ROOT"' EXIT

# Keep the data disk on the same ext4 feature contract as the rootfs.  Recent
# e2fsprogs host defaults enable metadata_csum_seed and orphan_file, which the
# guest lwext4 path does not support.  An explicit config also makes images
# reproducible across developer hosts rather than inheriting /etc/mke2fs.conf.
MKE2FS_CONFIG="$WORK_ROOT/mke2fs.conf"
cat >"$MKE2FS_CONFIG" <<'EOF'
[defaults]
    base_features = none
    default_mntopts = acl,user_xattr
    blocksize = 4096
    inode_size = 256
    inode_ratio = 16384
[fs_types]
    ext4 = {
        features = none,has_journal,ext_attr,dir_index,filetype,extent,64bit,flex_bg,sparse_super,large_file,huge_file,dir_nlink,extra_isize,metadata_csum,^metadata_csum_seed,^orphan_file
    }
    small = {
        blocksize = 1024
        inode_ratio = 4096
    }
EOF
export MKE2FS_CONFIG

truncate -s "${SIZE_MB}M" "$temporary"
# A fixed UUID keeps the artifact reproducible while the short filesystem
# label (ext4 labels are limited to 16 bytes) makes accidental attachment as
# the boot image visible in QEMU evidence.
mke2fs -q -F -t ext4 -b 4096 -L thekernel-perf \
    -U 00000000-0000-4000-8000-000000000001 "$temporary" >/dev/null
mv -f "$temporary" "$OUTPUT"
trap - EXIT
printf '%s\n' "$OUTPUT"
