#!/bin/sh
set -eu
target=$1
source_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
scanner=$HOST_DIR/bin/wayland-scanner
xml=$target/usr/share/wayland-protocols/stable/xdg-shell/xdg-shell.xml
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
