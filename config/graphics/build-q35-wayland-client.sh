#!/bin/sh
set -eu
target=$1
source_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
scanner=$HOST_DIR/bin/wayland-scanner
xml=$STAGING_DIR/usr/share/wayland-protocols/stable/xdg-shell/xdg-shell.xml
header=$BASE_DIR/q35-xdg-shell-client-protocol.h
code=$BASE_DIR/q35-xdg-shell-protocol.c
[ -x "$scanner" ] && [ -r "$xml" ]
"$scanner" client-header "$xml" "$header"
"$scanner" private-code "$xml" "$code"
"$HOST_DIR/bin/x86_64-buildroot-linux-gnu-gcc" -O2 -Wall -Wextra -Werror \
  -I"$STAGING_DIR/usr/include" -I"$BASE_DIR" \
  "$source_dir/q35-wayland-color-client.c" "$code" \
  -L"$STAGING_DIR/usr/lib" -Wl,-z,defs \
  -lwayland-egl -lwayland-client -lEGL -lGLESv2 \
  -o "$target/usr/local/bin/q35-wayland-color-client"
chmod 0755 "$target/usr/local/bin/q35-wayland-color-client"
"$HOST_DIR/bin/x86_64-buildroot-linux-gnu-gcc" -O2 -Wall -Wextra -Werror \
  -I"$STAGING_DIR/usr/include" \
  "$source_dir/q35-xwayland-xcb-client.c" \
  -L"$STAGING_DIR/usr/lib" -Wl,-z,defs -lxcb \
  -o "$target/usr/local/bin/q35-xwayland-xcb-client"
chmod 0755 "$target/usr/local/bin/q35-xwayland-xcb-client"
"$HOST_DIR/bin/x86_64-buildroot-linux-gnu-gcc" -O2 -std=c11 -Wall -Wextra -Werror \
  -I"$STAGING_DIR/usr/include/libdrm" \
  "$source_dir/q35-virgl-render-oracle.c" \
  -o "$target/usr/local/bin/q35-virgl-render-oracle"
chmod 0755 "$target/usr/local/bin/q35-virgl-render-oracle"
"$HOST_DIR/bin/x86_64-buildroot-linux-gnu-gcc" -O2 -Wall -Wextra -Werror \
  -I"$STAGING_DIR/usr/include" -I"$BASE_DIR" \
  "$source_dir/q35-wayland-shm-client.c" "$code" \
  -L"$STAGING_DIR/usr/lib" -Wl,-z,defs -lwayland-client \
  -o "$target/usr/local/bin/q35-wayland-shm-client"
chmod 0755 "$target/usr/local/bin/q35-wayland-shm-client"
"$HOST_DIR/bin/x86_64-buildroot-linux-gnu-gcc" -O2 -Wall -Wextra -Werror \
  -DTHEKERNEL_WIDTH=3840 -DTHEKERNEL_HEIGHT=2160 \
  -I"$STAGING_DIR/usr/include" -I"$BASE_DIR" \
  "$source_dir/q35-wayland-shm-client.c" "$code" \
  -L"$STAGING_DIR/usr/lib" -Wl,-z,defs -lwayland-client \
  -o "$target/usr/local/bin/q35-wayland-benchmark-client"
chmod 0755 "$target/usr/local/bin/q35-wayland-benchmark-client"
