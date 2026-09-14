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
WORK_ROOT=$(mktemp -d "$STATE_ROOT/nested-payload-build.XXXXXX")
trap 'rm -rf "$WORK_ROOT"' EXIT

# --- 1. the pinned musl C compiler ----------------------------------------

# shellcheck source=scripts/lib/musl-host-compiler.sh
. "$SCRIPT_DIR/lib/musl-host-compiler.sh"

SYSROOT="$WORK_ROOT/sysroot"
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
EXE_WRAPPER="$WORK_ROOT/exe-wrapper.sh"
cat >"$EXE_WRAPPER" <<'EOF'
#!/bin/sh
# The target is x86_64-linux-musl and the build machine is x86_64-linux-gnu.
# Target binaries are static, so they execute natively on the build machine.
exec "$@"
EOF
chmod 0755 "$EXE_WRAPPER"

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

BUILD="$WORK_ROOT/build"
mkdir -p "$BUILD"

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
( cd "$BUILD" && tar xf "$zlib_tar" && cd "zlib-$ZLIB_VERSION" &&
  CC="$MUSL_CC" ./configure --static --prefix="$SYSROOT" >"$WORK_ROOT/zlib.log" 2>&1 &&
  make -j"$JOBS" >>"$WORK_ROOT/zlib.log" 2>&1 &&
  make install >>"$WORK_ROOT/zlib.log" 2>&1 ) || {
    printf 'zlib build failed; see %s\n' "$WORK_ROOT/zlib.log" >&2; tail -20 "$WORK_ROOT/zlib.log" >&2; exit 1; }

log "building libffi $LIBFFI_VERSION"
# libffi's trampoline needs the kernel UAPI headers, which musl does not ship
# and the specs' -nostdinc excludes; hence the explicit -isystem overlay.
( cd "$BUILD" && tar xf "$libffi_tar" && cd "libffi-$LIBFFI_VERSION" &&
  CC="$MUSL_CC" CPPFLAGS="-isystem $SYSROOT/kernel-uapi" ./configure \
      --build=x86_64-pc-linux-gnu --host=x86_64-linux-musl \
      --disable-shared --enable-static --disable-multi-os-directory \
      --prefix="$SYSROOT" >"$WORK_ROOT/libffi.log" 2>&1 &&
  make -j"$JOBS" >>"$WORK_ROOT/libffi.log" 2>&1 &&
  make install >>"$WORK_ROOT/libffi.log" 2>&1 ) || {
    printf 'libffi build failed; see %s\n' "$WORK_ROOT/libffi.log" >&2; tail -20 "$WORK_ROOT/libffi.log" >&2; exit 1; }

log "building pcre2 $PCRE2_VERSION"
( cd "$BUILD" && tar xf "$pcre2_tar" && cd "pcre2-$PCRE2_VERSION" &&
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
( cd "$BUILD" && tar xf "$glib_tar" )
GLIB_SRC="$BUILD/glib-$GLIB_VERSION"
GLIB_BUILD="$BUILD/glib-build"
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
ninja -C "$GLIB_BUILD" -j "$JOBS" >"$WORK_ROOT/glib-ninja.log" 2>&1 || {
    printf 'GLib build failed; see %s\n' "$WORK_ROOT/glib-ninja.log" >&2
    tail -30 "$WORK_ROOT/glib-ninja.log" >&2; exit 1; }
ninja -C "$GLIB_BUILD" install >"$WORK_ROOT/glib-install.log" 2>&1 || {
    printf 'GLib install failed; see %s\n' "$WORK_ROOT/glib-install.log" >&2
    tail -30 "$WORK_ROOT/glib-install.log" >&2; exit 1; }

# The exe_wrapper above is only correct if it really ran target binaries and
# GLib really got the answers it probed for.  A silent fallback here is the
# difference between GLib's own printf and a bundled replacement, so it is
# checked rather than assumed.
grep -q 'Checking if "C99 vsnprintf" runs: YES' "$WORK_ROOT/glib-setup.log" || {
    printf 'GLib did not run target binaries during setup: exe_wrapper is not working.\n' >&2
    printf 'Without it GLib compiles in its gnulib printf replacements.\n' >&2
    exit 1
}

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
( cd "$BUILD" && tar xf "$QEMU_TAR" )
QEMU_SRC="$BUILD/qemu-$QEMU_VERSION"
QEMU_BUILD="$WORK_ROOT/qemu-build"
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

# --- 5. the inner image ----------------------------------------------------

log "building the Phase 2a inner image"
HELLO_OUT="$BUILD/inner-image"
mkdir -p "$HELLO_OUT"
HELLO_ACPI_SHUTDOWN=1 "$REPO_ROOT/tools/nested/hello/build.sh" "$HELLO_OUT" \
    >"$WORK_ROOT/hello.log" 2>&1 || {
    printf 'inner image build failed; see %s\n' "$WORK_ROOT/hello.log" >&2
    tail -20 "$WORK_ROOT/hello.log" >&2; exit 1; }
[ -f "$HELLO_OUT/hello-acpi.elf" ] || {
    printf 'inner image build produced no hello-acpi.elf\n' >&2; exit 1; }

# --- 6. stage --------------------------------------------------------------

# Layout inside the guest.  Everything is under /opt/thekernel-tools so the
# payload is one subtree that can be inspected, hashed, and removed again.
rm -rf "$OUTPUT"
mkdir -p "$OUTPUT/opt/thekernel-tools/bin" "$OUTPUT/opt/thekernel-tools/payloads"
install -m 0755 "$QEMU_BINARY" "$OUTPUT/opt/thekernel-tools/bin/qemu-system-x86_64"
install -m 0644 "$HELLO_OUT/hello-acpi.elf" \
    "$OUTPUT/opt/thekernel-tools/payloads/hello-acpi.elf"

{
    printf '# nested guest tool payload\n'
    printf 'qemu %s\n' "$QEMU_VERSION"
    printf 'glib %s\n' "$GLIB_VERSION"
    printf 'zlib %s\n' "$ZLIB_VERSION"
    printf 'libffi %s\n' "$LIBFFI_VERSION"
    printf 'pcre2 %s\n' "$PCRE2_VERSION"
    printf '# path size sha256\n'
    for file in opt/thekernel-tools/bin/qemu-system-x86_64 \
                opt/thekernel-tools/payloads/hello-acpi.elf; do
        printf '%s %s %s\n' "$file" "$(stat -c %s "$OUTPUT/$file")" \
            "$(sha256sum "$OUTPUT/$file" | cut -d' ' -f1)"
    done
} >"$OUTPUT/opt/thekernel-tools/MANIFEST"

printf 'build-nested-payload: staged nested payload in %s\n' "$OUTPUT" >&2
printf 'build-nested-payload: installed size %s\n' \
    "$(du -shL "$OUTPUT" | cut -f1)" >&2
printf 'build-nested-payload: emulator %s\n' \
    "$(stat -c %s "$OUTPUT/opt/thekernel-tools/bin/qemu-system-x86_64")" >&2
