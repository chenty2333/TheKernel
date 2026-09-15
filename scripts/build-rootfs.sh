#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)

BUSYBOX_VERSION=1.36.1
BUSYBOX_URL=https://busybox.net/downloads/busybox-${BUSYBOX_VERSION}.tar.bz2

ARCH=""
OUTPUT=""
SIZE_MB=${THEKERNEL_ROOTFS_SIZE_MB:-96}
TOOLCHAIN=${THEKERNEL_TOOLCHAIN:-none}
TOOLS_DIR=${THEKERNEL_ROOTFS_TOOLS_DIR:-}
SOURCE_CACHE=${THEKERNEL_SOURCE_CACHE:-$REPO_ROOT/.state/source-cache}

export LC_ALL=C
export TZ=UTC
export SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-1704067200}
export ZERO_AR_DATE=1
umask 022

usage() {
    cat <<'EOF'
Usage: scripts/build-rootfs.sh --arch {x86|x86_64} --output IMAGE [--size-mb N]

Build a project test ext4 root image containing a pinned static BusyBox
userspace, TheKernel's system init, boot-shell entrypoint, and semantic helpers.

Environment overrides:
  THEKERNEL_X86_CROSS_COMPILE x86_64 tool prefix (default: x86_64-linux-gnu-)
  THEKERNEL_USE_LOCAL_MUSL=1  opt into the repository-local .tmp/musl-root
  THEKERNEL_MUSL_ROOT         local musl root (default: .tmp/musl-root)
  THEKERNEL_MUSL_LINUX_UAPI_INCLUDE Linux UAPI headers (default: /usr/include)
  THEKERNEL_MUSL_LINUX_ARCH_INCLUDE architecture UAPI headers (optional)
  THEKERNEL_ROOTFS_OWNER_MODE image ownership (default: root; use preserve
                                when fakeroot is intentionally unavailable)
  THEKERNEL_TOOLCHAIN         guest tool payload name (default: none); it
                                selects which cases the suite contains, so it
                                is compiled into the image, not just recorded;
                                the payload itself is staged separately
  THEKERNEL_ROOTFS_TOOLS_DIR  tree of guest tools to copy into the image
  THEKERNEL_ROOTFS_SIZE_MB    image size (default: 96)
  THEKERNEL_SOURCE_CACHE      Download cache
EOF
}

while (($#)); do
    case "$1" in
        --arch) ARCH=${2:-}; shift 2 ;;
        --output) OUTPUT=${2:-}; shift 2 ;;
        --size-mb) SIZE_MB=${2:-}; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) printf 'unknown argument: %s\n' "$1" >&2; exit 2 ;;
    esac
done

case "$ARCH" in
    x86|x86_64)
        ARCH=x86
        BUSYBOX_ARCH=x86_64
        ;;
    *) printf '%s\n' '--arch must be x86 or x86_64' >&2; exit 2 ;;
esac
[ -n "$OUTPUT" ] || { printf '%s\n' '--output is required' >&2; exit 2; }
case "$SIZE_MB" in
    ''|*[!0-9]*) printf 'invalid --size-mb: %s\n' "$SIZE_MB" >&2; exit 2 ;;
esac
[ "$SIZE_MB" -ge 32 ] || { printf '%s\n' '--size-mb must be at least 32' >&2; exit 2; }
ROOTFS_OWNER_MODE=${THEKERNEL_ROOTFS_OWNER_MODE:-root}
case "$ROOTFS_OWNER_MODE" in
    root|preserve) ;;
    *) printf 'invalid THEKERNEL_ROOTFS_OWNER_MODE: %s\n' "$ROOTFS_OWNER_MODE" >&2; exit 2 ;;
esac

for command in curl debugfs make mke2fs \
    realpath tar touch truncate; do
    command -v "$command" >/dev/null 2>&1 || {
        printf 'required command not found: %s\n' "$command" >&2
        exit 1
    }
done

mkdir -p "$SOURCE_CACHE"
ARCHIVE="$SOURCE_CACHE/busybox-${BUSYBOX_VERSION}.tar.bz2"

if [ ! -f "$ARCHIVE" ]; then
    DOWNLOAD="$ARCHIVE.tmp.$$"
    trap 'rm -f "$DOWNLOAD"' EXIT
    curl --fail --location --retry 3 --output "$DOWNLOAD" "$BUSYBOX_URL"
    mv "$DOWNLOAD" "$ARCHIVE"
    trap - EXIT
fi

OUTPUT=$(realpath -m "$OUTPUT")
mkdir -p "$(dirname -- "$OUTPUT")"
WORK_ROOT=$(mktemp -d "$(dirname -- "$OUTPUT")/.rootfs-build.XXXXXX")
trap 'rm -rf "$WORK_ROOT"' EXIT

# mke2fs inherits the host's ext4 defaults.  Recent e2fsprogs enables
# metadata_csum_seed (incompat 0x2000), which lwext4 rejects even though the
# image otherwise only uses its supported incompat features (0x2c2 <= 0x2d2).
# lwext4 documents has_journal, ext_attr, dir_index, extents, 64bit, flex_bg,
# sparse_super, large_file, huge_file, dir_nlink, extra_isize, and metadata_csum
# as supported, but explicitly documents resize_inode as unsupported. Its
# compatible-feature check does not reject that bit, so do not enable it here.
# Keep the complete feature set in a throw-away config so rootfs creation is
# independent of the host defaults while retaining the supported ext4 layout.
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

# Keep the explicit toolchain override authoritative.  The local musl path is
# intentionally opt-in: .tmp is a developer cache, not a checked-in or
# implicit compatibility toolchain.  The wrapper is materialized below the
# throw-away rootfs work directory and is removed with it on exit.
if [ -n "${THEKERNEL_X86_CROSS_COMPILE:-}" ]; then
    CROSS_COMPILE=$THEKERNEL_X86_CROSS_COMPILE
elif [ "${THEKERNEL_USE_LOCAL_MUSL:-0}" = 1 ]; then
    MUSL_ROOT=${THEKERNEL_MUSL_ROOT:-$REPO_ROOT/.tmp/musl-root}
    MUSL_ROOT=$(realpath -e "$MUSL_ROOT") || {
        printf 'local musl root does not exist: %s\n' "$MUSL_ROOT" >&2
        exit 1
    }
    MUSL_PREFIX="$MUSL_ROOT/usr/x86_64-linux-musl"
    MUSL_SPEC_TEMPLATE="$MUSL_PREFIX/lib64/musl-gcc.specs"
    for path in "$MUSL_PREFIX/include" "$MUSL_PREFIX/lib64" \
        "$MUSL_SPEC_TEMPLATE"; do
        [ -e "$path" ] || {
            printf 'local musl root is incomplete: %s\n' "$path" >&2
            exit 1
        }
    done

    REAL_GCC=$(command -v gcc) || {
        printf '%s\n' 'local musl toolchain requires a host gcc' >&2
        exit 1
    }
    MUSL_UAPI_INCLUDE=${THEKERNEL_MUSL_LINUX_UAPI_INCLUDE:-/usr/include}
    [ -d "$MUSL_UAPI_INCLUDE/linux" ] || {
        printf 'local musl toolchain requires Linux UAPI headers: %s/linux\n' \
            "$MUSL_UAPI_INCLUDE" >&2
        exit 1
    }
    MUSL_UAPI_ARCH_INCLUDE=${THEKERNEL_MUSL_LINUX_ARCH_INCLUDE:-}
    if [ -z "$MUSL_UAPI_ARCH_INCLUDE" ] &&
        [ -d "$MUSL_UAPI_INCLUDE/x86_64-linux-gnu" ]; then
        MUSL_UAPI_ARCH_INCLUDE="$MUSL_UAPI_INCLUDE/x86_64-linux-gnu"
    fi
    if [ -n "$MUSL_UAPI_ARCH_INCLUDE" ] &&
        [ ! -d "$MUSL_UAPI_ARCH_INCLUDE/asm" ]; then
        printf 'local musl architecture UAPI headers are incomplete: %s/asm\n' \
            "$MUSL_UAPI_ARCH_INCLUDE" >&2
        exit 1
    fi
    TOOLCHAIN_DIR="$WORK_ROOT/toolchain"
    mkdir -p "$TOOLCHAIN_DIR"
    LOCAL_SPEC="$TOOLCHAIN_DIR/musl-gcc.specs"

    # Fedora's musl-gcc.specs records the install prefix it was generated
    # from.  Rewrite that prefix so a copied .tmp/musl-root remains usable
    # from another checkout or an overridden THEKERNEL_MUSL_ROOT.
    OLD_MUSL_PREFIX=$(sed -n 's/.*-isystem \([^ ]*\)\/include.*/\1/p' \
        "$MUSL_SPEC_TEMPLATE" | head -n 1)
    [ -n "$OLD_MUSL_PREFIX" ] || {
        printf 'cannot determine musl prefix from: %s\n' "$MUSL_SPEC_TEMPLATE" >&2
        exit 1
    }
    OLD_MUSL_PREFIX_ESC=$(printf '%s' "$OLD_MUSL_PREFIX" |
        sed 's/[.[\*^$\\]/\\&/g')
    NEW_MUSL_PREFIX_ESC=$(printf '%s' "$MUSL_PREFIX" |
        sed 's/[&|\\]/\\&/g')
    sed "s|$OLD_MUSL_PREFIX_ESC|$NEW_MUSL_PREFIX_ESC|g" \
        "$MUSL_SPEC_TEMPLATE" >"$LOCAL_SPEC"

    CROSS_COMPILE="$TOOLCHAIN_DIR/x86_64-linux-musl-"
    UAPI_FLAGS="-idirafter \"$MUSL_UAPI_INCLUDE\""
    if [ -n "$MUSL_UAPI_ARCH_INCLUDE" ]; then
        UAPI_FLAGS="$UAPI_FLAGS -idirafter \"$MUSL_UAPI_ARCH_INCLUDE\""
    fi
    cat >"${CROSS_COMPILE}gcc" <<EOF
#!/bin/sh
exec "$REAL_GCC" -specs "$LOCAL_SPEC" $UAPI_FLAGS "\$@"
EOF
    chmod 0755 "${CROSS_COMPILE}gcc"
    # BusyBox's build can invoke these tools through CROSS_COMPILE even though
    # the local musl sysroot only needs the host binutils.
    for tool in ar as ld nm objcopy objdump ranlib readelf strip; do
        tool_path=$(command -v "$tool") || {
            printf 'required host tool not found for local musl: %s\n' "$tool" >&2
            exit 1
        }
        ln -s "$tool_path" "${CROSS_COMPILE}${tool}"
    done
    printf 'build-rootfs: using opt-in local musl root %s\n' "$MUSL_ROOT" >&2
else
    CROSS_COMPILE=x86_64-linux-gnu-
    # Non-Debian x86_64 hosts (e.g. Fedora) ship no x86_64-linux-gnu- prefix;
    # fall back to the native gcc when it already targets x86_64 Linux.
    if ! command -v "${CROSS_COMPILE}gcc" >/dev/null 2>&1 &&
        [ "$(uname -m)" = x86_64 ] &&
        command -v gcc >/dev/null 2>&1; then
        case "$(gcc -dumpmachine)" in
            x86_64-*-linux*)
                CROSS_COMPILE=""
                printf 'build-rootfs: x86_64-linux-gnu-gcc not found; using native gcc\n' >&2
                ;;
        esac
    fi
fi

command -v "${CROSS_COMPILE}gcc" >/dev/null 2>&1 || {
    printf 'required command not found: %sgcc\n' "$CROSS_COMPILE" >&2
    printf 'set THEKERNEL_X86_CROSS_COMPILE or opt into the local musl root with THEKERNEL_USE_LOCAL_MUSL=1\n' >&2
    exit 1
}

# The rootfs requires a static BusyBox; fail early when the selected compiler
# cannot link statically instead of erroring deep inside its build.
printf 'int main(void){return 0;}\n' |
    "${CROSS_COMPILE}gcc" -static -x c - -o "$WORK_ROOT/static-probe" || {
    printf 'build-rootfs: %sgcc cannot link a static binary\n' "$CROSS_COMPILE" >&2
    printf 'install the static C library (Fedora: glibc-static, Debian: libc6-dev)\n' >&2
    exit 1
}
rm -f "$WORK_ROOT/static-probe"

tar -xjf "$ARCHIVE" -C "$WORK_ROOT"
SOURCE_DIR="$WORK_ROOT/busybox-${BUSYBOX_VERSION}"
STAGE="$WORK_ROOT/root"
IMAGE="$WORK_ROOT/rootfs.img"
BUSYBOX_BUILD="$WORK_ROOT/busybox-build"

mkdir -p "$BUSYBOX_BUILD"
install -m 0644 "$REPO_ROOT/tests/rootfs/busybox-${BUSYBOX_VERSION}.config" \
    "$BUSYBOX_BUILD/.config"
# BusyBox's own kconfig host tool is compiled here, before any .config-supplied
# flags can reach it, and its single diagnostic is one more instance of the
# third-party noise the target flags below exist to keep out of `make run`.
KCONFIG_NOTIMESTAMP=1 make -C "$SOURCE_DIR" O="$BUSYBOX_BUILD" \
    ARCH="$BUSYBOX_ARCH" CROSS_COMPILE="$CROSS_COMPILE" \
    HOSTCFLAGS="-Wno-discarded-qualifiers" silentoldconfig </dev/null
KCONFIG_NOTIMESTAMP=1 make -C "$BUSYBOX_BUILD" ARCH="$BUSYBOX_ARCH" \
    CROSS_COMPILE="$CROSS_COMPILE" -j"$(getconf _NPROCESSORS_ONLN)"

mkdir -p "$STAGE"
make -C "$BUSYBOX_BUILD" ARCH="$BUSYBOX_ARCH" \
    CROSS_COMPILE="$CROSS_COMPILE" CONFIG_PREFIX="$STAGE" install

mkdir -p "$STAGE/etc/thekernel" \
    "$STAGE/opt/thekernel-tests/bin" \
    "$STAGE/opt/thekernel-tests/portable" \
    "$STAGE/usr/share/licenses/busybox" \
    "$STAGE/usr/share/licenses/thekernel" \
    "$STAGE/usr/share/doc/thekernel" \
    "$STAGE/dev" "$STAGE/proc" "$STAGE/sys" "$STAGE/tmp" \
    "$STAGE/var/tmp" "$STAGE/root"
chmod 1777 "$STAGE/tmp" "$STAGE/var/tmp"
# Name-service identity for the existing root shell (whoami/id/ls). Locked
# password fields do not enable password login or add another entrypoint.
cat > "$STAGE/etc/passwd" <<'EOF'
root:!:0:0:root:/root:/bin/sh
EOF
cat > "$STAGE/etc/group" <<'EOF'
root:!:0:
EOF
chmod 0644 "$STAGE/etc/passwd" "$STAGE/etc/group"
install -m 0644 "$SOURCE_DIR/LICENSE" \
    "$STAGE/usr/share/licenses/busybox/LICENSE"
install -m 0644 "$BUSYBOX_BUILD/.config" \
    "$STAGE/usr/share/doc/thekernel/busybox.config"
install -m 0644 "$REPO_ROOT/LICENSE" \
    "$STAGE/usr/share/licenses/thekernel/LICENSE"
install -m 0644 "$REPO_ROOT/NOTICE" \
    "$STAGE/usr/share/licenses/thekernel/NOTICE"
install -m 0755 "$REPO_ROOT/tests/guest/shell-init.sh" \
    "$STAGE/etc/thekernel/shell-init.sh"
rm -f "$STAGE/sbin/init"
# The payload selection also selects which cases the suite contains: the
# native-compilation case is only meaningful when the compiler is installed,
# and compiling it in only then keeps the case table (and therefore the KTAP
# plan) a property of the image rather than of what happens to be present.
#
# `nested` is a superset of `tcc`: it stages the compiler *and* the system
# emulator, so its image carries both cases.  Selecting cases this way means
# `--toolchain nested` runs one more case than `--toolchain tcc`, and the plan
# line in the transcript always says which image was booted.
INIT_DEFINES=""
case "$TOOLCHAIN" in
    none) ;;
    tcc) INIT_DEFINES="-DTHEKERNEL_TOOL_PAYLOAD_TCC=1" ;;
    nested) INIT_DEFINES="-DTHEKERNEL_TOOL_PAYLOAD_TCC=1 -DTHEKERNEL_TOOL_PAYLOAD_NESTED=1" ;;
    # `glibc` deliberately does not include the tcc case: it is a staging
    # milestone, and the compiler is a separate payload with its own cost.
    glibc) INIT_DEFINES="-DTHEKERNEL_TOOL_PAYLOAD_GLIBC=1" ;;
    # `gcc` is a superset of `glibc`: the compiler is dynamic, so the glibc
    # loader case runs too and the image proves its own prerequisite.
    gcc) INIT_DEFINES="-DTHEKERNEL_TOOL_PAYLOAD_GLIBC=1 -DTHEKERNEL_TOOL_PAYLOAD_GCC=1" ;;
    *) printf 'unknown THEKERNEL_TOOLCHAIN: %s\n' "$TOOLCHAIN" >&2; exit 2 ;;
esac
# shellcheck disable=SC2086 # INIT_DEFINES is a deliberate flag list
"${CROSS_COMPILE}gcc" -O2 -static -s -std=c11 -Wall -Wextra -Werror \
    $INIT_DEFINES \
    "$REPO_ROOT/tests/guest/system-init.c" \
    -o "$STAGE/sbin/init"

for source in "$REPO_ROOT"/tests/guest/tools/*.c; do
    [ -f "$source" ] || continue
    name=${source##*/}
    name=${name%.c}
    "${CROSS_COMPILE}gcc" -O2 -static -s -std=c11 -Wall -Wextra -Werror \
        -pthread "$source" \
        -o "$STAGE/opt/thekernel-tests/bin/thekernel-$name"
done

for source in "$REPO_ROOT"/tests/guest/portable/*.c; do
    [ -f "$source" ] || continue
    name=${source##*/}
    name=${name%.c}
    "${CROSS_COMPILE}gcc" -O2 -static -s -std=c11 -Wall -Wextra -Werror \
        -pthread "$source" \
        -o "$STAGE/opt/thekernel-tests/portable/$name"
done

# The optional guest tool payload: a compiler and its development sysroot,
# built on the host by build-guest-tools.sh and copied in as-is.  Nothing here
# downloads or installs anything at run time, and the baseline image (no
# payload requested) is untouched by this step.
# The nested diagnostics are development evidence, not part of the acceptance
# criteria, so they are gated on a file that only exists when explicitly asked
# for.  THEKERNEL_ROOTFS_DIAGNOSTICS=1 builds the image that way.
if [ "${THEKERNEL_ROOTFS_DIAGNOSTICS:-0}" = "1" ]; then
    : > "$STAGE/opt/thekernel-tests/run-nested-diagnostics"
fi

if [ -n "$TOOLS_DIR" ]; then
    [ "$TOOLCHAIN" != none ] || {
        printf '%s\n' 'a guest tools directory was given without a payload name' >&2
        exit 2
    }
    [ -d "$TOOLS_DIR" ] || {
        printf 'guest tools directory does not exist: %s\n' "$TOOLS_DIR" >&2
        exit 1
    }
    TOOLS_DIR=$(realpath -e "$TOOLS_DIR")
    printf 'build-rootfs: installing %s guest tools from %s\n' \
        "$TOOLCHAIN" "$TOOLS_DIR" >&2
    # -a preserves the payload's symlinks, which is what keeps the duplicate
    # libc.a and header tree from doubling the image size.
    cp -a "$TOOLS_DIR/." "$STAGE/"
fi

"$SCRIPT_DIR/create-rootfs-image.sh" \
    --arch "$ARCH" --stage "$STAGE" --output "$IMAGE" --size-mb "$SIZE_MB" \
    --owner-mode "$ROOTFS_OWNER_MODE"

ROOTFS_BACKUP="$WORK_ROOT/rootfs-previous"
ROOTFS_HAD_PREVIOUS=0
ROOTFS_PUBLISHED=0

if [ -e "$OUTPUT" ] || [ -L "$OUTPUT" ]; then
    [ ! -d "$OUTPUT" ] || {
        printf 'rootfs output must not be a directory: %s\n' "$OUTPUT" >&2
        exit 1
    }
fi
rollback_publication() {
    local status=$1

    set +e
    if [ "$ROOTFS_PUBLISHED" -eq 1 ]; then
        rm -f -- "$OUTPUT"
    fi
    if [ "$ROOTFS_HAD_PREVIOUS" -eq 1 ]; then
        mv -- "$ROOTFS_BACKUP" "$OUTPUT"
    fi
    exit "$status"
}

if [ -e "$OUTPUT" ] || [ -L "$OUTPUT" ]; then
    if ! mv -- "$OUTPUT" "$ROOTFS_BACKUP"; then
        rollback_publication 1
    fi
    ROOTFS_HAD_PREVIOUS=1
fi
if ! mv -- "$IMAGE" "$OUTPUT"; then
    rollback_publication 1
fi
ROOTFS_PUBLISHED=1

rm -f -- "$ROOTFS_BACKUP"
printf '%s\n' "$OUTPUT"
