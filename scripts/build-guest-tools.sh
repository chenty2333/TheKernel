#!/usr/bin/env bash
# Build the optional guest tool payload: tools that run *inside* TheKernel's
# userspace, as opposed to the BusyBox/init userspace build-rootfs.sh stages.
#
# The tools are built statically against musl on the host and staged into a
# directory tree that build-rootfs.sh copies into the image.  Nothing is
# downloaded by the guest and no package manager runs in the guest: every
# version used here is pinned, and the pinned sources are what get built.
#
# See docs/design/guest-toolchain-and-nested-qemu.md for why this shape was
# chosen and which contracts in the kernel each tool depends on.
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)

# --- pins ------------------------------------------------------------------
#
# The host C compiler is the host GCC driven through the musl specs file from
# this exact Fedora RPM.  Pinning the RPM rather than "whatever musl-gcc is
# installed" keeps the build reproducible on a host with no root access: the
# RPM is unpacked into the cache, never installed.
MUSL_RPM_RELEASE=1.2.5-6.fc44
# Package name and pinned filename differ: `dnf download` takes the bare
# package name, while the file it writes carries the full NVR.
MUSL_RPM_PACKAGES=(musl-gcc musl-devel musl-libc-static)
declare -A MUSL_RPM_FILE=(
    [musl-gcc]="musl-gcc-${MUSL_RPM_RELEASE}.x86_64.rpm"
    [musl-devel]="musl-devel-${MUSL_RPM_RELEASE}.x86_64.rpm"
    [musl-libc-static]="musl-libc-static-${MUSL_RPM_RELEASE}.x86_64.rpm"
)
declare -A MUSL_RPM_SHA256=(
    [musl-gcc]="d93ac75c659e9e91edcce77dde6f328b93d81cb55896e9d189233c505d4d2250"
    [musl-devel]="df10ed1850ecae8ec7161a590968fd58f82e2f82c7e914d106290f95a6963dc5"
    [musl-libc-static]="8cac621cb0c8745d17a66e31168d64c3d1d32d804aa5eb06b29032712bdf941e"
)

# TinyCC builds the guest-side C compiler.  `mob` is the maintained branch;
# the commit is pinned because the branch moves.
TCC_URL=https://repo.or.cz/tinycc.git
TCC_BRANCH=mob
TCC_COMMIT=0fb54300b56512754221d80adda85ddb9815bceb

# --- arguments -------------------------------------------------------------

PAYLOAD=none
OUTPUT=
SOURCE_CACHE=${THEKERNEL_SOURCE_CACHE:-$REPO_ROOT/.state/source-cache}
STATE_ROOT=${THEKERNEL_STATE_DIR:-$HOME/.cache/thekernel-targets}
JOBS=${CARGO_BUILD_JOBS:-$(nproc)}

usage() {
    cat <<'EOF'
Usage: scripts/build-guest-tools.sh --payload {none,tcc,nested,glibc,gcc} --output DIR

Stage the optional guest tool payload into DIR, for build-rootfs.sh to copy
into the image.  `nested`, `glibc` and `gcc` are delegated to their own
builders (build-nested-payload.sh, build-glibc-payload.sh,
build-gcc-payload.sh), which own their pins.

Options:
  --payload NAME     payload to build; `none` stages nothing (default)
  --output DIR       staging tree to populate
  -h, --help         show this help

Environment:
  THEKERNEL_SOURCE_CACHE  download cache (default: REPO/.state/source-cache)
  THEKERNEL_STATE_DIR     build state root (default: ~/.cache/thekernel-targets)
  CARGO_BUILD_JOBS        parallel make jobs (default: nproc)
  THEKERNEL_GUEST_TOOLS_VERBOSE=1  echo each build step
EOF
}

while (($#)); do
    case "$1" in
        --payload) PAYLOAD=${2:-}; shift 2 ;;
        --output) OUTPUT=${2:-}; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) printf 'unknown argument: %s\n' "$1" >&2; exit 2 ;;
    esac
done

case "$PAYLOAD" in
    none|tcc) ;;
    # The nested payload is a superset of `tcc` and is built by its own script,
    # which owns the GLib/QEMU pins.  It is accepted here so that the payload
    # vocabulary has one definition rather than one per builder.
    nested)
        [ -n "$OUTPUT" ] || { printf '%s\n' '--output is required' >&2; exit 2; }
        exec "$SCRIPT_DIR/build-nested-payload.sh" --output "$OUTPUT" --jobs "$JOBS"
        ;;
    # The glibc payload is the opposite of the others: nothing is built, only
    # staged.  It exists because the guest image had no dynamic loader at all,
    # so the kernel's dynamic-linking contract could not be tested.
    glibc)
        [ -n "$OUTPUT" ] || { printf '%s\n' '--output is required' >&2; exit 2; }
        exec "$SCRIPT_DIR/build-glibc-payload.sh" --output "$OUTPUT"
        ;;
    # A real distribution C compiler.  Nothing is compiled here either: the
    # artifacts come from pinned RPMs and the work is staging the closure.
    gcc)
        [ -n "$OUTPUT" ] || { printf '%s\n' '--output is required' >&2; exit 2; }
        exec "$SCRIPT_DIR/build-gcc-payload.sh" --output "$OUTPUT"
        ;;
    *) printf '%s\n' '--payload must be none, tcc, nested, glibc or gcc' >&2; exit 2 ;;
esac

# `none` is a valid request that produces an empty staging tree; the caller
# still gets a directory so it can copy unconditionally.
if [ "$PAYLOAD" = none ]; then
    [ -n "$OUTPUT" ] || { printf '%s\n' '--output is required' >&2; exit 2; }
    mkdir -p "$OUTPUT"
    printf 'build-guest-tools: payload none, nothing staged\n' >&2
    exit 0
fi

[ -n "$OUTPUT" ] || { printf '%s\n' '--output is required' >&2; exit 2; }
OUTPUT=$(realpath -m "$OUTPUT")

for command in curl gcc git make patch rpm2cpio cpio tar; do
    command -v "$command" >/dev/null 2>&1 || {
        printf 'required command not found: %s\n' "$command" >&2
        exit 1
    }
done

log() {
    [ "${THEKERNEL_GUEST_TOOLS_VERBOSE:-0}" = 1 ] && printf 'build-guest-tools: %s\n' "$*" >&2
    return 0
}

mkdir -p "$SOURCE_CACHE" "$STATE_ROOT"
WORK_ROOT=$(mktemp -d "$STATE_ROOT/guest-tools-build.XXXXXX")
trap 'rm -rf "$WORK_ROOT"' EXIT

# --- 1. the pinned musl C compiler ----------------------------------------
#
# Fedora ships a musl-targeting wrapper (`musl-gcc`) around the host GCC plus
# a matching sysroot.  Unpacking it into the cache avoids needing root and
# keeps the compiler version pinned to the RPM release above.

MUSL_ROOT="$STATE_ROOT/musl-host"
MUSL_PREFIX="$MUSL_ROOT/usr/x86_64-linux-musl"

# Fetch one pinned RPM into the source cache.  `dnf download` is given the
# bare package name and the result is renamed to the pinned NVR filename, so
# the cache is keyed by what the pins above actually name.
fetch_rpm() {
    local package=$1 file=$2 expected=$3
    local dir="$SOURCE_CACHE/rpm"
    local path="$dir/$file"
    if [ ! -f "$path" ]; then
        mkdir -p "$dir"
        log "downloading $file"
        ( cd "$dir" && dnf download --destdir . "$package" >/dev/null 2>&1 ) || true
        if [ ! -f "$dir/$file" ]; then
            printf 'cannot download %s into %s\n' "$file" "$dir" >&2
            printf 'fetch %s from a Fedora mirror and place it there\n' "$file" >&2
            exit 1
        fi
    fi
    local actual
    actual=$(sha256sum "$path" | cut -d' ' -f1)
    [ "$actual" = "$expected" ] || {
        printf 'checksum mismatch for %s:\n  expected %s\n  actual   %s\n' \
            "$file" "$expected" "$actual" >&2
        exit 1
    }
    printf '%s\n' "$path"
}

mkdir -p "$MUSL_ROOT"
for package in "${MUSL_RPM_PACKAGES[@]}"; do
    # The `if !` guard is load-bearing: a bare `rpm_path=$(fetch_rpm ...)`
    # assignment would let a failure inside fetch_rpm exit only the command
    # substitution's subshell, and the build would continue with an empty path.
    if ! rpm_path=$(fetch_rpm "$package" "${MUSL_RPM_FILE[$package]}" "${MUSL_RPM_SHA256[$package]}"); then
        exit 1
    fi
    # The extraction is idempotent: the sysroot only needs to exist once.
    if [ ! -e "$MUSL_PREFIX/lib64/libc.a" ]; then
        # Not a pipe: cpio stops reading at the archive trailer and exits, and
        # rpm2cpio can still have the final padding write in flight when it
        # does -- an intermittent SIGPIPE (exit 141) with nothing actually
        # wrong, which pipefail then reports as a failed unpack.
        rpm2cpio "$rpm_path" > "$MUSL_ROOT/.payload.cpio" ||
            { printf 'rpm2cpio failed for %s\n' "$package" >&2; exit 1; }
        ( cd "$MUSL_ROOT" && cpio -idm --quiet < .payload.cpio ) ||
            { printf 'cpio could not unpack %s\n' "$package" >&2; exit 1; }
        rm -f "$MUSL_ROOT/.payload.cpio"
    fi
done

[ -f "$MUSL_PREFIX/lib64/libc.a" ] || {
    printf 'musl sysroot is incomplete: %s/lib64/libc.a missing\n' "$MUSL_PREFIX" >&2
    exit 1
}

# The specs file records the install prefix it was generated for.  Rewrite it
# so the unpacked copy stays usable wherever the cache lives.
LOCAL_SPECS="$MUSL_ROOT/local-musl-gcc.specs"
sed "s|/usr/x86_64-linux-musl|$MUSL_PREFIX|g" \
    "$MUSL_PREFIX/lib64/musl-gcc.specs" >"$LOCAL_SPECS"
MUSL_CC="$WORK_ROOT/musl-gcc"
cat >"$MUSL_CC" <<EOF
#!/bin/sh
exec "\${REALGCC:-gcc}" "\$@" -specs "$LOCAL_SPECS"
EOF
chmod 0755 "$MUSL_CC"
log "musl compiler: $MUSL_CC"

# --- 2. tcc -----------------------------------------------------------------

TCC_SRC="$SOURCE_CACHE/tinycc"
if [ ! -d "$TCC_SRC/.git" ]; then
    log "cloning tinycc"
    git clone --quiet "$TCC_URL" "$TCC_SRC"
fi
git -C "$TCC_SRC" fetch --quiet origin "$TCC_BRANCH" 2>/dev/null || true
git -C "$TCC_SRC" checkout --quiet "$TCC_COMMIT" || {
    printf 'cannot check out tcc commit %s\n' "$TCC_COMMIT" >&2
    exit 1
}

TCC_BUILD="$WORK_ROOT/tinycc-build"
rm -rf "$TCC_BUILD"
cp -a "$TCC_SRC" "$TCC_BUILD"
rm -rf "$TCC_BUILD/.git"

# The build-time include tree: tcc's own compiler headers (stdarg.h,
# stddef.h, ...) first, then musl's libc headers.  musl deliberately does not
# ship stdatomic.h or varargs.h, and tcc's versions are the ones its own
# libtcc1.a compiles against, so tcc's copy wins where both exist.
BUILD_INCLUDE="$WORK_ROOT/include"
mkdir -p "$BUILD_INCLUDE"
cp -a "$TCC_BUILD/include/." "$BUILD_INCLUDE/"
cp -a "$MUSL_PREFIX/include/." "$BUILD_INCLUDE/"

# Kernel UAPI headers are not part of musl.  They come from the host's
# kernel-headers package: they are the ABI the running kernel exposes, and
# each guest program that includes <linux/...> needs them.
for uapi in linux asm asm-generic mtd sound drm xen; do
    [ -d "/usr/include/$uapi" ] || continue
    cp -a "/usr/include/$uapi" "$BUILD_INCLUDE/"
done

log "building tcc at $TCC_COMMIT"
(
    cd "$TCC_BUILD"
    # The guest-side paths are the layout staged below, not the host's:
    #   {B}        the directory the guest is told about with -B
    #   /usr/lib   crt objects and libc.a as staged in the guest
    ./configure \
        --prefix=/usr \
        --cc="$MUSL_CC" \
        --config-musl \
        --extra-cflags="-O2 -static" \
        --extra-ldflags="-static" \
        --crtprefix='{B}:/usr/lib' \
        --libpaths='{B}:/usr/lib' \
        --sysincludepaths='{B}/include:/usr/include' \
        >"$WORK_ROOT/tcc-configure.log" 2>&1
    make -j"$JOBS" -s >"$WORK_ROOT/tcc-make.log" 2>&1 || {
        printf 'tcc build failed; see %s\n' "$WORK_ROOT/tcc-make.log" >&2
        tail -20 "$WORK_ROOT/tcc-make.log" >&2
        exit 1
    }
)

for artifact in tcc libtcc1.a libtcc.a; do
    [ -f "$TCC_BUILD/$artifact" ] || {
        printf 'tcc build did not produce %s\n' "$artifact" >&2
        exit 1
    }
done

# A guest tool that needs a dynamic loader would not run at all, so this is a
# build-time invariant rather than a documented expectation.
if readelf -d "$TCC_BUILD/tcc" 2>/dev/null | grep -q NEEDED; then
    printf 'tcc is dynamically linked; the guest has no dynamic loader\n' >&2
    exit 1
fi

# --- 3. stage ---------------------------------------------------------------
#
# Layout inside the guest:
#   /usr/bin/tcc                 the compiler
#   /usr/lib/tcc/                tcc's own runtime: libtcc1.a, libtcc.a, include/
#   /usr/lib/                    crt*.o and libc.a, where a program links them
#   /usr/include/                musl headers plus kernel UAPI headers
#
# tcc finds its support files through `-B/usr/lib/tcc`, and {B} in the paths
# configured above resolves there, so the crt objects and libc.a must be
# reachable from that directory too.  They are symlinked rather than copied:
# the same libc.a is needed at both paths, and the payload has to stay small
# enough for a modest image.
#
# The header tree is kept in exactly one place inside tcc's directory, which
# is the copy tcc actually reads, with /usr/include pointing at it.  A guest
# program that includes <stdio.h> and one built by tcc therefore see the same
# headers.

rm -rf "$OUTPUT"
mkdir -p "$OUTPUT/usr/bin" "$OUTPUT/usr/lib/tcc" "$OUTPUT/usr/include"
install -m 0755 "$TCC_BUILD/tcc" "$OUTPUT/usr/bin/tcc"
install -m 0644 "$TCC_BUILD/libtcc1.a" "$OUTPUT/usr/lib/tcc/libtcc1.a"
install -m 0644 "$TCC_BUILD/libtcc.a" "$OUTPUT/usr/lib/tcc/libtcc.a"
cp -a "$BUILD_INCLUDE/." "$OUTPUT/usr/lib/tcc/include/"
for object in crt1.o crti.o crtn.o libc.a; do
    install -m 0644 "$MUSL_PREFIX/lib64/$object" "$OUTPUT/usr/lib/$object"
    ln -s "../$object" "$OUTPUT/usr/lib/tcc/$object"
done
rmdir "$OUTPUT/usr/include"
# Relative to the link's own directory (/usr at the image root).
ln -s "lib/tcc/include" "$OUTPUT/usr/include"

printf 'build-guest-tools: staged %s payload in %s\n' "$PAYLOAD" "$OUTPUT" >&2
printf 'build-guest-tools: installed size %s\n' \
    "$(du -shL "$OUTPUT" | cut -f1)" >&2
