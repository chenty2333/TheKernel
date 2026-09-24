#!/usr/bin/env bash
#
# build-initramfs.sh -- build the TheKernel "Phase 2b" inner-guest initramfs.
#
# Produces a cpio "newc" archive (gzip-compressed by default) that boots under
# Alpine's linux-virt bzImage via QEMU's `-kernel ... -initrd ...` path, with no
# disk and no network.  PID 1 runs /init, prints INNER_-prefixed markers on the
# serial console and then performs a real ACPI S5 poweroff so that
# qemu-system-x86_64 exits with status 0.
#
# NOTE: "no disk" is not "no firmware".  QEMU still loads and executes SeaBIOS
# and still needs its firmware *data files* on disk at run time -- the inner
# QEMU dies with "could not load PC BIOS 'bios-256k.bin'" otherwise.  Ship
# stage/qemu-firmware/ into the image and pass `-L <dir>`.
#
# Network-free: the only input is the pinned Alpine minirootfs tarball.
#
# Usage:
#   ./build-initramfs.sh <output-path> [--full] [--compress gzip|lz4|none]
#
# Environment:
#   MINIROOTFS_TARBALL  path to alpine-minirootfs-<ver>-x86_64.tar.gz
#                       (default: the pinned cache path below)
#   WORKDIR             scratch directory (default: a fresh directory under TMPDIR)
#
set -Eeuo pipefail

# ---------------------------------------------------------------- pinning ---
ALPINE_RELEASE="3.24.1"
ALPINE_ARCH="x86_64"
MINIROOTFS_NAME="alpine-minirootfs-${ALPINE_RELEASE}-${ALPINE_ARCH}.tar.gz"
MINIROOTFS_SHA256="41f73e3cf5fa919b8aa5ca6b30dc48f0da2720776d7423e2a7748211456fe081"

# The minirootfs is a pinned, downloaded input; the caller owns the cache and
# passes it in.  There is no default, so a wrong path fails loudly instead of
# silently using whatever happens to be in one developer's cache.
MINIROOTFS_TARBALL="${MINIROOTFS_TARBALL:?set MINIROOTFS_TARBALL to the pinned Alpine minirootfs tarball}"

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
# Scratch space.  Defaults outside the repository, because unpacking the
# minirootfs here would leave build droppings in a source tree.
WORKDIR="${WORKDIR:-${TMPDIR:-/var/tmp}/thekernel-initramfs.$$}"

# ------------------------------------------------------------------ usage ---
die() { printf 'build-initramfs.sh: FATAL: %s\n' "$*" >&2; exit 1; }
say() { printf 'build-initramfs.sh: %s\n' "$*" >&2; }

MODE="minimal"
COMPRESS="gzip"
OUT=""
while [ $# -gt 0 ]; do
	case "$1" in
		--full)      MODE="full"; shift ;;
		--compress) [ $# -ge 2 ] || die "--compress needs an argument"
		             COMPRESS="$2"; shift 2 ;;
		-h|--help)   sed -n '2,25p' "${BASH_SOURCE[0]}"; exit 0 ;;
		-*)          die "unknown option: $1" ;;
		*)           [ -z "$OUT" ] || die "more than one output path given"
		             OUT="$1"; shift ;;
	esac
done

[ -n "$OUT" ] || die "usage: build-initramfs.sh <output-path> [--full] [--compress gzip|lz4|none]"
case "$COMPRESS" in gzip|lz4|none) ;; *) die "bad --compress value: $COMPRESS" ;; esac

# ------------------------------------------------- missing-input checks ----
for tool in tar cpio sha256sum find install mkdir rm; do
	command -v "$tool" >/dev/null 2>&1 || die "required tool not found in PATH: $tool"
done
case "$COMPRESS" in
	gzip) command -v gzip >/dev/null 2>&1 || die "required tool not found in PATH: gzip" ;;
	lz4)  command -v lz4  >/dev/null 2>&1 || die "required tool not found in PATH: lz4"  ;;
esac

[ -f "$MINIROOTFS_TARBALL" ] || die "minirootfs tarball not found: $MINIROOTFS_TARBALL
  Expected the pinned Alpine ${ALPINE_RELEASE} release artifact:
    https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_RELEASE%.*}/releases/${ALPINE_ARCH}/${MINIROOTFS_NAME}
  (sha256 ${MINIROOTFS_SHA256})"

say "verifying ${MINIROOTFS_TARBALL}"
actual="$(sha256sum -- "$MINIROOTFS_TARBALL" | cut -d' ' -f1)"
[ "$actual" = "$MINIROOTFS_SHA256" ] || die "minirootfs sha256 mismatch
  expected: $MINIROOTFS_SHA256
  actual:   $actual"
say "sha256 OK (${MINIROOTFS_SHA256})"

mkdir -p -- "$WORKDIR" || die "cannot create WORKDIR $WORKDIR"
OUT_DIR="$(cd -- "$(dirname -- "$OUT")" 2>/dev/null && pwd -P)" \
	|| die "output directory does not exist: $(dirname -- "$OUT")"
OUT="${OUT_DIR}/$(basename -- "$OUT")"

SCRATCH="$(mktemp -d "${WORKDIR}/.mkinitrd.XXXXXX")" || die "mktemp failed in $WORKDIR"
cleanup() { rm -rf -- "$SCRATCH"; }
trap cleanup EXIT

ROOT="${SCRATCH}/root"
mkdir -p -- "$ROOT"

# --------------------------------------------------------- unpack sources ---
say "unpacking minirootfs"
tar -xzf "$MINIROOTFS_TARBALL" -C "$ROOT" --exclude='./dev/*' \
	|| die "failed to unpack minirootfs"

SRC_BUSYBOX="${ROOT}/bin/busybox"
SRC_LD="${ROOT}/lib/ld-musl-x86_64.so.1"
[ -f "$SRC_BUSYBOX" ] || die "minirootfs has no /bin/busybox -- wrong or corrupt tarball?"
[ -f "$SRC_LD" ]      || die "minirootfs has no /lib/ld-musl-x86_64.so.1 -- wrong or corrupt tarball?"
grep -q "^${ALPINE_RELEASE}$" "${ROOT}/etc/alpine-release" \
	|| die "minirootfs reports release '$(cat "${ROOT}/etc/alpine-release" 2>/dev/null)', expected ${ALPINE_RELEASE}"

# ------------------------------------------------------------- assemble -----
STAGE="${SCRATCH}/stage"
mkdir -p -- "$STAGE"/{bin,dev,etc,lib,proc,run,sys,tmp,root,mnt,newroot}

if [ "$MODE" = "minimal" ]; then
	# Alpine's busybox is dynamically linked against musl, so the loader and
	# libc MUST travel with it.  That is the whole userspace we need: no
	# openrc, no apk, no modules.
	say "mode=minimal: busybox + musl + /init only"
	install -m 0755 -- "$SRC_BUSYBOX" "$STAGE/bin/busybox"
	install -m 0755 -- "$SRC_LD"      "$STAGE/lib/ld-musl-x86_64.so.1"
	ln -sf ld-musl-x86_64.so.1 "$STAGE/lib/libc.musl-x86_64.so.1"

	# Applet symlinks.  Keep this list to what /init actually needs so the
	# archive stays small; busybox multi-call dispatch is by argv[0].
	for a in sh ash mount umount echo cat sleep sync poweroff halt reboot \
	         ls dmesg uname printenv test true false mdev env cut head \
	         grep sed tr wc; do
		ln -sf busybox "$STAGE/bin/$a"
	done

	install -m 0644 -- "${ROOT}/etc/alpine-release" "$STAGE/etc/alpine-release"
	install -m 0644 -- "${ROOT}/etc/passwd"         "$STAGE/etc/passwd"
	install -m 0644 -- "${ROOT}/etc/group"          "$STAGE/etc/group"
else
	# Full minirootfs: same /init, but a complete Alpine userland underneath.
	# Much bigger and slower to unpack; kept as a fidelity fallback.
	say "mode=full: entire minirootfs + /init"
	cp -a -- "${ROOT}/." "$STAGE/" || die "failed to copy minirootfs"
	mkdir -p -- "$STAGE"/{proc,run,sys,tmp,dev}
	# /dev must stay empty: devtmpfs is mounted over it by the kernel
	# (CONFIG_DEVTMPFS_MOUNT=y), and stale nodes confuse nothing but bloat.
	rm -rf -- "$STAGE/dev" && mkdir -p -- "$STAGE/dev"
fi

# ------------------------------------------------------------------ /init ---
# NB: shebang is the *real* binary, not /bin/sh, so a lost symlink cannot
# brick the boot.  The kernel runs this as PID 1 via rdinit=/init.
cat > "$STAGE/init" <<'INIT_EOF'
#!/bin/busybox sh
# TheKernel Phase 2b inner-guest init.  PID 1, no firmware, no disk, no net.
export PATH=/bin:/sbin:/usr/bin:/usr/sbin
BB=/bin/busybox

# Everything below writes to fd 1, which the kernel wired to /dev/console,
# i.e. the ttyS0 the QEMU monitor/`-serial stdio` is attached to.
$BB mount -t proc     proc     /proc 2>/dev/null
$BB mount -t sysfs    sysfs    /sys  2>/dev/null
$BB mount -t devtmpfs devtmpfs /dev  2>/dev/null

echo "INNER_ALPINE_RELEASE $(cat /etc/alpine-release 2>/dev/null)"
echo "INNER_UNAME $($BB uname -srm 2>/dev/null)"
echo "INNER_KERNEL $(cat /proc/sys/kernel/osrelease 2>/dev/null)"
echo "INNER_CMDLINE $(cat /proc/cmdline 2>/dev/null)"
echo "INNER_INIT_PID $$"
echo "INNER_BUSYBOX $($BB 2>&1 | head -1)"
echo "INNER_USERLAND_OK"
echo "INNER_SHUTDOWN_BEGIN"

$BB sync
# Real ACPI S5 poweroff.  busybox poweroff -f issues reboot(RB_POWER_OFF)
# directly (no init signalling), which the kernel turns into an ACPI S5
# transition; QEMU then exits the process with status 0.
$BB poweroff -f
echo "INNER_POWEROFF_FALLBACK_SYSRQ"
echo o > /proc/sysrq-trigger 2>/dev/null
echo "INNER_POWEROFF_FAILED"
# Absolute last resort: never spin, just block.  The outer runner will time out.
while :; do $BB sleep 3600; done
INIT_EOF
chmod 0755 -- "$STAGE/init"
[ -x "$STAGE/init" ] || die "failed to create executable /init"

# ------------------------------------------------------------- pack cpio ----
rm -f -- "${OUT}.tmp"

# Normalise mtimes BEFORE packing.  `cpio --reproducible` only makes the
# archive device/inode independent -- it does NOT zero timestamps, so without
# this step two builds a minute apart produce different bytes.
say "normalising mtimes to epoch 0"
find "$STAGE" -exec touch -h --date='@0' -- {} + \
	|| die "failed to normalise mtimes under $STAGE"
# Verify it actually took: nothing may be newer than epoch 1.
_bad="$(find "$STAGE" -newermt '@1' -print -quit 2>/dev/null || true)"
[ -z "$_bad" ] || die "mtime normalisation incomplete: $_bad"

say "packing cpio newc (mode=${MODE}, compress=${COMPRESS})"
(
	cd "$STAGE"
	# --reproducible = --device-independent --renumber-inodes (mtimes are
	# already normalised above); -R 0:0 makes every entry root-owned even
	# though we are building unprivileged.
	find . -mindepth 1 -printf '%P\0' \
		| LC_ALL=C sort -z \
		| cpio --null --create --format=newc --reproducible --owner=0:0 --quiet
) > "${OUT}.tmp" || die "cpio failed"

case "$COMPRESS" in
	gzip) gzip -9n -c -- "${OUT}.tmp" > "$OUT" || die "gzip failed" ;;
	lz4)  lz4 -l -9 -c -- "${OUT}.tmp" > "$OUT" || die "lz4 failed" ;;
	none) mv -- "${OUT}.tmp" "$OUT" ;;
esac
rm -f -- "${OUT}.tmp"

[ -s "$OUT" ] || die "produced archive is empty: $OUT"
say "wrote $OUT ($(stat -c %s -- "$OUT") bytes)"
sha256sum -- "$OUT" >&2
