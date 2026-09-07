#!/bin/sh
# Install the guest-side DRM and evdev UAPI probes into every graphics image.
# Probes are discovered from tests/guest/graphics/*.c; the case table carries
# the libdrm include and the thekernel- prefixed guest install names.
set -eu

target=${1:?Buildroot target directory is required}
source_dir=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
compiler=$HOST_DIR/bin/x86_64-buildroot-linux-gnu-gcc

[ -x "$compiler" ]
install -d -m 0755 "$target/usr/local/bin" "$target/etc/thekernel"
# Graphics device oracles also run under the canonical boot-shell profile.
install -m 0755 "$source_dir/tests/guest/shell-init.sh" \
    "$target/etc/thekernel/shell-init.sh"

installed=
for source in "$source_dir/tests/guest/graphics/"*.c; do
    name=${source##*/}
    name=${name%.c}
    cflags=
    output=$name
    case "$name" in
        drm-uapi-oracle)
            cflags=-I$STAGING_DIR/usr/include/libdrm
            ;;
        device-lease-probe)
            cflags=-I$STAGING_DIR/usr/include/libdrm
            output=thekernel-device-lease-probe
            ;;
    esac
    "$compiler" -O2 -std=c11 -Wall -Wextra -Werror $cflags \
      "$source" \
      -o "$target/usr/local/bin/$output"
    installed="$installed $target/usr/local/bin/$output"
done
[ -n "$installed" ] || { echo "no graphics probe sources found" >&2; exit 1; }
# Intentional word splitting: each installed path is space-joined above.
# shellcheck disable=SC2086
chmod 0755 $installed

# The interactive desktop lets its unprivileged user request a normal init
# shutdown. The desktop permission table grants only this fixed helper setuid.
if grep -qx q35-software-desktop "$target/etc/thekernel-graphics-flavor"; then
    "$compiler" -O2 -std=c11 -Wall -Wextra -Werror \
        "$source_dir/config/graphics/desktop-poweroff.c" \
        -o "$target/usr/local/bin/thekernel-desktop-poweroff"
    chmod 0755 "$target/usr/local/bin/thekernel-desktop-poweroff"
fi
