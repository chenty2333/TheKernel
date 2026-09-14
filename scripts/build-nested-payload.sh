#!/usr/bin/env bash
# Build the `nested` guest tool payload: a statically linked system emulator
# plus the boot images it runs, all staged for build-rootfs.sh to copy in.
#
# Everything here is a host cross-build against musl.  The guest runs the
# results and nothing else: no download, no package manager, no dynamic loader
# (TheKernel has none, so a dynamically linked emulator could not start at all).
#
# Build order and why it is this order:
#
#   1. zlib, libffi, pcre2  -- static dependencies of GLib
#   2. GLib                  -- QEMU's hard dependency; needs an exe_wrapper
#                               because it runs target programs during setup
#   3. QEMU system-mode      -- the emulator, --static, x86_64-softmmu only
#   4. the inner image       -- the freestanding kernel QEMU boots
#
# The GLib step is the one that makes this possible at all: upstream notes that
# most libraries QEMU's system emulation uses are unavailable for static
# linking, and GLib is the dependency that decides it.  It is available; see
# docs/design/guest-toolchain-and-nested-qemu.md for the measured evidence.
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)

# --- pins ------------------------------------------------------------------
#
# Every version is pinned, and every downloaded file is checked against the
# sha256 recorded here.  The GLib stack is not in the pre-existing source
# cache: it was fetched once and hashed, and the hash is what makes the pin
# meaningful rather than decorative.
GLIB_VERSION=2.88.3
GLIB_SHA256=ab24d24e698dfa1e408b7bcdb508f4aafc906185a8b8ce72fdf79bbbdc9b383b
ZLIB_VERSION=1.3.1
ZLIB_SHA256=9a93b2b7dfdac77ceba5a558a580e74667dd6fede4585b91eefb60f03b72df23
LIBFFI_VERSION=3.4.6
LIBFFI_SHA256=b0dea9df23c863a7a50e825440f3ebffabd65df1497108e5d437747843895a4e
PCRE2_VERSION=10.44
PCRE2_SHA256=86b9cb0aa3bcb7994faa88018292bc704cdbb708e785f7c74352ff6ea7d3175b
QEMU_VERSION=11.1.1
QEMU_SHA256=079ffbff8a7111bbc89022107cbabf3bbfd614d5fc9d7cc675991196aca12482

# The Fedora musl RPMs are shared with the `tcc` payload.
MUSL_RPM_RELEASE=1.2.5-6.fc44
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

# --- arguments -------------------------------------------------------------

OUTPUT=
SOURCE_CACHE=${THEKERNEL_SOURCE_CACHE:-$REPO_ROOT/.state/source-cache}
STATE_ROOT=${THEKERNEL_STATE_DIR:-$HOME/.cache/thekernel-targets}
JOBS=${CARGO_BUILD_JOBS:-$(nproc)}
# Where a pinned QEMU source tarball may already be cached, so a repeated build
# does not need the network for the largest download in this script.
QEMU_TARBALL=${THEKERNEL_QEMU_TARBALL:-}

usage() {
    cat <<'EOF'
Usage: scripts/build-nested-payload.sh --output DIR [options]

Build the `nested` guest tool payload (static musl QEMU + the inner boot image)
and stage it into DIR for build-rootfs.sh to copy into the image.

Options:
  --output DIR       staging tree to populate (required)
  --jobs N           parallel build jobs (default: $CARGO_BUILD_JOBS or nproc)
  -h, --help         show this help

Environment:
  THEKERNEL_SOURCE_CACHE  download cache (default: REPO/.state/source-cache)
  THEKERNEL_STATE_DIR     build state root (default: ~/.cache/thekernel-targets)
  CARGO_BUILD_JOBS        parallel build jobs
  THEKERNEL_QEMU_TARBALL  existing qemu-<version>.tar.xz to use instead of
                          downloading (checked against the pinned sha256)
  THEKERNEL_NESTED_VERBOSE=1  echo each build step

Staged layout inside the guest:
  /opt/thekernel-tools/bin/qemu-system-x86_64   the emulator
  /opt/thekernel-tools/payloads/hello-acpi.elf  the Phase 2a inner image
  /opt/thekernel-tools/MANIFEST                 what was staged, with hashes
EOF
}

while (($#)); do
    case "$1" in
        --output) OUTPUT=${2:-}; shift 2 ;;
        --jobs) JOBS=${2:-}; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) printf 'unknown argument: %s\n' "$1" >&2; exit 2 ;;
    esac
done

[ -n "$OUTPUT" ] || { printf '%s\n' '--output is required' >&2; exit 2; }
OUTPUT=$(realpath -m "$OUTPUT")

for command in curl gcc make meson ninja pkg-config tar rpm2cpio cpio sha256sum; do
    command -v "$command" >/dev/null 2>&1 || {
        printf 'required command not found: %s\n' "$command" >&2
        exit 1
    }
done

log() {
    [ "${THEKERNEL_NESTED_VERBOSE:-0}" = 1 ] && printf 'build-nested-payload: %s\n' "$*" >&2
    return 0
}

# The emulator this produces is copied into a rootfs image, and the image is
# only rebuilt when its inputs change -- which is decided by hashing the staged
# payload.  A build that embeds the wall clock therefore makes every run look
# like a change and rebuilds a 234 MiB image for nothing, so the embedded time
# is pinned.  This is not cosmetic: measured, two consecutive builds of the
# same QEMU sources differed in the binary without it.
export SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-1700000000}

# Fetch one pinned source tarball, verifying it every time -- including when it
# is already cached, so a corrupted cache entry is caught rather than used.
fetch_source() {
    local url=$1 file=$2 expected=$3
    local path="$SOURCE_CACHE/$file"
    if [ ! -f "$path" ]; then
        mkdir -p "$SOURCE_CACHE"
        log "downloading $file"
        curl --fail --location --silent --show-error --output "$path.part" "$url" || {
            rm -f "$path.part"
            printf 'cannot download %s\n  from %s\n' "$file" "$url" >&2
            exit 1
        }
        mv "$path.part" "$path"
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

# Copy a pinned RPM into the cache.  Kept here (rather than sourced) because
# dnf's failure mode is "exit 0 and download nothing", which has to be checked.
fetch_rpm() {
    local package=$1 file=$2 expected=$3
    local dir="$SOURCE_CACHE/rpm"
    local path="$dir/$file"
    if [ ! -f "$path" ]; then
        mkdir -p "$dir"
        log "downloading $file"
        ( cd "$dir" && dnf download --destdir . "$package" >/dev/null 2>&1 ) || true
        if [ ! -f "$path" ]; then
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

mkdir -p "$SOURCE_CACHE" "$STATE_ROOT"
# Logs and throwaway scratch only; nothing compiled here may reach the payload.
WORK_ROOT=$(mktemp -d "$STATE_ROOT/nested-payload-build.XXXXXX")
trap 'rm -rf "$WORK_ROOT"' EXIT

# The build tree lives at a fixed path, not under WORK_ROOT, and that is a
# correctness requirement rather than a convenience: the emulator embeds the
# directories it was compiled from (a "-isystem .../qemu-11.1.1/include/..."
# string per translation unit), so a build tree under a `mktemp -d` path makes
# the binary differ between two builds of identical sources -- measured: 1072
# differing bytes of .rodata, all of them that path.  A payload that differs on
# every build can never satisfy the image-reuse fingerprint, so every run would
# rebuild a 234 MiB image for nothing.  A fixed path makes the build
# reproducible, and as a side effect ninja reuses it, so a rebuild is a relink
# instead of ten minutes of compilation.
BUILD_ROOT="$STATE_ROOT/nested-build"
mkdir -p "$BUILD_ROOT/src" "$BUILD_ROOT/inner-image"

# --- 1. the pinned musl C compiler ----------------------------------------

# shellcheck source=scripts/lib/musl-host-compiler.sh
. "$SCRIPT_DIR/lib/musl-host-compiler.sh"

SYSROOT="$BUILD_ROOT/sysroot"
mkdir -p "$SYSROOT"

# Kernel UAPI headers are not part of musl.  QEMU, GLib and libffi all include
# <linux/...>, and musl's specs put the include search under -nostdinc, so the
# host's kernel headers are overlaid explicitly rather than leaking in.
for uapi in linux asm asm-generic mtd sound drm xen; do
    [ -d "/usr/include/$uapi" ] || continue
    mkdir -p "$SYSROOT/kernel-uapi"
    cp -a "/usr/include/$uapi" "$SYSROOT/kernel-uapi/"
done
log "kernel UAPI overlay: $SYSROOT/kernel-uapi"

# Target programs run natively here, and they are fully static, so meson's
# exe_wrapper is a passthrough.  Without it GLib's 22 cc.run() probes all fall
# back to "no" and it silently compiles in its gnulib printf replacements.
EXE_WRAPPER="$BUILD_ROOT/exe-wrapper.sh"
cat >"$EXE_WRAPPER" <<'EOF'
#!/bin/sh
# The target is x86_64-linux-musl and the build machine is x86_64-linux-gnu.
# Target binaries are static, so they execute natively on the build machine.
exec "$@"
EOF
chmod 0755 "$EXE_WRAPPER"

# The cross file is regenerated on every run, so it deliberately does *not*
# live in the build tree: ninja tracks it as an input to build.ninja, and a
# newer cross file therefore makes ninja regenerate the build files and re-run
# every meson configure check on each build -- which is slow, and on this host
# fails outright the second time.  It names only stable paths ($SYSROOT and the
# musl wrapper), and the payload is checked below for any trace of this
# per-run directory, so nothing from here can reach the emulator.
CROSS_FILE="$WORK_ROOT/musl-cross.ini"
cat >"$CROSS_FILE" <<EOF
# Meson cross file: x86_64-linux-musl, fully static.
[binaries]
c = '$MUSL_CC'
ar = '/usr/bin/ar'
strip = '/usr/bin/strip'
ranlib = '/usr/bin/ranlib'
pkg-config = '/usr/bin/pkg-config'
exe_wrapper = '$EXE_WRAPPER'

[host_machine]
system = 'linux'
cpu_family = 'x86_64'
cpu = 'x86_64'
endian = 'little'

[properties]
pkg_config_libdir = '$SYSROOT/lib/pkgconfig:$SYSROOT/share/pkgconfig'

[built-in options]
default_library = 'static'
c_args = ['-O2', '-isystem', '$SYSROOT/kernel-uapi']
c_link_args = ['-static', '-L$SYSROOT/lib']
EOF

export PKG_CONFIG_LIBDIR="$SYSROOT/lib/pkgconfig:$SYSROOT/share/pkgconfig"
export PKG_CONFIG_PATH="$PKG_CONFIG_LIBDIR"
export PKG_CONFIG_SYSROOT_DIR=

BUILD="$BUILD_ROOT/src"

# --- 2. zlib, libffi, pcre2 ------------------------------------------------

# zlib keeps superseded releases under /fossils; the top-level name only
# carries the current release, so the pinned version is fetched from there.
zlib_tar=$(fetch_source "https://zlib.net/fossils/zlib-${ZLIB_VERSION}.tar.gz" \
    "zlib-${ZLIB_VERSION}.tar.gz" "$ZLIB_SHA256")
pcre2_tar=$(fetch_source \
    "https://github.com/PCRE2Project/pcre2/releases/download/pcre2-${PCRE2_VERSION}/pcre2-${PCRE2_VERSION}.tar.gz" \
    "pcre2-${PCRE2_VERSION}.tar.gz" "$PCRE2_SHA256")
libffi_tar=$(fetch_source \
    "https://github.com/libffi/libffi/releases/download/v${LIBFFI_VERSION}/libffi-${LIBFFI_VERSION}.tar.gz" \
    "libffi-${LIBFFI_VERSION}.tar.gz" "$LIBFFI_SHA256")

log "building zlib $ZLIB_VERSION"
( cd "$BUILD" && [ -d "zlib-$ZLIB_VERSION" ] || tar xf "$zlib_tar"; cd "zlib-$ZLIB_VERSION" &&
  CC="$MUSL_CC" ./configure --static --prefix="$SYSROOT" >"$WORK_ROOT/zlib.log" 2>&1 &&
  make -j"$JOBS" >>"$WORK_ROOT/zlib.log" 2>&1 &&
  make install >>"$WORK_ROOT/zlib.log" 2>&1 ) || {
    printf 'zlib build failed; see %s\n' "$WORK_ROOT/zlib.log" >&2; tail -20 "$WORK_ROOT/zlib.log" >&2; exit 1; }

log "building libffi $LIBFFI_VERSION"
# libffi's trampoline needs the kernel UAPI headers, which musl does not ship
# and the specs' -nostdinc excludes; hence the explicit -isystem overlay.
( cd "$BUILD" && [ -d "libffi-$LIBFFI_VERSION" ] || tar xf "$libffi_tar"; cd "libffi-$LIBFFI_VERSION" &&
  CC="$MUSL_CC" CPPFLAGS="-isystem $SYSROOT/kernel-uapi" ./configure \
      --build=x86_64-pc-linux-gnu --host=x86_64-linux-musl \
      --disable-shared --enable-static --disable-multi-os-directory \
      --prefix="$SYSROOT" >"$WORK_ROOT/libffi.log" 2>&1 &&
  make -j"$JOBS" >>"$WORK_ROOT/libffi.log" 2>&1 &&
  make install >>"$WORK_ROOT/libffi.log" 2>&1 ) || {
    printf 'libffi build failed; see %s\n' "$WORK_ROOT/libffi.log" >&2; tail -20 "$WORK_ROOT/libffi.log" >&2; exit 1; }

log "building pcre2 $PCRE2_VERSION"
( cd "$BUILD" && [ -d "pcre2-$PCRE2_VERSION" ] || tar xf "$pcre2_tar"; cd "pcre2-$PCRE2_VERSION" &&
  CC="$MUSL_CC" ./configure --build=x86_64-pc-linux-gnu --host=x86_64-linux-musl \
      --disable-shared --enable-static --enable-pcre2-8 \
      --disable-pcre2-16 --disable-pcre2-32 \
      --prefix="$SYSROOT" >"$WORK_ROOT/pcre2.log" 2>&1 &&
  make -j"$JOBS" >>"$WORK_ROOT/pcre2.log" 2>&1 &&
  make install >>"$WORK_ROOT/pcre2.log" 2>&1 ) || {
    printf 'pcre2 build failed; see %s\n' "$WORK_ROOT/pcre2.log" >&2; tail -20 "$WORK_ROOT/pcre2.log" >&2; exit 1; }

# --- 3. GLib ---------------------------------------------------------------

glib_tar=$(fetch_source \
    "https://download.gnome.org/sources/glib/2.88/glib-${GLIB_VERSION}.tar.xz" \
    "glib-${GLIB_VERSION}.tar.xz" "$GLIB_SHA256")

log "building GLib $GLIB_VERSION"
( cd "$BUILD" && { [ -d "glib-$GLIB_VERSION" ] || tar xf "$glib_tar"; } )
GLIB_SRC="$BUILD/glib-$GLIB_VERSION"
GLIB_BUILD="$BUILD_ROOT/glib-build"
# Rebuilding the tree in place is what makes a repeat run cheap, so the setup
# step is skipped once it has succeeded.  Meson regenerates its build file on
# its own when ninja runs and something it tracks has changed, so this is a
# short-circuit, not a cache that can go stale: the install below fails if the
# build directory was not the one that produced the sysroot.
# The build tree is reused across runs, which is what makes a repeat build a
# relink instead of ten minutes of compilation.  Meson regenerates its build
# file when ninja runs and something it tracks has moved, and that
# regeneration re-runs every configure check: on this host the second run dies
# inside them with a meson "Unhandled python OSError".  The tree is configured
# here, by this script, and nothing else touches it, so once the marker below
# exists the tree is used as it stands.
GLIB_SETUP_OK="$GLIB_BUILD/.thekernel-setup-ok"
GLIB_NINJA_FLAGS=()
if [ -f "$GLIB_SETUP_OK" ] && [ -f "$GLIB_BUILD/build.ninja" ]; then
    log "reusing the GLib build tree in $GLIB_BUILD"
    GLIB_NINJA_FLAGS=(-d keeprsp)
else
rm -rf "$GLIB_BUILD"
meson setup "$GLIB_BUILD" "$GLIB_SRC" \
    --cross-file "$CROSS_FILE" \
    --buildtype release \
    --wrap-mode nodownload \
    -Dintrospection=disabled \
    -Dtests=false \
    -Ddocumentation=false \
    -Dman-pages=disabled \
    -Dselinux=disabled \
    -Dlibmount=disabled \
    -Ddtrace=disabled \
    -Dsystemtap=disabled \
    -Dglib_debug=enabled \
    -Dbsymbolic_functions=true \
    --prefix="$SYSROOT" >"$WORK_ROOT/glib-setup.log" 2>&1 || {
    printf 'GLib meson setup failed; see %s\n' "$WORK_ROOT/glib-setup.log" >&2
    tail -30 "$WORK_ROOT/glib-setup.log" >&2; exit 1; }
touch "$GLIB_SETUP_OK"
fi
ninja -C "$GLIB_BUILD" -j "$JOBS" "${GLIB_NINJA_FLAGS[@]}" \
    >"$WORK_ROOT/glib-ninja.log" 2>&1 || {
    printf 'GLib build failed; see %s\n' "$WORK_ROOT/glib-ninja.log" >&2
    tail -30 "$WORK_ROOT/glib-ninja.log" >&2; exit 1; }
ninja -C "$GLIB_BUILD" install >"$WORK_ROOT/glib-install.log" 2>&1 || {
    printf 'GLib install failed; see %s\n' "$WORK_ROOT/glib-install.log" >&2
    tail -30 "$WORK_ROOT/glib-install.log" >&2; exit 1; }

# The exe_wrapper above is only correct if it really ran target binaries and
# GLib really got the answers it probed for.  A silent fallback here is the
# difference between GLib's own printf and a bundled replacement, so it is
# checked rather than assumed.
if [ -f "$WORK_ROOT/glib-setup.log" ]; then
    grep -q 'Checking if "C99 vsnprintf" runs: YES' "$WORK_ROOT/glib-setup.log" || {
        printf 'GLib did not run target binaries during setup: exe_wrapper is not working.\n' >&2
        printf 'Without it GLib compiles in its gnulib printf replacements.\n' >&2
        exit 1
    }
else
    # A reused build tree has no setup log; read the answer back out of the
    # generated configuration instead of trusting that it was ever right.
    grep -q 'define HAVE_C99_VSNPRINTF 1' "$GLIB_BUILD/config.h" || {
        printf 'the reused GLib build tree reports no working C99 vsnprintf;\n' >&2
        printf 'it was configured without a working exe_wrapper.\n' >&2
        exit 1
    }
fi

# Meson records -latomic in glib-2.0.pc.  The only libatomic.a reachable on
# this host is the one Fedora built against glibc, and linking that into a musl
# binary produces a program that dies at run time with no diagnostic.  The GLib
# stack has no 16-byte atomic references, so the flag is removed: a real
# requirement then fails loudly at link time instead of silently at run time.
GLIB_PC="$SYSROOT/lib/pkgconfig/glib-2.0.pc"
if grep -q -- '-latomic' "$GLIB_PC"; then
    log "removing -latomic from glib-2.0.pc (glibc libatomic is not linkable into musl)"
    sed -i 's/ -latomic//g' "$GLIB_PC"
fi

# A dynamically linked dependency would defeat the whole point: it must not be
# present in the sysroot at all.
if find "$SYSROOT/lib" -name 'libglib-2.0.so*' -o -name 'libpcre2-8.so*' | grep -q .; then
    printf 'the GLib sysroot contains shared libraries; QEMU must link them statically\n' >&2
    exit 1
fi

# --- 4. QEMU ---------------------------------------------------------------

if [ -n "$QEMU_TARBALL" ]; then
    QEMU_TAR=$(realpath -e "$QEMU_TARBALL")
    printf '%s\n' "$QEMU_SHA256  $QEMU_TAR" | sha256sum --check --status || {
        printf 'pinned QEMU tarball does not match the recorded sha256: %s\n' "$QEMU_TAR" >&2
        exit 1
    }
else
    QEMU_TAR=$(fetch_source "https://download.qemu.org/qemu-${QEMU_VERSION}.tar.xz" \
        "qemu-${QEMU_VERSION}.tar.xz" "$QEMU_SHA256")
fi

log "building QEMU $QEMU_VERSION"
( cd "$BUILD" && { [ -d "qemu-$QEMU_VERSION" ] || tar xf "$QEMU_TAR"; } )
QEMU_SRC="$BUILD/qemu-$QEMU_VERSION"
QEMU_BUILD="$BUILD_ROOT/qemu-build"
mkdir -p "$QEMU_BUILD"

# QEMU's configure generates the meson machine file itself from the compiler
# and flags given here; --cross-file is not a configure option.
#
# The flags that matter, and why:
#   --static            link everything in; the guest has no dynamic loader
#   -fno-link-libatomic GCC >= 16 links libatomic by itself for 16-byte
#                       atomics, and the only one here is glibc's
#   -isystem kernel-uapi  <linux/...> is not part of musl
#   --without-default-features   drop every optional dependency (no pixman,
#                       gtk, sdl, gio, dbus, ...), none of which are available
#                       statically
#   --without-default-devices    only the devices QEMU's own x86_64 set needs
#   --target-list=x86_64-softmmu system emulation only, no user emulation
( cd "$QEMU_BUILD" && "$QEMU_SRC/configure" \
    --cc="$MUSL_CC" \
    --target-list=x86_64-softmmu \
    --without-default-features \
    --static \
    --prefix=/usr \
    --extra-cflags="-O2 -fno-link-libatomic -isystem $SYSROOT/kernel-uapi" \
    --extra-ldflags="-static" \
    >"$WORK_ROOT/qemu-configure.log" 2>&1 ) || {
    printf 'QEMU configure failed; see %s\n' "$WORK_ROOT/qemu-configure.log" >&2
    tail -30 "$WORK_ROOT/qemu-configure.log" >&2; exit 1; }

# GLib came from this sysroot and nowhere else: if the host's glib had been
# found instead, the link would silently pull in glibc.
grep -q "glib-2.0 found: YES $GLIB_VERSION" "$WORK_ROOT/qemu-configure.log" || {
    printf 'QEMU did not find GLib %s from the musl sysroot; see %s\n' \
        "$GLIB_VERSION" "$WORK_ROOT/qemu-configure.log" >&2
    exit 1
}

ninja -C "$QEMU_BUILD" -j "$JOBS" >"$WORK_ROOT/qemu-ninja.log" 2>&1 || {
    printf 'QEMU build failed; see %s\n' "$WORK_ROOT/qemu-ninja.log" >&2
    tail -30 "$WORK_ROOT/qemu-ninja.log" >&2; exit 1; }

QEMU_BINARY="$QEMU_BUILD/qemu-system-x86_64"
[ -x "$QEMU_BINARY" ] || { printf 'QEMU build produced no %s\n' "$QEMU_BINARY" >&2; exit 1; }

# The payload is copied into a rootfs image, where debug info is dead weight:
# 25 MiB of the unstripped binary.  --enable-strip does not remove it for this
# build, so the strip is explicit.  Stripping cannot change what the binary
# requires, which the two invariant checks below then confirm on the result.
log "stripping the emulator"
strip --strip-unneeded "$QEMU_BINARY" || {
    printf 'strip failed on %s\n' "$QEMU_BINARY" >&2; exit 1; }

# QEMU loads its firmware blobs from a data directory found relative to the
# binary.  A bare binary therefore fails with "could not load PC BIOS
# 'bios-256k.bin'", so the data directory is staged beside it and the
# emulator is run to prove the pairing works.
DATA_DIR="$BUILD_ROOT/install"
rm -rf "$DATA_DIR"
# DESTDIR must be an environment variable, not an argument: `ninja install
# DESTDIR=x` is read by ninja as a target named `DESTDIR=x`.
DESTDIR="$DATA_DIR" ninja -C "$QEMU_BUILD" install >"$WORK_ROOT/qemu-install.log" 2>&1 || {
    printf 'QEMU install failed; see %s\n' "$WORK_ROOT/qemu-install.log" >&2
    tail -30 "$WORK_ROOT/qemu-install.log" >&2; exit 1; }
QEMU_SHARE="$DATA_DIR/usr/share/qemu"
[ -f "$QEMU_SHARE/bios-256k.bin" ] || {
    printf 'QEMU install staged no bios-256k.bin under %s\n' "$QEMU_SHARE" >&2; exit 1; }

# Two invariants the guest depends on.  Both have failed silently in the wild:
# a glibc-libatomic link produces a binary that runs and then dies, and a
# dynamic binary produces one that cannot start at all.
if readelf -d "$QEMU_BINARY" 2>/dev/null | grep -q NEEDED; then
    printf 'qemu-system-x86_64 is dynamically linked; the guest has no dynamic loader\n' >&2
    exit 1
fi
if nm "$QEMU_BINARY" 2>/dev/null | grep -q ' libat_'; then
    printf 'qemu-system-x86_64 linked glibc libatomic (%s); it would die at run time\n' \
        "$(nm "$QEMU_BINARY" | grep -m1 ' libat_' | awk '{print $3}')" >&2
    exit 1
fi

# --- 5. the Alpine inner OS (Phase 2b) -------------------------------------
#
# Phase 2a boots a freestanding kernel; this stage adds a real distribution.
# The value is in what it does *not* rely on: the kernel and userland are a
# pinned upstream release, so "a Linux distribution runs in TheKernel's
# userspace" is a claim about Alpine and its kernel rather than about code
# written for this test.
#
# The distribution's own initramfs is unusable here and this is worth
# recording, because the failure looks like a kernel problem and is not one.
# With no disk, no CD-ROM and no network, `initramfs-virt` cannot find the
# media it needs to mount `modloop-virt` and drops to an emergency shell.  The
# kernel boots fine -- `-kernel` and an external `-initrd` both work -- so the
# fix is a minimal initramfs built from the release's own minirootfs.

ALPINE_RELEASE=3.24.1
ALPINE_ARCH=x86_64
ALPINE_SERIES=v${ALPINE_RELEASE%.*}
ALPINE_MINIROOTFS="alpine-minirootfs-${ALPINE_RELEASE}-${ALPINE_ARCH}.tar.gz"
ALPINE_MINIROOTFS_SHA256=41f73e3cf5fa919b8aa5ca6b30dc48f0da2720776d7423e2a7748211456fe081
ALPINE_NETBOOT="alpine-netboot-${ALPINE_RELEASE}-${ALPINE_ARCH}.tar.gz"
ALPINE_NETBOOT_SHA256=9a7769ea8fa1737b1b49d82f1bdd53d0a17338d6d3b7cfc6f2c3ec5158596d8b
ALPINE_REPO="https://dl-cdn.alpinelinux.org/alpine/${ALPINE_SERIES}/releases/${ALPINE_ARCH}"
# The kernel inside the pinned netboot tarball.  Pinning the tarball and taking
# this path out of it is deliberate: the v3.24 *package repository* rotates and
# now serves a different kernel build than the one in this release tarball, so
# the release artifacts are the immutable input and the repository is not.
ALPINE_KERNEL_IN_TARBALL=boot/vmlinuz-virt

log "staging Alpine $ALPINE_RELEASE (Phase 2b)"
alpine_minirootfs=$(fetch_source "$ALPINE_REPO/$ALPINE_MINIROOTFS" \
    "$ALPINE_MINIROOTFS" "$ALPINE_MINIROOTFS_SHA256")
# 374 MiB, and only the kernel is taken out of it.  It is fetched once into the
# source cache and reused from there.
alpine_netboot=$(fetch_source "$ALPINE_REPO/$ALPINE_NETBOOT" \
    "$ALPINE_NETBOOT" "$ALPINE_NETBOOT_SHA256")

mkdir -p "$BUILD_ROOT/alpine"
tar -xzf "$alpine_netboot" -C "$BUILD_ROOT/alpine" "$ALPINE_KERNEL_IN_TARBALL"
ALPINE_KERNEL="$BUILD_ROOT/alpine/$ALPINE_KERNEL_IN_TARBALL"
[ -s "$ALPINE_KERNEL" ] || {
    printf 'the Alpine tarball contained no %s\n' "$ALPINE_KERNEL_IN_TARBALL" >&2
    exit 1; }

ALPINE_INITRAMFS="$BUILD_ROOT/alpine/inner-initramfs.cpio.gz"
MINIROOTFS_TARBALL="$alpine_minirootfs" WORKDIR="$WORK_ROOT/alpine-initramfs" \
    "$REPO_ROOT/tools/nested/alpine/build-initramfs.sh" "$ALPINE_INITRAMFS" \
    >"$WORK_ROOT/alpine-initramfs.log" 2>&1 || {
    printf 'the Alpine initramfs build failed; see %s\n' \
        "$WORK_ROOT/alpine-initramfs.log" >&2
    tail -20 "$WORK_ROOT/alpine-initramfs.log" >&2; exit 1; }
[ -s "$ALPINE_INITRAMFS" ] || {
    printf 'the Alpine initramfs build produced nothing\n' >&2; exit 1; }

# --- 6. the inner image ----------------------------------------------------

log "building the Phase 2a inner image"
HELLO_OUT="$BUILD_ROOT/inner-image"
mkdir -p "$HELLO_OUT"
HELLO_ACPI_SHUTDOWN=1 "$REPO_ROOT/tools/nested/hello/build.sh" "$HELLO_OUT" \
    >"$WORK_ROOT/hello.log" 2>&1 || {
    printf 'inner image build failed; see %s\n' "$WORK_ROOT/hello.log" >&2
    tail -20 "$WORK_ROOT/hello.log" >&2; exit 1; }
[ -f "$HELLO_OUT/hello-acpi.elf" ] || {
    printf 'inner image build produced no hello-acpi.elf\n' >&2; exit 1; }

# --- 6. stage --------------------------------------------------------------

# `nested` is a superset of `tcc`, and the guest suite for a nested image
# contains the compiler case because of it.  Building on top of the tcc stage
# is what makes that true; an earlier version cleared the output directory
# here and staged only the emulator, which left /usr/bin/tcc missing and the
# image reporting a failure for a case its own plan promised.
log "staging the tcc payload this one is a superset of"
"$REPO_ROOT/scripts/build-guest-tools.sh" --payload tcc --output "$OUTPUT" \
    >"$WORK_ROOT/tcc-stage.log" 2>&1 || {
    printf 'staging the tcc payload failed; see %s\n' "$WORK_ROOT/tcc-stage.log" >&2
    tail -20 "$WORK_ROOT/tcc-stage.log" >&2; exit 1; }
[ -x "$OUTPUT/usr/bin/tcc" ] || {
    printf 'the tcc stage produced no %s\n' "$OUTPUT/usr/bin/tcc" >&2; exit 1; }

# Layout inside the guest.  The compiler lives where the tcc payload put it;
# everything this script adds is under /opt/thekernel-tools, so the emulator
# part of the payload is one subtree that can be inspected and hashed.
mkdir -p "$OUTPUT/opt/thekernel-tools/bin" "$OUTPUT/opt/thekernel-tools/payloads"
install -m 0755 "$QEMU_BINARY" "$OUTPUT/opt/thekernel-tools/bin/qemu-system-x86_64"
# Only the x86 firmware is staged.  A full install is 316 MiB because it
# carries the EDK2 blobs for aarch64, arm, loongarch64 and riscv, none of which
# this emulator can even select -- it is built for x86_64-softmmu alone, and
# the payload is copied into a fixed-size rootfs image.  The allowlist below is
# every firmware file the pinned QEMU can ask for on x86; a file that is
# renamed or added upstream is caught by the emulator smoke test at the end of
# this script rather than by a silent fallback.
QEMU_DATA="$OUTPUT/opt/thekernel-tools/share"
mkdir -p "$QEMU_DATA"
# -L, not the default search path: the payload lives under /opt, and the data
# directory must be found the same way in the guest as it is here.
FIRMWARE_FILES=(
    # SeaBIOS: the hard requirement for pc/q35, measured.  The other two are
    # the microvm and 128 KiB variants.
    bios-256k.bin bios.bin bios-microvm.bin
    # APIC virtualisation, loaded by pc/q35 when the CPU supports it.
    kvmvapic.bin
    # -kernel direct boot with a PVH or multiboot note.
    pvh.bin multiboot_dma.bin linuxboot_dma.bin
    # VGA option ROMs.  Unused by Stage 2a's -display none, but reachable by
    # any run that does not pass it.
    vgabios.bin vgabios-stdvga.bin vgabios-cirrus.bin vgabios-qxl.bin
    vgabios-virtio.bin vgabios-vmware.bin vgabios-bochs-display.bin
    vgabios-ramfb.bin vgabios-ati.bin QEMU,cgthree.bin QEMU,tcx.bin
    qemu_vga.ndrv
    # Network boot ROMs.  Stage 2a boots with -nodefaults and never consults
    # them, but leaving them out was measured to break a plain `-machine pc`
    # run outright: "failed to find romfile "efi-e1000.rom"".  The emulator
    # is meant to be usable, not only usable by our own command line, so a
    # configuration that a reader would reasonably try must not fail on a
    # missing data file.
    pxe-e1000.rom pxe-eepro100.rom pxe-ne2k_pci.rom pxe-pcnet.rom
    pxe-rtl8139.rom pxe-virtio.rom
    efi-e1000.rom efi-e1000e.rom efi-eepro100.rom efi-ne2k_pci.rom
    efi-pcnet.rom efi-rtl8139.rom efi-virtio.rom efi-vmxnet3.rom
    # x86 UEFI, selected through the firmware descriptors copied below.
    edk2-i386-code.fd edk2-i386-vars.fd edk2-i386-secure-code.fd
    edk2-x86_64-code.fd edk2-x86_64-vars.fd edk2-x86_64-secure-code.fd
)
staged_firmware=0
for file in "${FIRMWARE_FILES[@]}"; do
    # A missing optional blob is not fatal; a missing bios-256k.bin is caught
    # both above and by the smoke test.
    [ -f "$QEMU_SHARE/$file" ] || continue
    install -m 0644 "$QEMU_SHARE/$file" "$QEMU_DATA/$file"
    staged_firmware=$((staged_firmware + 1))
done
# The device tree blobs and the firmware descriptor directory are small and are
# consulted by QEMU's firmware auto-selection.
cp -a "$QEMU_SHARE/dtb" "$QEMU_DATA/dtb"
cp -a "$QEMU_SHARE/firmware" "$QEMU_DATA/firmware"
# The license text is staged because the payload redistributes the blobs.
[ -f "$QEMU_SHARE/edk2-licenses.txt" ] &&
    install -m 0644 "$QEMU_SHARE/edk2-licenses.txt" "$QEMU_DATA/edk2-licenses.txt"
log "staged $staged_firmware x86 firmware blobs into $QEMU_DATA"
install -m 0644 "$HELLO_OUT/hello-acpi.elf" \
    "$OUTPUT/opt/thekernel-tools/payloads/hello-acpi.elf"
install -m 0644 "$ALPINE_KERNEL" \
    "$OUTPUT/opt/thekernel-tools/payloads/vmlinuz-virt"
install -m 0644 "$ALPINE_INITRAMFS" \
    "$OUTPUT/opt/thekernel-tools/payloads/alpine-initramfs.cpio.gz"

{
    printf '# nested guest tool payload\n'
    printf 'qemu %s\n' "$QEMU_VERSION"
    printf 'alpine %s\n' "$ALPINE_RELEASE"
    printf 'glib %s\n' "$GLIB_VERSION"
    printf 'zlib %s\n' "$ZLIB_VERSION"
    printf 'libffi %s\n' "$LIBFFI_VERSION"
    printf 'pcre2 %s\n' "$PCRE2_VERSION"
    printf '# path size sha256\n'
    for file in opt/thekernel-tools/bin/qemu-system-x86_64 \
                opt/thekernel-tools/payloads/hello-acpi.elf \
                opt/thekernel-tools/payloads/vmlinuz-virt \
                opt/thekernel-tools/payloads/alpine-initramfs.cpio.gz; do
        printf '%s %s %s\n' "$file" "$(stat -c %s "$OUTPUT/$file")" \
            "$(sha256sum "$OUTPUT/$file" | cut -d' ' -f1)"
    done
} >"$OUTPUT/opt/thekernel-tools/MANIFEST"

# Run the staged emulator on the staged image, from the staged paths.  This is
# the only check that covers the pairing of binary, data directory and inner
# image, and it is cheap: the inner boot is well under a second.  A payload
# that cannot pass its own smoke test is not worth copying into an image.
#
# The run deliberately keeps QEMU's default device set: VGA and a NIC, and
# therefore the option ROMs for both.  Booting with -nodefaults, which is what
# the guest case does, was measured to pass against a data directory that a
# plain run rejects with `failed to find romfile "efi-e1000.rom"`.  Passing
# only our own command line would have certified a payload that breaks the
# moment anyone types a different one.
log "smoke-testing the staged emulator with the default device set"
if ! timeout 120 "$OUTPUT/opt/thekernel-tools/bin/qemu-system-x86_64" \
        -L "$OUTPUT/opt/thekernel-tools/share" \
        -accel tcg -m 256 \
        -kernel "$OUTPUT/opt/thekernel-tools/payloads/hello-acpi.elf" \
        -display none -serial stdio -no-reboot \
        >"$WORK_ROOT/emulator-smoke.log" 2>&1; then
    printf 'the staged emulator did not exit 0 on its own inner image; see %s\n' \
        "$WORK_ROOT/emulator-smoke.log" >&2
    tail -20 "$WORK_ROOT/emulator-smoke.log" >&2
    exit 1
fi
# An allowlist can be incomplete in a way that only shows up later, on a
# machine or a device the smoke test does not use.  QEMU names every data file
# it wanted and could not find, so a staged payload with any such complaint is
# treated as incomplete.  Checked for every boot below, not just this one.
check_data_files() {
    local log=$1
    if grep -q 'could not load\|Could not open\|failed to find' "$log"; then
        printf 'the staged emulator could not find data files it needs:\n' >&2
        grep 'could not load\|Could not open\|failed to find' "$log" >&2
        exit 1
    fi
}
check_data_files "$WORK_ROOT/emulator-smoke.log"
grep -q '^INNER_HELLO_OK$' "$WORK_ROOT/emulator-smoke.log" || {
    printf 'the staged emulator produced no inner banner; see %s\n' \
        "$WORK_ROOT/emulator-smoke.log" >&2
    exit 1
}

# And the same emulator has to boot the Alpine image, which is a much larger
# claim: a real distribution kernel with its own userland, reached through the
# payload's own kernel, initramfs and data directory.  The marker set is the
# pass condition, exactly as the guest case will read it.
log "smoke-testing the Alpine inner boot"
if ! timeout 300 "$OUTPUT/opt/thekernel-tools/bin/qemu-system-x86_64" \
        -L "$OUTPUT/opt/thekernel-tools/share" \
        -machine pc -accel tcg -m 256 -smp 1 \
        -display none -serial stdio -no-reboot -vga none -nic none \
        -kernel "$OUTPUT/opt/thekernel-tools/payloads/vmlinuz-virt" \
        -initrd "$OUTPUT/opt/thekernel-tools/payloads/alpine-initramfs.cpio.gz" \
        -append "console=ttyS0 rdinit=/init quiet" \
        < /dev/null >"$WORK_ROOT/alpine-smoke.log" 2>&1; then
    printf 'the staged emulator did not exit 0 on the Alpine image; see %s\n' \
        "$WORK_ROOT/alpine-smoke.log" >&2
    tail -20 "$WORK_ROOT/alpine-smoke.log" >&2
    exit 1
fi
check_data_files "$WORK_ROOT/alpine-smoke.log"
# Markers with a value are matched with their separator; the bare ones are
# matched as whole lines.  `-a` because the inner console mixes the kernel's
# own line endings with the ones the init script writes.
for marker in "INNER_ALPINE_RELEASE " "INNER_KERNEL " "INNER_USERLAND_OK" \
              "INNER_SHUTDOWN_BEGIN" "INNER_INIT_PID 1"; do
    grep -qa "^$marker" "$WORK_ROOT/alpine-smoke.log" || {
        printf 'the Alpine boot produced no "%s" marker; see %s\n' \
            "$marker" "$WORK_ROOT/alpine-smoke.log" >&2
        tail -20 "$WORK_ROOT/alpine-smoke.log" >&2
        exit 1
    }
done

# Reproducibility is a property this build can check cheaply, and the check is
# worth its cost: if the emulator is not reproducible, the image-reuse decision
# never converges and every run rebuilds the image for no reason.  Rebuilding
# QEMU here recompiles nothing -- ninja sees an unchanged tree -- so this is a
# link and a strip.
log "checking that the emulator build is reproducible"
REPRO_CHECK="$BUILD_ROOT/repro-check"
mkdir -p "$REPRO_CHECK"
if ! ninja -C "$QEMU_BUILD" qemu-system-x86_64 >"$WORK_ROOT/repro-ninja.log" 2>&1; then
    printf 'the reproducibility re-link failed; see %s\n' "$WORK_ROOT/repro-ninja.log" >&2
    exit 1
fi
install -m 0755 "$QEMU_BINARY" "$REPRO_CHECK/qemu-system-x86_64"
strip --strip-unneeded "$REPRO_CHECK/qemu-system-x86_64"
# The per-run scratch directory must not appear in the emulator.  It would
# make two builds differ, and it is the kind of thing that gets in through a
# flag nobody looks at twice.
if grep -qa "$WORK_ROOT" "$OUTPUT/opt/thekernel-tools/bin/qemu-system-x86_64"; then
    printf 'the emulator embeds the per-run build directory %s;\n' "$WORK_ROOT" >&2
    printf 'it would differ between builds. Move whatever names it to a stable path.\n' >&2
    exit 1
fi
if ! cmp -s "$REPRO_CHECK/qemu-system-x86_64" \
        "$OUTPUT/opt/thekernel-tools/bin/qemu-system-x86_64"; then
    printf 'the emulator is not reproducible: two builds of the same sources differ.\n' >&2
    printf 'Every rootfs build would then see a changed payload and rebuild the image.\n' >&2
    printf 'Pin whatever varies (SOURCE_DATE_EPOCH is already set to %s).\n' \
        "$SOURCE_DATE_EPOCH" >&2
    exit 1
fi

printf 'build-nested-payload: staged nested payload in %s\n' "$OUTPUT" >&2
printf 'build-nested-payload: installed size %s\n' \
    "$(du -shL "$OUTPUT" | cut -f1)" >&2
printf 'build-nested-payload: emulator %s\n' \
    "$(stat -c %s "$OUTPUT/opt/thekernel-tools/bin/qemu-system-x86_64")" >&2
