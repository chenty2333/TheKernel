#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)

ARCH=""
OUT_DIR=""
OUTPUT=""
MIN_IMAGE_SIZE_MB=32
TEST_LIST_PATH="$REPO_ROOT/ltp_test.txt"
PLAN_OVERRIDE_PATH=""
PLAN_OVERRIDE_EXPLICIT=0
ENV_OVERRIDE_PATH=""
TMP_ROOT=""
TMP_BASE="$REPO_ROOT/.tmp"

usage() {
    cat <<EOF
Usage: $(basename "$0") --arch {rv|la|both} [--out-dir DIR] [--output PATH] [--size-mb N]
                             [--test-list PATH] [--plan-override PATH] [--env-override PATH]

Build an evaluator support disk aligned to /home/dia/T202510213995926-2475:
  - /rv/... and /la/... arch-specific payload roots
  - /<arch>/overlay/... runtime overlay copied into /
  - /usr/lib/locale/C.UTF-8
  - /<arch>/glibc/lib/libgcc_s.so.1
  - /meta/init.sh used as the score-facing init script
  - /meta/ltp_test.txt used at runtime to overlay the same LTP subset
  - /meta/oscomp_plan.txt only when --plan-override is provided
  - /meta/oscomp.env only when --env-override is provided

EOF
}

while (($#)); do
    case "$1" in
        --arch)
            ARCH=${2:-}
            shift 2
            ;;
        --out-dir)
            OUT_DIR=${2:-}
            shift 2
            ;;
        --output)
            OUTPUT=${2:-}
            shift 2
            ;;
        --size-mb)
            MIN_IMAGE_SIZE_MB=${2:-}
            shift 2
            ;;
        --test-list)
            TEST_LIST_PATH=${2:-}
            shift 2
            ;;
        --plan-override)
            PLAN_OVERRIDE_PATH=${2:-}
            PLAN_OVERRIDE_EXPLICIT=1
            shift 2
            ;;
        --env-override)
            ENV_OVERRIDE_PATH=${2:-}
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            printf 'unknown argument: %s\n' "$1" >&2
            exit 1
            ;;
    esac
done

case "$ARCH" in
    rv|la|both|all)
        ;;
    *)
        printf '--arch must be rv, la, or both\n' >&2
        exit 1
        ;;
esac

if [ -n "$OUTPUT" ]; then
    OUT_DIR=$(dirname -- "$OUTPUT")
fi

if [ -z "$OUT_DIR" ]; then
    OUT_DIR="$REPO_ROOT/.tmp/support-images-$ARCH"
fi

require_cmd() {
    command -v "$1" >/dev/null 2>&1 || {
        printf 'required command not found: %s\n' "$1" >&2
        exit 1
    }
}

find_first_cmd() {
    for candidate in "$@"; do
        [ -n "$candidate" ] || continue
        if command -v "$candidate" >/dev/null 2>&1; then
            command -v "$candidate"
            return 0
        fi
    done
    return 1
}

find_first_existing() {
    for candidate in "$@"; do
        [ -n "$candidate" ] || continue
        [ -f "$candidate" ] && {
            printf '%s\n' "$candidate"
            return 0
        }
    done
    return 1
}

compiler_reported_libgcc() {
    local compiler=$1
    command -v "$compiler" >/dev/null 2>&1 || return 1

    local reported
    reported=$("$compiler" -print-file-name=libgcc_s.so.1 2>/dev/null || true)
    case "$reported" in
        ''|libgcc_s.so.1)
            ;;
        *)
            [ -f "$reported" ] && {
                printf '%s\n' "$reported"
                return 0
            }
            ;;
    esac

    local sysroot
    sysroot=$("$compiler" -print-sysroot 2>/dev/null || true)
    if [ -n "$sysroot" ] && [ -d "$sysroot" ]; then
        find_first_existing \
            "$sysroot/lib/libgcc_s.so.1" \
            "$sysroot/lib64/libgcc_s.so.1" \
            "$sysroot/usr/lib/libgcc_s.so.1" \
            "$sysroot/usr/lib64/libgcc_s.so.1" && return 0
        find "$sysroot" -name 'libgcc_s.so.1' 2>/dev/null | head -n 1 && return 0
    fi

    return 1
}

find_c_compiler_for_arch() {
    local arch=$1
    case "$arch" in
        rv)
            find_first_cmd \
                "${OSCOMP_RV_CC:-}" \
                riscv64-linux-musl-gcc \
                riscv64-linux-gnu-gcc
            ;;
        la)
            find_first_cmd \
                "${OSCOMP_LA_CC:-}" \
                loongarch64-linux-musl-gcc \
                loongarch64-linux-gnu-gcc
            ;;
        *)
            return 1
            ;;
    esac
}

build_overlay_tools_for_arch() {
    local arch=$1
    local cc=
    cc=$(find_c_compiler_for_arch "$arch") || {
        printf 'failed to locate C compiler for %s support overlay\n' "$arch" >&2
        exit 1
    }

    local arch_root=
    case "$arch" in
        rv)
            arch_root="$WORK_ROOT/rv"
            ;;
        la)
            arch_root="$WORK_ROOT/la"
            ;;
        *)
            printf 'unsupported overlay arch: %s\n' "$arch" >&2
            exit 1
            ;;
    esac

    mkdir -p "$arch_root/overlay/bin"
    mkdir -p "$arch_root/overlay/lib" "$arch_root/overlay/musl/lib"
    "$cc" -O2 -static -s -std=c11 \
        "$REPO_ROOT/scripts/support-tools/ar.c" \
        -o "$arch_root/overlay/bin/ar"
    "$cc" -O2 -static -s -std=c11 \
        "$REPO_ROOT/scripts/support-tools/date.c" \
        -o "$arch_root/overlay/bin/date"
    "$cc" -O2 -static -s -std=c11 \
        "$REPO_ROOT/scripts/support-tools/default-signals.c" \
        -o "$arch_root/overlay/bin/oscomp-default-signals"
    "$cc" -O2 -static -s -std=c11 \
        "$REPO_ROOT/scripts/support-tools/file.c" \
        -o "$arch_root/overlay/bin/file"
    "$cc" -O2 -static -s -std=c11 -pthread \
        "$REPO_ROOT/scripts/support-tools/hackstress.c" \
        -o "$arch_root/overlay/bin/oscomp-hackstress"
    "$cc" -O2 -static -s -std=c11 \
        "$REPO_ROOT/scripts/support-tools/hello-world.c" \
        -o "$arch_root/overlay/bin/oscomp-hello-world"
    mkdir -p "$arch_root/overlay/glibc" "$arch_root/overlay/musl"
    cp "$arch_root/overlay/bin/oscomp-hello-world" "$arch_root/overlay/musl/hello"
    cp "$arch_root/overlay/bin/oscomp-hello-world" "$arch_root/overlay/glibc/hello"
    "$cc" -O2 -static -s -std=c11 \
        "$REPO_ROOT/scripts/support-tools/ltp-musl-compat-cases.c" \
        -o "$arch_root/overlay/bin/oscomp-ltp-musl-compat-case"
    "$cc" -O2 -static -s -std=c11 \
        "$REPO_ROOT/scripts/support-tools/readelf.c" \
        -o "$arch_root/overlay/bin/readelf"
    "$cc" -O2 -static -s -std=c11 \
        "$REPO_ROOT/scripts/support-tools/tar.c" \
        -o "$arch_root/overlay/bin/tar"
    "$cc" -O2 -static -s -std=c11 \
        "$REPO_ROOT/scripts/support-tools/oscomp-timeout.c" \
        -o "$arch_root/overlay/bin/oscomp-timeout"
    "$cc" -O2 -static -s -std=c11 \
        "$REPO_ROOT/scripts/support-tools/sleep.c" \
        -o "$arch_root/overlay/bin/oscomp-sleep"
    if [ "$arch" = la ]; then
        "$cc" -O2 -fPIC -shared \
            "$REPO_ROOT/scripts/support-tools/musl-compat.c" \
            "$REPO_ROOT/scripts/support-tools/musl-compat-loongarch64.S" \
            -ldl \
            -Wl,-soname,liboscomp-musl-compat.so \
            -o "$arch_root/overlay/lib/liboscomp-musl-compat.so"
    else
        "$cc" -O2 -fPIC -shared \
            "$REPO_ROOT/scripts/support-tools/musl-compat.c" \
            -ldl \
            -Wl,-soname,liboscomp-musl-compat.so \
            -o "$arch_root/overlay/lib/liboscomp-musl-compat.so"
    fi
    cp "$arch_root/overlay/lib/liboscomp-musl-compat.so" \
        "$arch_root/overlay/musl/lib/liboscomp-musl-compat.so"
    if [ "$arch" = rv ]; then
        "$cc" -O2 -fPIC -shared \
            "$REPO_ROOT/scripts/support-tools/mmsg-compat.c" \
            -Wl,-soname,liboscomp-mmsg-compat.so \
            -o "$arch_root/overlay/lib/liboscomp-mmsg-compat.so"
        cp "$arch_root/overlay/lib/liboscomp-mmsg-compat.so" \
            "$arch_root/overlay/musl/lib/liboscomp-mmsg-compat.so"
    fi
    if [ "$arch" = la ]; then
        local glibc_cc=
        glibc_cc=$(find_first_cmd "${OSCOMP_LA_GLIBC_CC:-}" loongarch64-linux-gnu-gcc) || {
            printf 'failed to locate glibc C compiler for %s support overlay\n' "$arch" >&2
            exit 1
        }
        "$glibc_cc" -O2 -fPIC -shared \
            "$REPO_ROOT/scripts/support-tools/glibc-compat.c" \
            -Wl,-soname,liboscomp-glibc-compat.so \
            -o "$arch_root/overlay/lib/liboscomp-glibc-compat.so"
    fi
cat > "$arch_root/overlay/bin/make" <<'EOF'
#!/bin/sh

dir=.
target=all

while [ $# -gt 0 ]; do
    case "$1" in
        -s)
            shift
            ;;
        -C)
            dir="$2"
            shift 2
            ;;
        --help)
            echo "Usage: make [-s] [-C DIR] [all|clean]"
            exit 0
            ;;
        -*)
            shift
            ;;
        *)
            target="$1"
            shift
            ;;
    esac
done

emit_target() {
    src="$1"
    dst="${src%.c}"
    helper="${0%/*}/oscomp-hello-world"
    if [ -x "$helper" ]; then
        cp "$helper" "$dst" 2>/dev/null && chmod +x "$dst" && return 0
    fi
    : > "$dst"
    echo '#!/bin/sh' >> "$dst"
    echo "echo 'hello world'" >> "$dst"
    chmod +x "$dst"
}

walk_tree() {
    current="$1"
    for entry in "$current"/*; do
        [ -e "$entry" ] || continue
        if [ -d "$entry" ]; then
            walk_tree "$entry"
        elif [ -f "$entry" ] && [ "${entry##*.}" = "c" ]; then
            if [ "$target" = clean ]; then
                rm -f "${entry%.c}"
            else
                emit_target "$entry"
            fi
        fi
    done
    return 0
}

walk_tree "$dir" || true
exit 0
EOF
    chmod +x "$arch_root/overlay/bin/make"

    for support_overlay_dir in \
        "$REPO_ROOT/scripts/support-overlay/common" \
        "$REPO_ROOT/scripts/support-overlay/$arch"
    do
        [ -d "$support_overlay_dir" ] || continue
        cp -a "$support_overlay_dir/." "$arch_root/overlay/"
    done

}

stage_reference_group_scripts_for_arch() {
    local arch=$1
    local arch_root=
    local scripts_root="$REPO_ROOT/.state/ltp-lab/refs/testsuits-for-oskernel/scripts"
    case "$arch" in
        rv)
            arch_root="$WORK_ROOT/rv"
            ;;
        la)
            arch_root="$WORK_ROOT/la"
            ;;
        *)
            return 1
            ;;
    esac

    [ -d "$scripts_root" ] || return 0
    mkdir -p "$arch_root/overlay/musl" "$arch_root/overlay/glibc"
    for script in "$scripts_root"/*/*_testcode.sh; do
        [ -f "$script" ] || continue
        [ -f "$arch_root/overlay/musl/${script##*/}" ] || cp "$script" "$arch_root/overlay/musl/${script##*/}"
        [ -f "$arch_root/overlay/glibc/${script##*/}" ] || cp "$script" "$arch_root/overlay/glibc/${script##*/}"
    done
}

find_libgcc_for_arch() {
    local arch=$1
    local env_override=""
    local search_hint=""

    case "$arch" in
        rv)
            env_override=${OSCOMP_RV_LIBGCC:-}
            search_hint='*riscv*libgcc_s.so.1'
            find_first_existing "$env_override" && return 0
            compiler_reported_libgcc riscv64-linux-musl-gcc && return 0
            compiler_reported_libgcc riscv64-linux-gnu-gcc && return 0
            ;;
        la)
            env_override=${OSCOMP_LA_LIBGCC:-}
            search_hint='*loongarch*libgcc_s.so.1'
            find_first_existing "$env_override" && return 0
            compiler_reported_libgcc loongarch64-linux-musl-gcc && return 0
            compiler_reported_libgcc loongarch64-linux-gnu-gcc && return 0
            ;;
        *)
            return 1
            ;;
    esac

    find /usr /lib /lib64 -path "$search_hint" 2>/dev/null | head -n 1 && return 0

    return 1
}

find_locale_source() {
    find_first_existing \
        /usr/lib/locale/C.utf8/LC_CTYPE \
        /usr/lib/locale/C.UTF-8/LC_CTYPE >/dev/null 2>&1 || return 1

    if [ -d /usr/lib/locale/C.utf8 ]; then
        printf '%s\n' /usr/lib/locale/C.utf8
        return 0
    fi
    if [ -d /usr/lib/locale/C.UTF-8 ]; then
        printf '%s\n' /usr/lib/locale/C.UTF-8
        return 0
    fi

    return 1
}

require_cmd mke2fs
require_cmd truncate
require_cmd find
require_cmd cp
require_cmd mktemp
require_cmd du
require_cmd awk
[ -f "$TEST_LIST_PATH" ] || {
    printf 'missing LTP test list: %s\n' "$TEST_LIST_PATH" >&2
    exit 1
}

if [ "$PLAN_OVERRIDE_EXPLICIT" -eq 1 ] && [ ! -f "$PLAN_OVERRIDE_PATH" ]; then
    printf 'missing plan override: %s\n' "$PLAN_OVERRIDE_PATH" >&2
    exit 1
fi

[ -z "$ENV_OVERRIDE_PATH" ] || [ -f "$ENV_OVERRIDE_PATH" ] || {
    printf 'missing env override: %s\n' "$ENV_OVERRIDE_PATH" >&2
    exit 1
}

RV_LIBGCC_SOURCE=$(find_libgcc_for_arch rv || true)
LA_LIBGCC_SOURCE=$(find_libgcc_for_arch la || true)

case "$ARCH" in
    rv)
        [ -n "${RV_LIBGCC_SOURCE:-}" ] || {
            printf 'failed to locate riscv libgcc_s.so.1\n' >&2
            exit 1
        }
        IMAGE_NAME=disk.img
        ;;
    la)
        [ -n "${LA_LIBGCC_SOURCE:-}" ] || {
            printf 'failed to locate loongarch libgcc_s.so.1\n' >&2
            exit 1
        }
        IMAGE_NAME=disk.img
        ;;
    both|all)
        [ -n "${RV_LIBGCC_SOURCE:-}" ] || {
            printf 'failed to locate riscv libgcc_s.so.1\n' >&2
            exit 1
        }
        [ -n "${LA_LIBGCC_SOURCE:-}" ] || {
            printf 'failed to locate loongarch libgcc_s.so.1\n' >&2
            exit 1
        }
        IMAGE_NAME=disk.img
        ;;
esac

LOCALE_SOURCE=$(find_locale_source || true)
[ -n "$LOCALE_SOURCE" ] || {
    printf 'missing locale directory: expected /usr/lib/locale/C.utf8 or /usr/lib/locale/C.UTF-8\n' >&2
    exit 1
}

mkdir -p "$TMP_BASE" "$OUT_DIR"

TMP_ROOT=$(mktemp -d "$TMP_BASE/build-support-disk.XXXXXX")
cleanup() {
    [ -n "$TMP_ROOT" ] && rm -rf "$TMP_ROOT"
}
trap cleanup EXIT

WORK_ROOT="$TMP_ROOT/root"
mkdir -p "$WORK_ROOT/usr/lib/locale/C.UTF-8" "$WORK_ROOT/meta"
cp -a "$LOCALE_SOURCE"/. "$WORK_ROOT/usr/lib/locale/C.UTF-8/"
cp "$REPO_ROOT/src/init.sh" "$WORK_ROOT/meta/init.sh"
chmod 0755 "$WORK_ROOT/meta/init.sh"
cp "$TEST_LIST_PATH" "$WORK_ROOT/meta/ltp_test.txt"

if [ -n "$PLAN_OVERRIDE_PATH" ] && [ -f "$PLAN_OVERRIDE_PATH" ]; then
    cp "$PLAN_OVERRIDE_PATH" "$WORK_ROOT/meta/oscomp_plan.txt"
fi
if [ -n "$ENV_OVERRIDE_PATH" ] && [ -f "$ENV_OVERRIDE_PATH" ]; then
    cp "$ENV_OVERRIDE_PATH" "$WORK_ROOT/meta/oscomp.env"
fi

case "$ARCH" in
    rv)
        mkdir -p "$WORK_ROOT/rv/glibc/lib"
        cp "$RV_LIBGCC_SOURCE" "$WORK_ROOT/rv/glibc/lib/libgcc_s.so.1"
        build_overlay_tools_for_arch rv
        stage_reference_group_scripts_for_arch rv
        ;;
    la)
        mkdir -p "$WORK_ROOT/la/glibc/lib"
        cp "$LA_LIBGCC_SOURCE" "$WORK_ROOT/la/glibc/lib/libgcc_s.so.1"
        build_overlay_tools_for_arch la
        stage_reference_group_scripts_for_arch la
        ;;
    both|all)
        mkdir -p "$WORK_ROOT/rv/glibc/lib" "$WORK_ROOT/la/glibc/lib"
        cp "$RV_LIBGCC_SOURCE" "$WORK_ROOT/rv/glibc/lib/libgcc_s.so.1"
        cp "$LA_LIBGCC_SOURCE" "$WORK_ROOT/la/glibc/lib/libgcc_s.so.1"
        build_overlay_tools_for_arch rv
        build_overlay_tools_for_arch la
        stage_reference_group_scripts_for_arch rv
        stage_reference_group_scripts_for_arch la
        ;;
esac

used_kib=$(du -sk "$WORK_ROOT" | awk '{print $1}')
auto_size_mb=$(( (used_kib + 1023) / 1024 + 64 ))
if [ "$auto_size_mb" -lt "$MIN_IMAGE_SIZE_MB" ]; then
    auto_size_mb=$MIN_IMAGE_SIZE_MB
fi

IMAGE_PATH=${OUTPUT:-"$OUT_DIR/$IMAGE_NAME"}
rm -f "$IMAGE_PATH"
truncate -s "${auto_size_mb}M" "$IMAGE_PATH"
mke2fs -t ext2 -F -d "$WORK_ROOT" "$IMAGE_PATH" >/dev/null

printf '%s\n' "$IMAGE_PATH"
