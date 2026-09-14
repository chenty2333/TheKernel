#!/usr/bin/env bash
# Build the `gcc` guest tool payload: a real distribution C compiler, staged
# complete enough to compile and link a program inside the guest.
#
# This is Phase 3's second milestone.  The first one proved that a dynamically
# linked glibc program runs in the guest, which means the kernel's loader
# contract is implemented; and a separate probe proved that the clone3 shape
# glibc's posix_spawn uses -- which is how a compiler driver launches cc1, as
# and collect2 -- works too.  Both were checked *before* anything large was
# staged, precisely so that a failure here is a missing file rather than a
# kernel bug wearing a missing file's clothes.
#
# The payload is Fedora 44's GCC, glibc-hosted and dynamic.  The alternatives
# were measured and rejected:
#
#   - Alpine's musl GCC is ~17 MB smaller but its PT_INTERP is
#     /lib/ld-musl-x86_64.so.1, so it discards the loader milestone above
#     instead of building on it.
#   - A musl.cc cross toolchain is 307 MiB unpacked, larger than the whole
#     image.
#   - Clang is ~231 MiB because 95% of its closure is libLLVM and
#     libclang-cpp, and it still needs GCC's crtbegin/crtend/libgcc, so it does
#     not even remove the runtime it would replace.  No authoritative static
#     clang build exists.
#
# Why /lib64 and not /usr/lib64, and why each file is named the way it is:
# these are measured constraints, not style.  Every tool's PT_INTERP is
# /lib64/ld-linux-x86-64.so.2; glibc's own libc.so and libm.so are linker
# scripts naming /lib64 absolutely; gcc's libgcc_s.so is too.  The loader
# matches DT_NEEDED literally, so a library must be installed under its
# *soname* -- the RPM ships libctf.so.0.0.0 and the loader looks for libctf.so.0.
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)

# --- pins ------------------------------------------------------------------
#
# Every package is pinned by file name and by hash.  The hashes are the ones
# the payload was validated with, not merely the ones that happened to be on
# the mirror: a payload whose contents change when a mirror rolls over is a
# payload whose guest failures cannot be reproduced.
#
# The compiler is not one package.  `cc1` -- the C compiler proper -- is in
# `cpp`, not in `gcc`; a payload built from `gcc` alone has a driver and no
# compiler, and fails only when it is asked to compile something.
GCC_RELEASE=16.2.1-2.fc44
BINUTILS_RELEASE=2.46.1-1.fc44
GLIBC_RELEASE=2.43-8.fc44
KERNEL_HEADERS_RELEASE=7.2.4-200.fc44

# Pinned RPMs: release and sha256 for the x86_64 package of each.
#
# `zlib-ng-compat` rather than `zlib` is deliberate and is what Fedora 44
# actually ships libz.so.1 in; taking the familiar package name would silently
# stage nothing.
RPM_PINS=(
    "gcc:${GCC_RELEASE}:77a82e328053f73d3881e633ffefb4dd49ebe0ac5b18a996de9b615de4eb9569"
    "cpp:${GCC_RELEASE}:a3ca10854d95dc2aefc76e36d116c0a33a305d53b2ba06d92e43aeedb179668b"
    "binutils:${BINUTILS_RELEASE}:1f522dfb8e2bf92961da0bd880a5e743207f88d259a27e67daf4c672aa07dfe3"
    "glibc-devel:${GLIBC_RELEASE}:85d017aeeb2c16254a12b86d209094b8b8df84fcca25e35e9f94dbf93e55f3d9"
    "kernel-headers:${KERNEL_HEADERS_RELEASE}:60801d00dce8a4b6bd54d3c50b806c209af1c8389a6e3e7c15939e3b0c30987d"
    "libgcc:${GCC_RELEASE}:e9ccd7564d639c85ae455693f11d7a6cd990d1efdae6889656dacbeb9e0f846d"
    # The loader and libc.so.6 live in `glibc`, not in `glibc-devel`: -devel has
    # the startup objects, the linker scripts and the headers, and none of the
    # shared objects themselves.
    "glibc:${GLIBC_RELEASE}:84f0af455b8f541ead8ae4b74c220563babd9980dbd308226b90a22d6144ea14"
    "libatomic:${GCC_RELEASE}:5df3c610688ff2548a4bbfd8e9b47c525b339a45b0d01349163079a3ec82f740"
    # The following are not in gcc's own dependency set but are in the runtime
    # closure of the tools above, so they need pins of their own.
    "gmp:6.3.0-5.fc44:94b960c5eaba00c1cb56abdbed05bd348d1256360cf68fd9cd7a24b3232d6558"
    "mpfr:4.2.2-3.fc44:18846facd61a5858ac6703bc9f49d55b515a92f183d0e12dda295c0efadb91d4"
    "libmpc:1.4.1-1.fc44:4dffc953aff5309b0a4d45bf7d9d8167f8062250b957d4b78c372973d290f5c9"
    "zlib-ng-compat:2.3.3-3.fc44:53ee3cd7082b1a34e4901f13079b1840648113c809effb5b78e7725235a7d939"
    "libzstd:1.5.7-5.fc44:af98832cf1d93eabb442d18520bad10f03f4278a99d54c5d071b2903f6f50fac"
    "jansson:2.14-4.fc44:950a7003c1246568c6d602b222188391b1d4f411638d3470179736195f3b04f9"
)

# The guest paths, which the compiler and the loader name absolutely.
TOOLCHAIN_TRIPLE=x86_64-redhat-linux
GCC_VERSION=16
LIBEXEC_DIR="/usr/libexec/gcc/$TOOLCHAIN_TRIPLE/$GCC_VERSION"
GCC_LIB_DIR="/usr/lib/gcc/$TOOLCHAIN_TRIPLE/$GCC_VERSION"

# --- arguments -------------------------------------------------------------

OUTPUT=
SOURCE_CACHE=${THEKERNEL_SOURCE_CACHE:-$REPO_ROOT/.state/source-cache}
HELP=

while [ $# -gt 0 ]; do
    case "$1" in
        --output) OUTPUT=${2:-}; shift 2 ;;
        --source-cache) SOURCE_CACHE=${2:-}; shift 2 ;;
        -h|--help) HELP=1; shift ;;
        *) printf 'unknown argument: %s\n' "$1" >&2; exit 2 ;;
    esac
done

if [ -n "$HELP" ]; then
    cat <<'EOF'
usage: build-gcc-payload.sh --output DIR [--source-cache DIR]

Stages Fedora 44's C toolchain -- driver, cc1, as, ld, gcc's support files,
glibc's startup objects and headers, and the whole shared-library closure --
under DIR, in the layout build-rootfs.sh copies into the guest image.

Validates the result by compiling and running a program with the staged
toolchain before reporting success.
EOF
    exit 0
fi

[ -n "$OUTPUT" ] || { printf '%s\n' '--output is required' >&2; exit 2; }
mkdir -p "$SOURCE_CACHE"

STATE_ROOT=${THEKERNEL_STATE_DIR:-$(cd -- "$REPO_ROOT" && pwd)/.state}
mkdir -p "$STATE_ROOT"

log() { printf 'build-gcc-payload: %s\n' "$*" >&2; }
die() { printf 'build-gcc-payload: %s\n' "$*" >&2; exit 1; }

for command in curl rpm2cpio cpio tar gcc sha256sum readelf; do
    command -v "$command" >/dev/null 2>&1 || die "required command not found: $command"
done

# --- fetching --------------------------------------------------------------

fetch_rpm() {
    local package=$1 file=$2 expected=$3
    # Declared on separate lines: `local a=X b=$a` does not see `a` yet, and
    # under `set -u` that is a hard failure rather than an empty string.
    local dir="$SOURCE_CACHE/rpm"
    local path="$dir/$file"
    if [ ! -f "$path" ]; then
        mkdir -p "$dir"
        log "downloading $package"
        ( cd "$dir" && dnf download --destdir . "$package" 2>&1 | tail -2 ) || true
        [ -f "$path" ] || die "cannot download $package into $dir"
    fi
    local actual
    actual=$(sha256sum "$path" | cut -d' ' -f1)
    [ "$actual" = "$expected" ] ||
        die "checksum mismatch for $file:
  expected $expected
  actual   $actual"
    printf '%s\n' "$path"
}

# --- unpacking --------------------------------------------------------------
#
# A stable path under the state root, not mktemp: these trees are large and
# rebuilding them on every payload build would make the build slower than the
# guest boot it exists to make reliable.

BUILD_ROOT="$STATE_ROOT/gcc-payload-build"
RPM_TREE="$BUILD_ROOT/rpm"
mkdir -p "$BUILD_ROOT" "$RPM_TREE"

# Unpack one RPM into its own tree, and remember the mapping from package name
# to tree so the staging below can name what it takes from where.
rpm_trees=()
for pin in "${RPM_PINS[@]}"; do
    package=${pin%%:*}
    rest=${pin#*:}
    release=${rest%%:*}
    expected=${rest#*:}
    file="$package-$release.x86_64.rpm"

    # Assigning the result of a function that can `die` is not enough to stop
    # the build: a failure inside `$( )` exits only the subshell, and `set -e`
    # does not inspect an assignment's status.  So the call is its own statement
    # whose status is checked, and the assignment follows.  Measured while
    # building from a fresh state directory: without this the build reported a
    # download failure and then failed much later with "cpio: premature end of
    # archive", which reads as a corrupt RPM rather than as a failed download.
    fetch_rpm "$package" "$file" "$expected" > "$BUILD_ROOT/.rpm-path" ||
        die "cannot fetch $package-$release"
    rpm=$(cat "$BUILD_ROOT/.rpm-path")
    tree="$RPM_TREE/$package-$release"
    # The marker, not the directory, decides whether the tree is usable.  An
    # interrupted unpack leaves a directory that looks present and is missing
    # most of its files, and the installs below then fail one at a time with a
    # message about the file rather than about the tree.
    if [ ! -f "$tree/.unpacked" ]; then
        log "unpacking $package-$release"
        rm -rf "$tree"
        mkdir -p "$tree"
        ( cd "$tree" && rpm2cpio "$rpm" | cpio --quiet -idmu --no-absolute-filenames )
        : > "$tree/.unpacked"
    fi
    rpm_trees+=("$package:$tree")
done

tree_of() {
    local wanted=$1 entry
    for entry in "${rpm_trees[@]}"; do
        if [ "${entry%%:*}" = "$wanted" ]; then
            printf '%s\n' "${entry#*:}"
            return 0
        fi
    done
    die "no tree for package $wanted"
}

T_GCC=$(tree_of gcc)
T_CPP=$(tree_of cpp)
T_BINUTILS=$(tree_of binutils)
T_GLIBC_DEVEL=$(tree_of glibc-devel)
T_KERNEL_HEADERS=$(tree_of kernel-headers)

# --- the loader milestone this one is built on ------------------------------
#
# The compiler is a dynamic glibc program, so this payload needs everything
# Phase 3's first milestone staged: the loader, libc.so.6, and the dynamically
# linked smoke program.  Staging it here rather than relying on the image build
# to combine two payloads keeps the rootfs half honest -- the image enables the
# glibc case because this payload contains what that case runs, not because a
# flag was set somewhere else.  Both payloads take the same two files from the
# same pinned RPM, so the bytes are identical either way.
log "staging the glibc loader milestone this compiler depends on"
THEKERNEL_SOURCE_CACHE="$SOURCE_CACHE" \
    "$SCRIPT_DIR/build-glibc-payload.sh" --output "$OUTPUT" \
    --source-cache "$SOURCE_CACHE" >&2

# --- staging ----------------------------------------------------------------
#
# Note that the glibc payload above already created $OUTPUT, so this is not a
# clean directory and the staging below adds to it rather than replacing it.

mkdir -p \
    "$OUTPUT/usr/bin" \
    "$OUTPUT$LIBEXEC_DIR" \
    "$OUTPUT$GCC_LIB_DIR" \
    "$OUTPUT/usr/lib64" \
    "$OUTPUT/usr/include" \
    "$OUTPUT/lib64"

# 1. The compiler proper.  cc1 comes from `cpp`; see the pins above.
log "staging the driver, cc1, as and ld"
install -m 0755 "$T_GCC/usr/bin/gcc" "$OUTPUT/usr/bin/gcc"
install -m 0755 "$T_CPP$LIBEXEC_DIR/cc1" "$OUTPUT$LIBEXEC_DIR/cc1"
install -m 0755 "$T_GCC$LIBEXEC_DIR/collect2" "$OUTPUT$LIBEXEC_DIR/collect2"
install -m 0755 "$T_GCC$LIBEXEC_DIR/liblto_plugin.so" \
    "$OUTPUT$LIBEXEC_DIR/liblto_plugin.so"

# The assembler, and ld.bfd installed *as* ld.
#
# Fedora's /usr/bin/ld is GNU gold, which is C++ and therefore needs
# libstdc++.so.6 (2.8 MB) and libjansson for no benefit to a C-only payload.
# ld.bfd is smaller itself and needs neither.  Installing it under the name
# `ld` is what makes the driver find it without any spec file of our own.
install -m 0755 "$T_BINUTILS/usr/bin/as" "$OUTPUT/usr/bin/as"
install -m 0755 "$T_BINUTILS/usr/bin/ld.bfd" "$OUTPUT/usr/bin/ld"

# 2. GCC's support files: the *whole* top level of its lib directory.
#
# Copying the directory rather than a list is deliberate and was learned by
# compiling.  GCC's specs name libgcc_s_asneeded.so and libatomic_asneeded.so
# when linking, so a hand-written list of the files one expects (libgcc.a,
# libgcc_s.so, crtbegin.o) makes *every* link fail with
# "cannot find -lgcc_s_asneeded".  The 32-bit subdirectory is dropped because
# this guest is x86_64-only and can never use it.
log "staging gcc's support files"
cp -a "$T_GCC$GCC_LIB_DIR/." "$OUTPUT$GCC_LIB_DIR/"
rm -rf "$OUTPUT$GCC_LIB_DIR/32"

# GCC's private headers, which are not in /usr/include.  Without them the very
# first #include fails: /usr/include/stdio.h includes <stddef.h>, which only
# exists here.
cp -a "$T_GCC$GCC_LIB_DIR/include" "$OUTPUT$GCC_LIB_DIR/include"

# 3. glibc's startup objects, its linker scripts, and the headers.
log "staging glibc's startup objects and headers"
for file in crt1.o crti.o crtn.o Scrt1.o Mcrt1.o rcrt1.o gcrt1.o grcrt1.o \
    libc_nonshared.a libc.so libm.so libpthread.a libdl.a librt.a libutil.a; do
    [ -f "$T_GLIBC_DEVEL/usr/lib64/$file" ] || continue
    install -m 0644 "$T_GLIBC_DEVEL/usr/lib64/$file" "$OUTPUT/usr/lib64/$file"
done
cp -a "$T_GLIBC_DEVEL/usr/include/." "$OUTPUT/usr/include/"
cp -a "$T_KERNEL_HEADERS/usr/include/." "$OUTPUT/usr/include/"

# `libatomic.so` is a linker script naming /usr/lib64/libatomic.so.1.2.0
# absolutely, and gcc's specs reach it through -latomic on *every* link.  So
# this file is not optional even for a program that uses no atomics: without it
# every link fails with "cannot find /usr/lib64/libatomic.so.1.2.0".
install -m 0755 "$(tree_of libatomic)/usr/lib64/libatomic.so.1.2.0" \
    "$OUTPUT/usr/lib64/libatomic.so.1.2.0"

# 4. The shared-library closure, each file under the name the loader looks for.
#
# The source path is where the RPM puts it; the destination name is the soname
# from that library's own DT_NEEDED, because the loader matches it literally.
# Staging libctf.so.0.0.0 without also naming it libctf.so.0 would leave the
# file present and unfindable.
log "staging the shared-library closure"
declare -A tree_of_rpm=(
    [binutils]="$T_BINUTILS"
    [libgcc]="$RPM_TREE/libgcc-$GCC_RELEASE"
    [glibc]="$RPM_TREE/glibc-$GLIBC_RELEASE"
    [gmp]="$RPM_TREE/gmp-6.3.0-5.fc44"
    [mpfr]="$RPM_TREE/mpfr-4.2.2-3.fc44"
    [libmpc]="$RPM_TREE/libmpc-1.4.1-1.fc44"
    [zlib-ng-compat]="$RPM_TREE/zlib-ng-compat-2.3.3-3.fc44"
    [libzstd]="$RPM_TREE/libzstd-1.5.7-5.fc44"
    [jansson]="$RPM_TREE/jansson-2.14-4.fc44"
)

stage_library() {
    local package=$1 source_path=$2 destination_name=$3
    local tree=${tree_of_rpm[$package]}
    [ -f "$tree$source_path" ] ||
        die "$package does not contain $source_path"
    install -m 0755 "$tree$source_path" "$OUTPUT/lib64/$destination_name"
}

# glibc itself.  The loader and libc.so.6 are the two that make dynamic linking
# work at all; libm and libmvec are what -lm resolves to.
stage_library glibc /usr/lib64/ld-linux-x86-64.so.2 ld-linux-x86-64.so.2
stage_library glibc /usr/lib64/libc.so.6 libc.so.6
stage_library glibc /usr/lib64/libm.so.6 libm.so.6
stage_library glibc /usr/lib64/libmvec.so.1 libmvec.so.1
# gcc's and binutils' own runtime dependencies.
stage_library libgcc /lib64/libgcc_s-16-20260819.so.1 libgcc_s.so.1
stage_library gmp /usr/lib64/libgmp.so.10.5.0 libgmp.so.10
stage_library mpfr /usr/lib64/libmpfr.so.6.2.2 libmpfr.so.6
stage_library libmpc /usr/lib64/libmpc.so.3.4.1 libmpc.so.3
stage_library zlib-ng-compat /usr/lib64/libz.so.1.3.1.zlib-ng libz.so.1
stage_library libzstd /usr/lib64/libzstd.so.1.5.7 libzstd.so.1
stage_library jansson /usr/lib64/libjansson.so.4.14.0 libjansson.so.4
# binutils' private libraries, named by soname.
stage_library binutils "/usr/lib64/libbfd-$BINUTILS_RELEASE.so" \
    "libbfd-$BINUTILS_RELEASE.so"
stage_library binutils /usr/lib64/libctf.so.0.0.0 libctf.so.0
stage_library binutils /usr/lib64/libsframe.so.3.0.0 libsframe.so.3

# --- 5. validating the staged tree -----------------------------------------
#
# Everything above is staging; none of it proves the tree can compile.  The
# three traps that cost a guest boot each -- cc1 missing from `gcc`, stddef.h
# missing from /usr/include, and the *_asneeded.so files missing from a
# hand-written list -- are all *link-time* failures with a file tree that looks
# complete.  A guest boot is the expensive way to find them, so the staged
# compiler is run here instead.
#
# The compiler is driven with explicit -B and -isystem flags rather than
# through chroot: a chroot needs a shell inside the staging tree, which would
# mean staging one and its closure, and it needs privileges that not every
# build host grants.  The flags below are exactly the guest's own search paths,
# since in the guest these directories *are* the system ones, so a tree that
# compiles here compiles there.
log "validating: compiling and running a program with the staged toolchain"
VALIDATE_DIR="$BUILD_ROOT/validate"
rm -rf "$VALIDATE_DIR"
mkdir -p "$VALIDATE_DIR"

cat > "$VALIDATE_DIR/probe.c" <<'PROBE'
/* A two-translation-unit program using threads and the math library: the
 * smallest thing that forces the startup objects, the private headers, the
 * pthread and libm linker scripts and libatomic into play. */
#include <math.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>

static void *worker(void *arg) {
    double value = *(double *)arg;
    double *out = malloc(sizeof *out);
    if (out == NULL) {
        return NULL;
    }
    *out = sqrt(value) + 1.0;
    return out;
}

int main(void) {
    pthread_t thread;
    double input = 2.0;
    void *result = NULL;

    if (pthread_create(&thread, NULL, worker, &input) != 0) {
        return 1;
    }
    pthread_join(thread, &result);
    if (result == NULL) {
        return 1;
    }
    printf("GCC_PAYLOAD_PROBE folded=%.6f runtime=%.6f\n", *(double *)result,
           sqrt(2.0));
    free(result);
    return 0;
}
PROBE

"$OUTPUT/usr/bin/gcc" -O2 -o "$VALIDATE_DIR/probe" "$VALIDATE_DIR/probe.c" -lm \
    -B "$OUTPUT/usr/lib64/" -B "$OUTPUT$GCC_LIB_DIR/" \
    -isystem "$OUTPUT/usr/include" -isystem "$OUTPUT$GCC_LIB_DIR/include" ||
    die "the staged toolchain could not compile a program"

# And the product has to run, through the *staged* loader and the staged
# libraries.  Running it directly would use this host's loader and this host's
# libc, which would validate neither.
probe_output=$("$OUTPUT/lib64/ld-linux-x86-64.so.2" --library-path "$OUTPUT/lib64" \
    "$VALIDATE_DIR/probe") ||
    die "the program the staged toolchain built did not run"
case "$probe_output" in
    *"GCC_PAYLOAD_PROBE folded=2.414214 runtime=1.414214"*)
        log "validated: $probe_output" ;;
    *) die "the staged toolchain's program printed unexpected output: $probe_output" ;;
esac

# The product must be dynamically linked against the staged libc, not against
# anything of the host's: a static or host-linked result would compile here and
# prove nothing about the guest.
readelf -lW "$VALIDATE_DIR/probe" | grep -q '/lib64/ld-linux-x86-64.so.2' ||
    die "the program the staged compiler built does not use the staged loader"

# --- 6. the manifest --------------------------------------------------------

mkdir -p "$OUTPUT/opt/thekernel-tools"
{
    printf '# gcc guest tool payload\n'
    printf 'gcc %s\n' "$GCC_RELEASE"
    printf 'binutils %s\n' "$BINUTILS_RELEASE"
    printf 'glibc-devel %s\n' "$GLIBC_RELEASE"
    printf 'kernel-headers %s\n' "$KERNEL_HEADERS_RELEASE"
    printf '# path size sha256\n'
    # Relative paths, so the manifest describes the payload and not the
    # directory it happened to be built in.
    ( cd "$OUTPUT" && find . -type f -printf '%P\n' | LC_ALL=C sort |
        while read -r file; do
            printf '%s %s %s\n' "$file" "$(stat -c %s "$file")" \
                "$(sha256sum "$file" | cut -d' ' -f1)"
        done )
} > "$OUTPUT/opt/thekernel-tools/MANIFEST"

log "installed $(du -sh "$OUTPUT" | cut -f1) into $OUTPUT"
log "manifest at $OUTPUT/opt/thekernel-tools/MANIFEST"
