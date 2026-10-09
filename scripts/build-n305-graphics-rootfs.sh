#!/usr/bin/env bash
# Reuse an existing SDL2-enabled Buildroot output; never change its target.
# The N305 PXE image omits the 2 GiB Piglit tree, not a second graphics stack.
set -euo pipefail
REPO=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
[[ $# == 2 ]] || { echo "usage: $0 BUILDROOT_OUTPUT OUTPUT_DIRECTORY" >&2; exit 2; }
SOURCE=$(realpath "$1"); OUT=$(realpath -m "$2")
case "$OUT" in /tmp/*|/dev/shm/*) echo "output must be under /home, not tmpfs" >&2; exit 2;; esac
[[ "$OUT" != "$SOURCE" && "$OUT" != "$SOURCE"/* ]] || exit 2
[[ -x "$SOURCE/host/bin/x86_64-buildroot-linux-gnu-gcc" && -f "$SOURCE/target/usr/lib/libSDL2.so" ]] || { echo "SDL2-enabled Buildroot output is required" >&2; exit 1; }
for variable in \
    THEKERNEL_RTL8168_FIRMWARE_DIR \
    THEKERNEL_IWX_FIRMWARE_DIR \
    THEKERNEL_I915_UC_FIRMWARE_DIR \
    THEKERNEL_I915_DMC_FIRMWARE_DIR; do
    [[ -n "${!variable:-}" ]] || { printf 'N305 graphics rootfs requires %s\n' "$variable" >&2; exit 2; }
done

MESA_IRIS_STAGE=${THEKERNEL_MESA_IRIS_STAGE:-}
if [[ -n "$MESA_IRIS_STAGE" ]]; then
    MESA_IRIS_STAGE=$(realpath -e "$MESA_IRIS_STAGE")
    "$REPO/scripts/build-graphics-rootfs.sh" --flavor n305-iris-smoke --check \
        --mesa-iris-stage "$MESA_IRIS_STAGE"
fi
mkdir -p "$OUT/stage"
tar -C "$SOURCE/target" --exclude='./usr/lib/piglit' --exclude='./usr/bin/piglit' -cf - . | tar --no-same-owner -C "$OUT/stage" -xf -
# User/group creation happens in Buildroot's fakeroot phase, after target/.
# Import the completed image's identity files instead of inventing IDs here.
[[ -f "$SOURCE/images/rootfs.ext2" ]] || { echo "completed Buildroot rootfs.ext2 is required" >&2; exit 1; }
for name in passwd group shadow; do
    debugfs -R "dump /etc/$name $OUT/stage/etc/$name" "$SOURCE/images/rootfs.ext2" >/dev/null 2>&1
done
grep -q '^weston:' "$OUT/stage/etc/passwd" || { echo "completed image has no weston account" >&2; exit 1; }
CC="$SOURCE/host/bin/x86_64-buildroot-linux-gnu-gcc"
SYSROOT="$SOURCE/host/x86_64-buildroot-linux-gnu/sysroot"
"$CC" -O2 -Wall -Wextra -Werror -I"$SYSROOT/usr/include/SDL2" "$REPO/config/graphics/n305-sdl-kms-smoke.c" -lSDL2 -o "$OUT/stage/usr/local/bin/n305-sdl-kms-smoke"
install -m 0755 "$REPO/scripts/ci/n305-dhcp.script" "$OUT/stage/etc/thekernel/n305-dhcp.script"
mkdir -p "$OUT/stage/opt/thekernel-tests/bin"
"$CC" -O2 -Wall -Wextra -Werror "$REPO/tests/guest/tools/netconsole.c" -o "$OUT/stage/opt/thekernel-tests/bin/thekernel-netconsole"
"$CC" -O2 -Wall -Wextra -Werror "$REPO/tests/guest/tools/usb-input-smoke.c" -o "$OUT/stage/opt/thekernel-tests/bin/thekernel-usb-input-smoke"
HOST_DIR="$SOURCE/host" \
STAGING_DIR="$SOURCE/host/x86_64-buildroot-linux-gnu/sysroot" \
    "$REPO/config/graphics/build-guest-tools.sh" "$OUT/stage"
"$REPO/scripts/stage-rootfs-firmware.sh" "$OUT/stage"
if [[ -n "$MESA_IRIS_STAGE" ]]; then
    install -m 0644 \
        "$MESA_IRIS_STAGE/usr/lib/libgallium-26.1.2.so" \
        "$OUT/stage/usr/lib/libgallium-26.1.2.so"
    install -m 0755 \
        "$REPO/config/graphics/overlay/n305-iris-smoke/etc/init.d/S90n305-iris-smoke" \
        "$OUT/stage/etc/thekernel/n305-iris-loader-smoke"
fi
# No persistent home disk, audio device or Virgl dependency on the DUT.
printf 'q35-graphics-seatd\n' > "$OUT/stage/etc/thekernel-graphics-flavor"
: > "$OUT/stage/etc/default/weston"
rm -f "$OUT/stage/etc/weston/weston.ini"
ln -s weston-drm.ini "$OUT/stage/etc/weston/weston.ini"
truncate -s 512M "$OUT/rootfs.ext2"
FAKEROOT="$SOURCE/host/bin/fakeroot"
if [[ -x "$FAKEROOT" ]]; then
    "$FAKEROOT" -- bash -c 'chown -R 0:0 "$1"; "$3" -q -t ext2 -F -d "$1" "$2"' bash "$OUT/stage" "$OUT/rootfs.ext2" "$SOURCE/host/sbin/mke2fs"
else
    echo "Buildroot host/bin/fakeroot missing; cannot assign rootfs ownership without sudo" >&2; exit 1
fi
printf 'N305 graphics rootfs: %s\n' "$OUT/rootfs.ext2"
