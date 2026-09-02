#!/bin/sh
# Install the guest-side DRM and evdev UAPI probes into every graphics image.
set -eu

target=${1:?Buildroot target directory is required}
source_dir=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
compiler=$HOST_DIR/bin/x86_64-buildroot-linux-gnu-gcc

[ -x "$compiler" ]
install -d -m 0755 "$target/usr/local/bin"

"$compiler" -O2 -std=c11 -Wall -Wextra -Werror \
  -I"$STAGING_DIR/usr/include/libdrm" \
  "$source_dir/tests/guest/graphics/drm-uapi-oracle.c" \
  -o "$target/usr/local/bin/drm-uapi-oracle"
"$compiler" -O2 -std=c11 -Wall -Wextra -Werror \
  "$source_dir/tests/guest/graphics/evdev-uapi-oracle.c" \
  -o "$target/usr/local/bin/evdev-uapi-oracle"
"$compiler" -O2 -std=c11 -Wall -Wextra -Werror \
  -I"$STAGING_DIR/usr/include/libdrm" \
  "$source_dir/tests/guest/graphics/device-lease-probe.c" \
  -o "$target/usr/local/bin/thekernel-device-lease-probe"
chmod 0755 "$target/usr/local/bin/drm-uapi-oracle" \
  "$target/usr/local/bin/evdev-uapi-oracle" \
  "$target/usr/local/bin/thekernel-device-lease-probe"
