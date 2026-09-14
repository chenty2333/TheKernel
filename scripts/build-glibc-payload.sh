#!/usr/bin/env bash
# Build the `glibc` guest tool payload: a dynamic loader, a shared libc, and a
# dynamically linked program, all staged for build-rootfs.sh to copy in.
#
# This is Phase 3's first milestone, and its point is that it contains no
# kernel work at all.  Reconnaissance against the kernel's ELF loader found
# that TheKernel already implements the whole dynamic-linking contract glibc
# needs -- PT_INTERP is read from the file, the interpreter is mapped and
# started with AT_BASE pointing at it, and the auxiliary vector it builds was
# measured numerically identical to Linux's on every entry glibc reads.  What
# was missing was only the *image*: no /lib64, no loader, no libc.so.6, and no
# dynamically linked binary anywhere in the rootfs.
#
# So this stages those four things and nothing else.  If a dynamic program runs
# in the guest after this, the loader contract is proven; if it does not, the
# failure is real kernel evidence rather than a missing file.
#
# Why the loader is taken from the binary RPM instead of the live host:
#
#   - The host's /lib64 may be a different version from anything pinned, and
#     copying it makes the payload depend on this machine's updates.
#   - Rebuilding glibc is not needed and is actively risky.  A glibc built with
#     -march=x86-64-v3 (the desktop default on some distributions) produces a
#     loader that aborts on a CPU below that ISA level, which is exactly the
#     kind of failure that looks like a kernel bug.  A pinned binary RPM is
#     built for the baseline and is the same bytes for everyone who builds it.
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)

# --- pins ------------------------------------------------------------------
#
# Fedora 44's glibc, matching the toolchain that builds the smoke binary.  The
# loader and the shared library must come from the same build: glibc's internal
# ABI is versioned per release, and mixing a loader from one release with a libc
# from another fails in ways that look like a kernel fault.
GLIBC_RPM_RELEASE=2.43-8.fc44
GLIBC_RPM_FILE="glibc-${GLIBC_RPM_RELEASE}.x86_64.rpm"
GLIBC_RPM_SHA256=84f0af455b8f541ead8ae4b74c220563babd9980dbd308226b90a22d6144ea14

# Where the RPM keeps the two files.  Fedora's glibc is a /usr-merged package:
# the real files live under /usr/lib64 and /lib64 holds compatibility symlinks
# that only exist on an installed system, so the RPM has nothing at /lib64.
RPM_LOADER_PATH=/usr/lib64/ld-linux-x86-64.so.2
RPM_LIBC_PATH=/usr/lib64/libc.so.6

# Where the guest needs them, which is not a free choice.  The loader's path is
# recorded inside every dynamically linked executable as PT_INTERP, and the
# kernel resolves exactly that string, so a loader staged anywhere else is never
# found.  Likewise the soname: the loader resolves DT_NEEDED "libc.so.6" through
# its search path, which on a merged system includes /lib64 and /usr/lib64 --
# /lib64 is chosen because it is the path PT_INTERP already commits to, so one
# directory holds both files and the layout matches what the ELF headers say.
LOADER_PATH=/lib64/ld-linux-x86-64.so.2
LIBC_PATH=/lib64/libc.so.6

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
usage: build-glibc-payload.sh --output DIR [--source-cache DIR]

Stages a dynamic loader, a shared libc and a dynamically linked smoke program
under DIR, in the layout build-rootfs.sh copies into the guest image.
EOF
    exit 0
fi

[ -n "$OUTPUT" ] || { printf '%s\n' '--output is required' >&2; exit 2; }
mkdir -p "$SOURCE_CACHE"

STATE_ROOT=${THEKERNEL_STATE_DIR:-$(cd -- "$REPO_ROOT" && pwd)/.state}
mkdir -p "$STATE_ROOT"

log() { printf 'build-glibc-payload: %s\n' "$*" >&2; }
die() { printf 'build-glibc-payload: %s\n' "$*" >&2; exit 1; }

for command in curl rpm2cpio cpio tar gcc sha256sum; do
    command -v "$command" >/dev/null 2>&1 || die "required command not found: $command"
done

# --- fetching --------------------------------------------------------------
#
# The same cache and the same policy as the other payloads: a file that is not
# already cached is downloaded once, and every use is checked against the pin.
fetch_source() {
    local url=$1 file=$2 expected=$3
    local path="$SOURCE_CACHE/$file"
    if [ ! -f "$path" ]; then
        log "downloading $file"
        curl --fail --location --silent --show-error --output "$path.part" "$url" ||
            die "cannot download $url"
        mv "$path.part" "$path"
    fi
    local actual
    actual=$(sha256sum "$path" | cut -d' ' -f1)
    [ "$actual" = "$expected" ] ||
        die "checksum mismatch for $file:
  expected $expected
  actual   $actual"
    printf '%s\n' "$path"
}

# Fedora RPMs come through dnf, which knows the mirror layout; the file name
# and hash are still what decides whether the cached copy is usable.
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

# --- build tree ------------------------------------------------------------
#
# A stable path under the state root, not mktemp: the binary's build ID and any
# path it records have to be the same on every build, or the payload appears to
# change when nothing did.
BUILD_ROOT="$STATE_ROOT/glibc-payload-build"
RPM_TREE="$BUILD_ROOT/rpm"
mkdir -p "$BUILD_ROOT"

# --- 1. the pinned RPM ------------------------------------------------------

log "unpacking glibc $GLIBC_RPM_RELEASE"
# A failure inside `$( )` exits only the subshell, and `set -e` does not look
# at an assignment's status, so the call is its own checked statement.
fetch_rpm glibc "$GLIBC_RPM_FILE" "$GLIBC_RPM_SHA256" > "$BUILD_ROOT/.rpm-path" ||
    die "cannot fetch glibc"
glibc_rpm=$(cat "$BUILD_ROOT/.rpm-path")
if [ ! -e "$RPM_TREE$RPM_LOADER_PATH" ] || [ ! -e "$RPM_TREE$RPM_LIBC_PATH" ]; then
    rm -rf "$RPM_TREE"
    mkdir -p "$RPM_TREE"
    ( cd "$RPM_TREE" && rpm2cpio "$glibc_rpm" | cpio -idm --quiet )
fi

[ -f "$RPM_TREE$RPM_LOADER_PATH" ] || die "the RPM contains no $RPM_LOADER_PATH"
[ -f "$RPM_TREE$RPM_LIBC_PATH" ] || die "the RPM contains no $RPM_LIBC_PATH"
# A symlink here would stage a dangling path into the guest, where the kernel
# would resolve it and fail with a confusing ENOENT.
[ ! -L "$RPM_TREE$RPM_LOADER_PATH" ] || die "$RPM_LOADER_PATH is a symlink in the RPM"
[ ! -L "$RPM_TREE$RPM_LIBC_PATH" ] || die "$RPM_LIBC_PATH is a symlink in the RPM"

# The loader must be a real x86_64 ELF, and it must not itself need another
# loader: a PT_INTERP inside ld.so would be a broken RPM.
loader_elf=$(head -c 20 "$RPM_TREE$RPM_LOADER_PATH" | od -An -tx1 | tr -d ' \n')
case "$loader_elf" in
    7f454c46020101*) ;;
    *) die "$LOADER_PATH is not a 64-bit little-endian ELF" ;;
esac

# --- 2. the smoke program ---------------------------------------------------
#
# Built with the host gcc, dynamically, against the host's glibc headers.  It
# has to be dynamic: a static binary proves nothing about the loader, which is
# the entire subject of this payload.
#
# It prints what the *kernel* told it rather than what it was compiled with, so
# a passing run is evidence about the loader contract and not about the build.
mkdir -p "$BUILD_ROOT/smoke"
cat >"$BUILD_ROOT/smoke/glibc-smoke.c" <<'EOF'
/* TheKernel guest-toolchain: does a dynamically linked program start at all?
 *
 * A dynamic program is started by the kernel, which reads the interpreter path
 * out of the file and runs *that* instead, passing the program's own entry
 * point through the auxiliary vector.  So this program running to completion
 * means four separate things happened, and it reports each of them:
 *
 *   1. the kernel found and mapped the interpreter named by PT_INTERP
 *   2. ld.so relocated libc.so.6, which means it read the program headers and
 *      the search path correctly
 *   3. the kernel handed ld.so an auxiliary vector it accepted, including the
 *      AT_PHDR/AT_PHNUM/AT_ENTRY triple that describes the *executable* rather
 *      than the interpreter
 *   4. control arrived back at this program's own entry point with AT_ENTRY
 *
 * Every value printed comes from getauxval(), which reads the vector the kernel
 * built.  Nothing here is baked in at compile time, so the output is evidence
 * about this boot rather than about the build.
 */
#include <errno.h>
#include <gnu/libc-version.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/auxv.h>
#include <sys/utsname.h>
#include <unistd.h>

/* The dynamic loader is what actually runs first; the kernel only starts it.
 * Printing the loader's own idea of where the program's headers are is the
 * cheapest proof that it read the auxiliary vector rather than guessed. */
static void dump_auxv(void) {
    /* Queried here rather than in a file-scope initialiser: getauxval is a
     * call, and a static array cannot be filled by one. */
    static const struct {
        const char *name;
        int key;
    } entries[] = {
        { "AT_PHDR", AT_PHDR },
        { "AT_PHNUM", AT_PHNUM },
        { "AT_PHENT", AT_PHENT },
        { "AT_ENTRY", AT_ENTRY },
        { "AT_BASE", AT_BASE },
        { "AT_PAGESZ", AT_PAGESZ },
        { "AT_RANDOM", AT_RANDOM },
        { "AT_SECURE", AT_SECURE },
        { "AT_EXECFN", AT_EXECFN },
    };
    for (size_t index = 0; index < sizeof(entries) / sizeof(entries[0]); ++index) {
        /* errno is cleared first so an absent entry is distinguishable from a
         * present one whose value happens to be zero. */
        errno = 0;
        unsigned long value = getauxval(entries[index].key);
        printf("THEKERNEL_GLIBC_SMOKE_AUXV %s=0x%lx%s\n", entries[index].name, value,
               errno == ENOENT ? " (absent)" : "");
    }
}

int main(void) {
    struct utsname uts;

    /* stdout is unbuffered so each marker survives a crash later in the run;
     * a partial transcript is more useful than none. */
    setvbuf(stdout, NULL, _IONBF, 0);
    puts("THEKERNEL_GLIBC_SMOKE_START");

    /* AT_BASE is the interpreter's base address and is non-zero for any
     * dynamically linked program.  Zero means the kernel started the program
     * directly, which would make everything below a static-linking result. */
    unsigned long base = getauxval(AT_BASE);
    printf("THEKERNEL_GLIBC_SMOKE_LOADER base=0x%lx ptr=%p\n", base,
           (void *)(unsigned long)getauxval(AT_BASE));
    if (base == 0) {
        puts("THEKERNEL_GLIBC_SMOKE_NO_LOADER");
        return 1;
    }

    dump_auxv();

    if (uname(&uts) != 0) {
        perror("uname");
        return 1;
    }
    printf("THEKERNEL_GLIBC_SMOKE_UNAME sysname=%s release=%s machine=%s\n",
           uts.sysname, uts.release, uts.machine);

    /* glibc's own version, from the library that was just relocated.  This is
     * the string ld.so and libc.so.6 have to agree on. */
    printf("THEKERNEL_GLIBC_SMOKE_GLIBC release=%s\n", gnu_get_libc_version());

    /* The program's own path, resolved through the kernel's auxv entry for it.
     * A missing or empty AT_EXECFN is harmless to glibc but says the kernel did
     * not finish the vector. */
    const char *execfn = (const char *)getauxval(AT_EXECFN);
    printf("THEKERNEL_GLIBC_SMOKE_EXECFN %s\n",
           (execfn != NULL && execfn[0] != '\0') ? execfn : "(absent)");

    /* Heap allocation, because it exercises the brk/mmap path through a libc
     * that was loaded rather than linked in. */
    char *scratch = malloc(4096);
    if (scratch == NULL) {
        perror("malloc");
        return 1;
    }
    scratch[0] = 'x';
    scratch[4095] = '\0';
    free(scratch);

    /* The real marker, printed last: everything above it already happened. */
    puts("THEKERNEL_GLIBC_SMOKE_OK");
    return 0;
}
EOF

log "building the dynamically linked smoke program"
gcc -O2 -Wall -Wextra -o "$BUILD_ROOT/smoke/glibc-smoke" "$BUILD_ROOT/smoke/glibc-smoke.c"

# The whole point is that this binary needs a loader.  A static one would run
# even in an image with no /lib64 at all, so it would certify nothing.
readelf_output=$(readelf -lW "$BUILD_ROOT/smoke/glibc-smoke")
printf '%s\n' "$readelf_output" | grep -q 'Requesting program interpreter' ||
    die "the smoke program is not dynamically linked; it would prove nothing"
interp=$(printf '%s\n' "$readelf_output" |
    sed -n 's/.*Requesting program interpreter: \(.*\)\]/\1/p' | head -1)
[ "$interp" = "$LOADER_PATH" ] ||
    die "the smoke program asks for interpreter $interp, but this payload stages $LOADER_PATH"
printf '%s\n' "$readelf_output" >"$BUILD_ROOT/smoke/readelf.txt"

# And its only library dependency has to be the libc this payload stages.
needed=$(readelf -dW "$BUILD_ROOT/smoke/glibc-smoke" |
    sed -n 's/.*NEEDED.*\[\(.*\)\]/\1/p' | sort -u)
printf '%s\n' "$needed" >"$BUILD_ROOT/smoke/needed.txt"
[ "$(printf '%s\n' "$needed" | tr -d '\n')" = "libc.so.6" ] ||
    die "the smoke program needs [$needed]; this payload stages only libc.so.6"

# --- 3. stage ---------------------------------------------------------------
#
# Rebuilt from scratch so a removed file cannot survive in the output, then
# restored from the tcc payload's terms: the guest image is the only consumer
# and it gets exactly this tree.
rm -rf "$OUTPUT"
mkdir -p "$OUTPUT/lib64" "$OUTPUT/opt/thekernel-tools/bin" "$OUTPUT/opt/thekernel-tools/payloads"

install -m 0755 "$RPM_TREE$RPM_LOADER_PATH" "$OUTPUT$LOADER_PATH"
install -m 0755 "$RPM_TREE$RPM_LIBC_PATH" "$OUTPUT$LIBC_PATH"
install -m 0755 "$BUILD_ROOT/smoke/glibc-smoke" "$OUTPUT/opt/thekernel-tools/bin/glibc-smoke"

# ld.so.cache and ld.so.preload are both optional: measured with an empty /etc,
# the loader resolves libc.so.6 from its built-in default search path (the
# directory named by the program's own DT_RUNPATH, then /lib64, then /usr/lib64).
# Writing a cache here would need ldconfig, which is another program to ship and
# another thing to be wrong; the default path is enough and is what is tested.

# --- 4. checks --------------------------------------------------------------

[ -x "$OUTPUT$LOADER_PATH" ] || die "the staged loader is not executable"
[ -f "$OUTPUT$LIBC_PATH" ] || die "the staged libc is missing"

# The loader must not depend on anything: it is the first thing to run and there
# is nothing to load its dependencies with.
if readelf -dW "$OUTPUT$LOADER_PATH" | grep -q '(NEEDED)'; then
    die "the staged loader itself has DT_NEEDED; it cannot be the first thing to run"
fi

# Sizes and hashes recorded, so a later stage can notice a changed payload
# without re-deriving what it should contain.
{
    printf '# glibc guest tool payload\n'
    printf 'glibc %s\n' "$GLIBC_RPM_RELEASE"
    printf 'loader %s\n' "$LOADER_PATH"
    printf '# path size sha256\n'
    for file in "$LOADER_PATH" "$LIBC_PATH" \
                /opt/thekernel-tools/bin/glibc-smoke; do
        relative=${file#/}
        full="$OUTPUT/$relative"
        printf '%s %s %s\n' "$relative" "$(stat -c %s "$full")" \
            "$(sha256sum "$full" | cut -d' ' -f1)"
    done
} >"$OUTPUT/opt/thekernel-tools/MANIFEST"

log "installed $(du -sh "$OUTPUT" | cut -f1) into $OUTPUT"
printf '%s\n' "$OUTPUT/opt/thekernel-tools/MANIFEST"
