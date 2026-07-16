#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)

export LC_ALL=C
export TZ=UTC
umask 022

ARCH=
STAGE=
OUTPUT=
SIZE_MB=96
OWNER_MODE=root
SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-1704067200}

usage() {
    cat <<'EOF'
Usage: scripts/create-rootfs-image.sh \
  --arch {rv|la} --stage DIR --output IMAGE [--size-mb N]
  [--owner-mode {root|preserve}]

Create a byte-reproducible ext4 image from one completed staging tree. The
source tree's mtimes and all ext4 creation times are normalized to
SOURCE_DATE_EPOCH. Filesystem UUID, directory hash seed, and lazy-init policy
are fixed by the architecture-specific image contract.
EOF
}

while (($#)); do
    case "$1" in
        --arch) ARCH=${2:-}; shift 2 ;;
        --stage) STAGE=${2:-}; shift 2 ;;
        --output) OUTPUT=${2:-}; shift 2 ;;
        --size-mb) SIZE_MB=${2:-}; shift 2 ;;
        --owner-mode) OWNER_MODE=${2:-}; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) printf 'unknown argument: %s\n' "$1" >&2; exit 2 ;;
    esac
done

case "$ARCH" in
    rv|la) ;;
    *) printf '%s\n' '--arch must be rv or la' >&2; exit 2 ;;
esac
[ -n "$STAGE" ] || { printf '%s\n' '--stage is required' >&2; exit 2; }
[ -n "$OUTPUT" ] || { printf '%s\n' '--output is required' >&2; exit 2; }
case "$SIZE_MB" in
    ''|*[!0-9]*) printf 'invalid --size-mb: %s\n' "$SIZE_MB" >&2; exit 2 ;;
esac
[ "$SIZE_MB" -ge 32 ] \
    || { printf '%s\n' '--size-mb must be at least 32' >&2; exit 2; }
case "$SOURCE_DATE_EPOCH" in
    ''|*[!0-9]*)
        printf 'SOURCE_DATE_EPOCH must be a non-negative integer: %s\n' \
            "$SOURCE_DATE_EPOCH" >&2
        exit 2
        ;;
esac
case "$OWNER_MODE" in
    root|preserve) ;;
    *) printf '%s\n' '--owner-mode must be root or preserve' >&2; exit 2 ;;
esac

for command in debugfs find grep mke2fs mkdir mktemp mv realpath sha256sum \
    touch truncate wc; do
    command -v "$command" >/dev/null 2>&1 || {
        printf 'required command not found: %s\n' "$command" >&2
        exit 1
    }
done
owner_runner=()
if [ "$OWNER_MODE" = root ]; then
    command -v fakeroot >/dev/null 2>&1 \
        || { printf '%s\n' 'required command not found: fakeroot' >&2; exit 1; }
    owner_runner=(fakeroot --)
fi

STAGE=$(realpath -e "$STAGE")
[ -d "$STAGE" ] || { printf 'stage is not a directory: %s\n' "$STAGE" >&2; exit 2; }
OUTPUT=$(realpath -m "$OUTPUT")
mkdir -p "$(dirname -- "$OUTPUT")"

# install(1), compiler outputs, checkout files, and symlinks otherwise retain
# wall-clock or worktree-specific mtimes. mke2fs copies those inode times even
# when its own superblock clock is fixed, so normalize the complete tree first.
find "$STAGE" -xdev -exec \
    touch -h -d "@$SOURCE_DATE_EPOCH" -- {} +

UUID_HEX=$(printf 'thekernel-test-rootfs-v1:%s' "$ARCH" \
    | sha256sum | awk '{print $1}')
FS_UUID=${UUID_HEX:0:8}-${UUID_HEX:8:4}-${UUID_HEX:12:4}-${UUID_HEX:16:4}-${UUID_HEX:20:12}
TEMP_IMAGE=$(mktemp "$(dirname -- "$OUTPUT")/.rootfs-image.XXXXXX")
TEMP_COMMANDS=$(mktemp "$(dirname -- "$OUTPUT")/.rootfs-times.XXXXXX")
TEMP_DEBUGFS_LOG=$(mktemp "$(dirname -- "$OUTPUT")/.rootfs-debugfs.XXXXXX")
cleanup() {
    rm -f -- "$TEMP_IMAGE" "$TEMP_COMMANDS" "$TEMP_DEBUGFS_LOG"
}
trap cleanup EXIT
truncate -s "${SIZE_MB}M" "$TEMP_IMAGE"

# E2FSPROGS_FAKE_TIME is the libext2fs clock contract. SOURCE_DATE_EPOCH alone
# is not consumed by mke2fs 1.47 and leaves create/write/check times variable.
E2FSPROGS_FAKE_TIME=$SOURCE_DATE_EPOCH "${owner_runner[@]}" sh -c '
    set -eu
    if [ "$4" = root ]; then
        chown -R 0:0 "$1"
    fi
    mke2fs -q -F -t ext4 -b 4096 -d "$1" \
        -E "no_copy_xattrs,root_owner=0:0,hash_seed=$3,lazy_itable_init=0,lazy_journal_init=0" \
        -U "$3" -L THEKERNEL_ROOT "$2"
' sh "$STAGE" "$TEMP_IMAGE" "$FS_UUID" "$OWNER_MODE"

# mke2fs -d deliberately preserves source inode metadata. Its internal fake
# clock fixes filesystem-created fields, but traversing the source tree still
# refreshes directory/file atime and touch(1) necessarily gives source inodes a
# wall-clock ctime. Normalize every path imported from the controlled staging
# tree through libext2fs, which also recomputes each ext4 inode checksum.
: >"$TEMP_COMMANDS"
while IFS= read -r -d '' entry; do
    relative=${entry#"$STAGE"}
    if [ -z "$relative" ]; then
        relative=/
    fi
    # The debugfs command parser treats backslashes literally inside quotes,
    # but it has no representation for a literal double quote in a quoted
    # filespec. Reject the latter (and command separators) instead of silently
    # leaving one inode unnormalized.
    case "$relative" in
        *$'\n'*|*$'\r'*|*\"*)
            printf 'rootfs path cannot be represented by debugfs: %q\n' \
                "$relative" >&2
            exit 1
            ;;
    esac
    for field in atime ctime mtime crtime; do
        printf 'set_inode_field "%s" %s @%s\n' \
            "$relative" "$field" "$SOURCE_DATE_EPOCH" >>"$TEMP_COMMANDS"
    done
done < <(find "$STAGE" -xdev -print0)

E2FSPROGS_FAKE_TIME=$SOURCE_DATE_EPOCH \
    debugfs -w -f "$TEMP_COMMANDS" "$TEMP_IMAGE" \
    >/dev/null 2>"$TEMP_DEBUGFS_LOG"
# debugfs returns success even when an individual command fails. A valid batch
# writes only its one version banner to stderr; reject every additional line.
if [ "$(wc -l <"$TEMP_DEBUGFS_LOG")" -ne 1 ] \
    || ! grep -Eq '^debugfs [0-9]' "$TEMP_DEBUGFS_LOG"; then
    printf '%s\n' 'debugfs inode-time normalization failed:' >&2
    cat "$TEMP_DEBUGFS_LOG" >&2
    exit 1
fi

mv -f "$TEMP_IMAGE" "$OUTPUT"
trap - EXIT
cleanup
printf '%s\n' "$OUTPUT"
