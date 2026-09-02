#!/bin/sh
# Benchmark-only guest tools.  Keep the destructive KMS fault helper out of
# the seatd desktop images: it is invoked only after the benchmark has stopped
# Weston and intentionally acquired DRM master.
set -eu

target=${1:?Buildroot target directory is required}
base_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
compiler=$HOST_DIR/bin/x86_64-buildroot-linux-gnu-gcc
scanner=$HOST_DIR/bin/wayland-scanner
xml=$STAGING_DIR/usr/share/wayland-protocols/stable/xdg-shell/xdg-shell.xml

"$base_dir/build-guest-tools.sh" "$target"
"$base_dir/build-q35-wayland-client.sh" "$target"
"$compiler" -O2 -std=c11 -Wall -Wextra -Werror \
  -I"$STAGING_DIR/usr/include" -I"$BASE_DIR" \
  "$base_dir/q35-wayland-egl-benchmark.c" \
  "$BASE_DIR/q35-xdg-shell-protocol.c" \
  -L"$STAGING_DIR/usr/lib" -Wl,-z,defs \
  -lwayland-egl -lwayland-client -lEGL -lGLESv2 \
  -o "$target/usr/local/bin/q35-wayland-egl-benchmark-client"
chmod 0755 "$target/usr/local/bin/q35-wayland-egl-benchmark-client"

venus_header=$BASE_DIR/q35-venus-xdg-shell-client-protocol.h
venus_code=$BASE_DIR/q35-venus-xdg-shell-protocol.c
"$scanner" client-header "$xml" "$venus_header"
"$scanner" private-code "$xml" "$venus_code"
"$compiler" -O2 -std=c11 -Wall -Wextra -Werror \
  -I"$STAGING_DIR/usr/include" -I"$BASE_DIR" \
  "$base_dir/q35-wayland-vulkan-benchmark.c" "$venus_code" \
  -L"$STAGING_DIR/usr/lib" -Wl,-z,defs -lvulkan -lwayland-client \
  -o "$target/usr/local/bin/q35-wayland-vulkan-benchmark-client"
chmod 0755 "$target/usr/local/bin/q35-wayland-vulkan-benchmark-client"

"$compiler" -O2 -std=c11 -Wall -Wextra -Werror \
  -I"$STAGING_DIR/usr/include/libdrm" \
  "$base_dir/q35-drm-modeset-restore.c" \
  -o "$target/usr/local/bin/q35-drm-modeset-restore"
chmod 0755 "$target/usr/local/bin/q35-drm-modeset-restore"
