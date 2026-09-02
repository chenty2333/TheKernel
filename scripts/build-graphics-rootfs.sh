#!/usr/bin/env bash
# Build a Buildroot graphics rootfs.  It never changes the normal
# test-rootfs builder and keeps downloads in a caller-visible cache.
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
PINS=$REPO_ROOT/config/graphics/pins.env
COMMON=$REPO_ROOT/config/graphics/common.config
BUSYBOX_FRAGMENT=$REPO_ROOT/config/graphics/busybox.fragment

flavor=
output=
buildroot_dir=${THEKERNEL_BUILDROOT_DIR:-}
download_dir=${THEKERNEL_GRAPHICS_DL_DIR:-$REPO_ROOT/.state/graphics-downloads}
fetch_buildroot=0
source_only=0
check_only=0
host_deps_dir=${THEKERNEL_GRAPHICS_HOST_DEPS_DIR:-}
perl_module_root=${THEKERNEL_GRAPHICS_PERL_MODULE_ROOT:-}
tmpdir=${THEKERNEL_GRAPHICS_TMPDIR:-}
fault=

usage() {
    cat <<'EOF'
Usage: scripts/build-graphics-rootfs.sh --flavor {headless-abi-smoke|q35-graphics-seatd|q35-software-desktop|q35-graphics-benchmark|q35-venus-desktop|q35-graphics-logind} --output DIR [options]

Options:
  --buildroot-dir DIR   pre-checked-out Buildroot 2026.05.2 source tree
  --fetch-buildroot     fetch Buildroot into .state/buildroot
  --download-dir DIR    Buildroot package-download cache
  --source-only         download package sources, do not build
  --host-deps-dir DIR   optional task-local Perl dependency prefix
  --perl-module-root DIR trusted installed Perl-module tree used to seed the prefix
  --tmpdir DIR          Buildroot temporary directory (defaults below --output)
  --fault NAME          inject one graphics fault action into a benchmark rootfs
  --check               validate this wrapper and checked-in configurations only

--fetch-buildroot is explicit.
EOF
}

while (($#)); do
    case "$1" in
        --flavor) flavor=${2:-}; shift 2 ;;
        --output) output=${2:-}; shift 2 ;;
        --buildroot-dir) buildroot_dir=${2:-}; shift 2 ;;
        --fetch-buildroot) fetch_buildroot=1; shift ;;
        --download-dir) download_dir=${2:-}; shift 2 ;;
        --source-only) source_only=1; shift ;;
        --host-deps-dir) host_deps_dir=${2:-}; shift 2 ;;
        --perl-module-root) perl_module_root=${2:-}; shift 2 ;;
        --tmpdir) tmpdir=${2:-}; shift 2 ;;
        --fault) fault=${2:-}; shift 2 ;;
        --check) check_only=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) printf 'unknown argument: %s\n' "$1" >&2; exit 2 ;;
    esac
done

[ -r "$PINS" ] && [ -r "$COMMON" ] || { printf '%s\n' 'graphics configuration is incomplete' >&2; exit 1; }
# shellcheck source=/dev/null
. "$PINS"
case "$flavor" in
    headless-abi-smoke) flavor_overlay=headless; fragment=$REPO_ROOT/config/graphics/$flavor.fragment ;;
    q35-graphics-seatd) flavor_overlay=q35-graphics-seatd; fragment=$REPO_ROOT/config/graphics/$flavor.fragment ;;
    q35-software-desktop) flavor_overlay=q35-software-desktop; fragment=$REPO_ROOT/config/graphics/$flavor.fragment ;;
    q35-graphics-benchmark) flavor_overlay=q35-graphics-benchmark; fragment=$REPO_ROOT/config/graphics/$flavor.fragment ;;
    q35-venus-desktop) flavor_overlay=q35-venus-desktop; fragment=$REPO_ROOT/config/graphics/$flavor.fragment ;;
    q35-graphics-logind) flavor_overlay=q35-graphics-logind; fragment=$REPO_ROOT/config/graphics/$flavor.fragment ;;
    '') [ "$check_only" -eq 1 ] || { printf '%s\n' '--flavor is required' >&2; exit 2; }; flavor=headless-abi-smoke; flavor_overlay=headless; fragment=$REPO_ROOT/config/graphics/headless-abi-smoke.fragment ;;
    *) printf 'unsupported graphics flavor: %s\n' "$flavor" >&2; exit 2 ;;
esac

case "$fault" in ''|modeset|client-crash|vt-switch|weston-restart|input-hotplug) ;; *) printf 'unsupported graphics fault: %s\n' "$fault" >&2; exit 2 ;; esac

validate_checked_in() {
    [ -r "$BUSYBOX_FRAGMENT" ] || { printf 'missing BusyBox config fragment: %s\n' "$BUSYBOX_FRAGMENT" >&2; return 1; }
    grep -qx 'CONFIG_STAT=y' "$BUSYBOX_FRAGMENT"
    grep -qx 'CONFIG_FEATURE_STAT_FORMAT=y' "$BUSYBOX_FRAGMENT"
    grep -qx 'BR2_PACKAGE_BUSYBOX_CONFIG_FRAGMENT_FILES="@REPO_ROOT@/config/graphics/busybox.fragment"' "$COMMON"
    if [ "$flavor" = q35-graphics-logind ]; then
        validate_logind_checked_in
        return
    fi
    for path in \
        "$fragment" \
        "$REPO_ROOT/config/graphics/users.table" \
        "$REPO_ROOT/config/graphics/overlay/common/usr/local/bin/graphics-abi-smoke" \
        "$REPO_ROOT/config/graphics/overlay/common/usr/local/bin/graphics-session" \
        "$REPO_ROOT/config/graphics/overlay/common/usr/local/bin/thekernel-xwayland-glamor" \
        "$REPO_ROOT/config/graphics/overlay/common/etc/init.d/S70seatd" \
        "$REPO_ROOT/config/graphics/overlay/common/etc/init.d/S80weston" \
        "$REPO_ROOT/config/graphics/overlay/common/etc/udev/rules.d/71-thekernel-graphics.rules" \
        "$REPO_ROOT/config/graphics/overlay/common/etc/weston/weston-headless.ini" \
        "$REPO_ROOT/config/graphics/overlay/common/etc/weston/weston-drm.ini" \
        "$REPO_ROOT/config/graphics/build-guest-tools.sh" \
        "$REPO_ROOT/tests/guest/graphics/drm-uapi-oracle.c" \
        "$REPO_ROOT/tests/guest/graphics/evdev-uapi-oracle.c" \
        "$REPO_ROOT/tests/guest/graphics/device-lease-probe.c" \
        "$REPO_ROOT/config/graphics/q35-wayland-color-client.c" \
        "$REPO_ROOT/config/graphics/q35-virgl-render-oracle.c" \
        "$REPO_ROOT/config/graphics/build-q35-wayland-client.sh" \
        "$REPO_ROOT/config/graphics/q35-wayland-vulkan-client.c" \
        "$REPO_ROOT/config/graphics/build-q35-wayland-vulkan-client.sh" \
        "$REPO_ROOT/config/graphics/overlay/$flavor_overlay/etc/thekernel-graphics-flavor"; do
        [ -r "$path" ] || { printf 'missing graphics input: %s\n' "$path" >&2; return 1; }
    done
    for path in \
        "$REPO_ROOT/config/graphics/overlay/common/usr/local/bin/graphics-abi-smoke" \
        "$REPO_ROOT/config/graphics/overlay/common/usr/local/bin/graphics-session" \
        "$REPO_ROOT/config/graphics/overlay/common/usr/local/bin/thekernel-xwayland-glamor" \
        "$REPO_ROOT/config/graphics/overlay/common/etc/init.d/S70seatd" \
        "$REPO_ROOT/config/graphics/overlay/common/etc/init.d/S80weston" \
        "$REPO_ROOT/config/graphics/build-guest-tools.sh"; do
        [ -x "$path" ] || { printf 'graphics executable is not executable: %s\n' "$path" >&2; return 1; }
    done
    grep -qx 'BR2_INIT_BUSYBOX=y' "$COMMON"
    ! grep -qx 'BR2_INIT_NONE=y' "$COMMON"
    grep -qx 'BR2_ROOTFS_USERS_TABLES="@REPO_ROOT@/config/graphics/users.table"' "$COMMON"
    grep -qx 'BR2_TARGET_ROOTFS_EXT2_MKFS_OPTIONS="-O ^64bit,^metadata_csum_seed,^orphan_file"' "$COMMON"
    grep -qx 'BR2_PACKAGE_LIBDRM=y' "$COMMON"
    grep -qx 'BR2_ROOTFS_DEVICE_CREATION_DYNAMIC_EUDEV=y' "$COMMON"
    grep -qx 'BR2_PACKAGE_EUDEV=y' "$COMMON"
    grep -qx 'BR2_PACKAGE_LIBEVDEV=y' "$COMMON"
    grep -qx 'BR2_PACKAGE_LIBINPUT=y' "$COMMON"
    grep -qx 'BR2_PACKAGE_SEATD=y' "$COMMON"
    grep -qx 'BR2_PACKAGE_SEATD_DAEMON=y' "$COMMON"
    grep -qx 'BR2_PACKAGE_WAYLAND=y' "$COMMON"
    grep -qx 'BR2_PACKAGE_PIXMAN=y' "$COMMON"
    grep -qx 'BR2_PACKAGE_WESTON=y' "$COMMON"
    grep -qx 'weston -1 weston -1 !\* /var/lib/weston /bin/sh seat,render Weston compositor' "$REPO_ROOT/config/graphics/users.table"
    grep -qx 'SUBSYSTEM=="drm", KERNEL=="card\[0-9\]\*", GROUP="video", MODE="0660"' "$REPO_ROOT/config/graphics/overlay/common/etc/udev/rules.d/71-thekernel-graphics.rules"
    grep -qx 'SUBSYSTEM=="drm", KERNEL=="renderD\[0-9\]\*", GROUP="render", MODE="0660"' "$REPO_ROOT/config/graphics/overlay/common/etc/udev/rules.d/71-thekernel-graphics.rules"
    grep -qx 'SUBSYSTEM=="graphics", KERNEL=="fb\[0-9\]\*", GROUP="video", MODE="0660"' "$REPO_ROOT/config/graphics/overlay/common/etc/udev/rules.d/71-thekernel-graphics.rules"
    grep -Fqx 'SUBSYSTEM=="input", KERNEL=="event[0-9]*", ENV{ID_SEAT}="seat0", ENV{WL_SEAT}="default", GROUP="input", MODE="0660"' "$REPO_ROOT/config/graphics/overlay/common/etc/udev/rules.d/71-thekernel-graphics.rules"
    grep -qx '.*-g seat' "$REPO_ROOT/config/graphics/overlay/common/etc/init.d/S70seatd"
    grep -Fqx '    if ! chown root:seat /run/seatd.sock || ! chmod 0660 /run/seatd.sock; then' "$REPO_ROOT/config/graphics/overlay/common/etc/init.d/S70seatd"
    grep -qx '.*-c "\$USER".*' "$REPO_ROOT/config/graphics/overlay/common/etc/init.d/S80weston"
    grep -qx 'FLAVOR_FILE=/etc/thekernel-graphics-flavor' "$REPO_ROOT/config/graphics/overlay/common/etc/init.d/S80weston"
    ! grep -q 'THEKERNEL_GRAPHICS_FLAVOR' "$REPO_ROOT/config/graphics/overlay/common/etc/init.d/S80weston"
    grep -qx 'export XDG_RUNTIME_DIR="\$runtime_dir"' "$REPO_ROOT/config/graphics/overlay/common/usr/local/bin/graphics-session"
    grep -qx 'export LIBSEAT_BACKEND=seatd' "$REPO_ROOT/config/graphics/overlay/common/usr/local/bin/graphics-session"
    grep -qx '/usr/local/bin/drm-uapi-oracle' "$REPO_ROOT/config/graphics/overlay/common/usr/local/bin/graphics-abi-smoke"
    grep -qx '/usr/local/bin/evdev-uapi-oracle' "$REPO_ROOT/config/graphics/overlay/common/usr/local/bin/graphics-abi-smoke"
    case "$flavor" in
        headless-abi-smoke)
            grep -qx 'BR2_ROOTFS_POST_BUILD_SCRIPT="@REPO_ROOT@/config/graphics/build-guest-tools.sh"' "$fragment"
            grep -qx 'BR2_PACKAGE_WESTON_DEFAULT_HEADLESS=y' "$fragment"
            grep -qx '# BR2_PACKAGE_WESTON_DRM is not set' "$fragment"
            grep -qx 'headless-abi-smoke' "$REPO_ROOT/config/graphics/overlay/headless/etc/thekernel-graphics-flavor"
            [ -x "$REPO_ROOT/config/graphics/overlay/headless/etc/init.d/S90graphics-abi-smoke" ]
            ;;
        q35-software-desktop)
            grep -qx 'BR2_ROOTFS_POST_BUILD_SCRIPT="@REPO_ROOT@/config/graphics/build-guest-tools.sh @REPO_ROOT@/config/graphics/build-q35-wayland-client.sh"' "$fragment"
            grep -qx 'BR2_PACKAGE_WESTON_DEFAULT_DRM=y' "$fragment"
            grep -qx 'BR2_PACKAGE_MESA3D_GALLIUM_DRIVER_SOFTPIPE=y' "$fragment"
            grep -qx 'BR2_PACKAGE_MESA3D_GALLIUM_DRIVER_VIRGL=y' "$fragment"
            grep -qx 'BR2_PACKAGE_MESA3D_OPENGL_GLX=y' "$fragment"
            grep -qx 'BR2_PACKAGE_KMSCUBE=y' "$fragment"
            grep -qx 'BR2_PACKAGE_PYTHON3=y' "$fragment"
            grep -qx 'BR2_PACKAGE_PIGLIT=y' "$fragment"
            grep -qx 'BR2_PACKAGE_XORG7=y' "$fragment"
            grep -qx 'BR2_PACKAGE_LIBEPOXY=y' "$fragment"
            grep -qx 'BR2_TARGET_ROOTFS_EXT2_SIZE="3G"' "$fragment"
            grep -qx 'BR2_PACKAGE_WESTON_XWAYLAND=y' "$fragment"
            grep -qx 'xwayland=true' "$REPO_ROOT/config/graphics/overlay/q35-software-desktop/etc/weston/weston-drm.ini"
            grep -qx 'q35-software-desktop' "$REPO_ROOT/config/graphics/overlay/q35-software-desktop/etc/thekernel-graphics-flavor"
            grep -qx '/usr/local/bin/drm-uapi-oracle' "$REPO_ROOT/config/graphics/overlay/q35-software-desktop/etc/init.d/S90q35-weston-smoke"
            grep -qx '/usr/local/bin/evdev-uapi-oracle' "$REPO_ROOT/config/graphics/overlay/q35-software-desktop/etc/init.d/S90q35-weston-smoke"
            grep -Fq "Seat opened with backend 'seatd'" "$REPO_ROOT/config/graphics/overlay/q35-software-desktop/etc/init.d/S90q35-weston-smoke"
            [ -x "$REPO_ROOT/config/graphics/overlay/q35-software-desktop/etc/init.d/S75q35-virgl-kmscube" ]
            [ -x "$REPO_ROOT/config/graphics/overlay/q35-software-desktop/usr/local/bin/q35-piglit-quick" ]
            [ -x "$REPO_ROOT/config/graphics/overlay/q35-software-desktop/usr/local/bin/q35-piglit-result-check" ]
            [ -x "$REPO_ROOT/config/graphics/overlay/q35-software-desktop/usr/local/bin/q35-virgl-workloads" ]
            ;;
        q35-graphics-seatd)
            grep -qx 'BR2_ROOTFS_POST_BUILD_SCRIPT="@REPO_ROOT@/config/graphics/build-guest-tools.sh @REPO_ROOT@/config/graphics/build-q35-wayland-client.sh @REPO_ROOT@/config/graphics/build-q35-wayland-vulkan-client.sh"' "$fragment"
            grep -qx 'BR2_PACKAGE_WESTON_DEFAULT_DRM=y' "$fragment"
            grep -qx 'BR2_PACKAGE_MESA3D_GALLIUM_DRIVER_SOFTPIPE=y' "$fragment"
            grep -qx 'BR2_PACKAGE_MESA3D_GALLIUM_DRIVER_VIRGL=y' "$fragment"
            grep -qx 'BR2_PACKAGE_MESA3D_VULKAN_DRIVER_VIRTIO=y' "$fragment"
            grep -qx 'BR2_PACKAGE_MESA3D_OPENGL_GLX=y' "$fragment"
            grep -qx 'BR2_PACKAGE_PIGLIT=y' "$fragment"
            grep -qx 'BR2_PACKAGE_XORG7=y' "$fragment"
            grep -qx 'BR2_PACKAGE_LIBEPOXY=y' "$fragment"
            grep -qx 'BR2_TARGET_ROOTFS_EXT2_SIZE="3G"' "$fragment"
            grep -qx 'BR2_PACKAGE_WESTON_SHELL_DESKTOP=y' "$fragment"
            grep -qx 'BR2_PACKAGE_WESTON_XWAYLAND=y' "$fragment"
            grep -qx 'xwayland=true' "$REPO_ROOT/config/graphics/overlay/q35-software-desktop/etc/weston/weston-drm.ini"
            grep -qx 'BR2_PACKAGE_FOOT=y' "$fragment"
            grep -qx 'q35-graphics-seatd' "$REPO_ROOT/config/graphics/overlay/q35-graphics-seatd/etc/thekernel-graphics-flavor"
            [ -x "$REPO_ROOT/config/graphics/overlay/q35-software-desktop/etc/init.d/S90q35-weston-smoke" ]
            ;;
        q35-graphics-benchmark)
            grep -qx 'BR2_ROOTFS_POST_BUILD_SCRIPT="@REPO_ROOT@/config/graphics/build-q35-graphics-benchmark-tools.sh"' "$fragment"
            grep -qx 'BR2_PACKAGE_WESTON_DEFAULT_DRM=y' "$fragment"
            grep -qx 'BR2_PACKAGE_MESA3D_VULKAN_DRIVER_VIRTIO=y' "$fragment"
            grep -qx 'BR2_PACKAGE_VULKAN_LOADER=y' "$fragment"
            grep -qx 'BR2_PACKAGE_VULKAN_TOOLS=y' "$fragment"
            grep -qx 'BR2_TARGET_ROOTFS_EXT2_SIZE="512M"' "$fragment"
            grep -qx 'q35-software-desktop' "$REPO_ROOT/config/graphics/overlay/q35-graphics-benchmark/etc/thekernel-graphics-flavor"
            [ -x "$REPO_ROOT/config/graphics/overlay/q35-graphics-benchmark/etc/init.d/S90q35-graphics-benchmark" ]
            [ -x "$REPO_ROOT/config/graphics/build-q35-graphics-benchmark-tools.sh" ]
            [ -r "$REPO_ROOT/config/graphics/q35-drm-modeset-restore.c" ]
            [ -r "$REPO_ROOT/config/graphics/q35-wayland-egl-benchmark.c" ]
            [ -r "$REPO_ROOT/config/graphics/q35-wayland-vulkan-benchmark.c" ]
            ;;
        q35-venus-desktop)
            grep -qx 'BR2_PACKAGE_MESA3D_VULKAN_DRIVER_VIRTIO=y' "$fragment"
            grep -qx 'BR2_PACKAGE_VULKAN_LOADER=y' "$fragment"
            grep -qx 'BR2_PACKAGE_VULKAN_TOOLS=y' "$fragment"
            grep -qx '# BR2_PACKAGE_MESA3D_GALLIUM_DRIVER_VIRGL is not set' "$fragment"
            grep -qx 'q35-venus-desktop' "$REPO_ROOT/config/graphics/overlay/q35-venus-desktop/etc/thekernel-graphics-flavor"
            [ -x "$REPO_ROOT/config/graphics/overlay/q35-venus-desktop/etc/init.d/S90q35-venus-smoke" ]
            ;;
    esac
}

validate_logind_checked_in() {
    for path in \
        "$fragment" \
        "$REPO_ROOT/config/graphics/logind-users.table" \
        "$REPO_ROOT/config/graphics/build-guest-tools.sh" \
        "$REPO_ROOT/tests/guest/graphics/drm-uapi-oracle.c" \
        "$REPO_ROOT/tests/guest/graphics/evdev-uapi-oracle.c" \
        "$REPO_ROOT/config/graphics/overlay/q35-graphics-logind/etc/thekernel-graphics-flavor" \
        "$REPO_ROOT/config/graphics/overlay/q35-graphics-logind/etc/environment" \
        "$REPO_ROOT/config/graphics/overlay/q35-graphics-logind/etc/pam.d/login" \
        "$REPO_ROOT/config/graphics/overlay/q35-graphics-logind/etc/systemd/system/thekernel-sway-session.target" \
        "$REPO_ROOT/config/graphics/overlay/q35-graphics-logind/etc/systemd/system/thekernel-logind-cycle.service" \
        "$REPO_ROOT/config/graphics/overlay/q35-graphics-logind/etc/systemd/system/getty@tty1.service.d/thekernel-alice.conf" \
        "$REPO_ROOT/config/graphics/overlay/q35-graphics-logind/etc/systemd/system/getty@tty2.service.d/thekernel-bob.conf" \
        "$REPO_ROOT/config/graphics/overlay/q35-graphics-logind/usr/local/bin/thekernel-sway-session" \
        "$REPO_ROOT/config/graphics/overlay/q35-graphics-logind/usr/local/bin/thekernel-logind-cycle"; do
        [ -r "$path" ] || { printf 'missing graphics input: %s\n' "$path" >&2; return 1; }
    done
    for program in thekernel-sway-session thekernel-logind-cycle; do
        [ -x "$REPO_ROOT/config/graphics/overlay/q35-graphics-logind/usr/local/bin/$program" ] || {
            printf 'logind program is not executable: %s\n' "$program" >&2; return 1;
        }
    done
    grep -qx 'BR2_INIT_SYSTEMD=y' "$fragment"
    grep -qx 'BR2_PACKAGE_SYSTEMD_LOGIND=y' "$fragment"
    grep -qx 'BR2_PACKAGE_DBUS=y' "$fragment"
    grep -qx 'BR2_PACKAGE_LINUX_PAM=y' "$fragment"
    grep -qx 'BR2_PACKAGE_UTIL_LINUX_LOGIN=y' "$fragment"
    grep -qx 'BR2_PACKAGE_ACL=y' "$fragment"
    grep -qx 'BR2_PACKAGE_SWAY=y' "$fragment"
    grep -qx 'BR2_PACKAGE_MESA3D_GALLIUM_DRIVER_VIRGL=y' "$fragment"
    grep -qx 'BR2_PACKAGE_MESA3D_VULKAN_DRIVER_VIRTIO=y' "$fragment"
    grep -qx '# BR2_PACKAGE_SEATD_DAEMON is not set' "$fragment"
    grep -qx '# BR2_PACKAGE_WESTON is not set' "$fragment"
    grep -qx 'LIBSEAT_BACKEND=logind' "$REPO_ROOT/config/graphics/overlay/q35-graphics-logind/etc/environment"
    grep -qx 'session    required   pam_systemd.so' "$REPO_ROOT/config/graphics/overlay/q35-graphics-logind/etc/pam.d/login"
    grep -Fq 'exec dbus-run-session sh -eu -c' "$REPO_ROOT/config/graphics/overlay/q35-graphics-logind/usr/local/bin/thekernel-sway-session"
    grep -Eq '^[[:space:]]*chvt "[$]tty"$' "$REPO_ROOT/config/graphics/overlay/q35-graphics-logind/usr/local/bin/thekernel-logind-cycle"
    grep -Eq '^[[:space:]]*loginctl activate "[$]session"$' "$REPO_ROOT/config/graphics/overlay/q35-graphics-logind/usr/local/bin/thekernel-logind-cycle"
    ! find "$REPO_ROOT/config/graphics/overlay/q35-graphics-logind" -type f -print | grep -Eq '/S70seatd$|/S80weston$'
}

validate_mesa_gallium_output() {
    local target=$1
    [ -e "$target/usr/lib/gbm/dri_gbm.so" ]
    find "$target/usr/lib" -maxdepth 1 -type f -name 'libgallium-*.so' -print -quit | grep -q .
}

validate_busybox_stat_output() {
    local target=$1 busybox_config
    busybox_config=$(find "$output/build" -mindepth 2 -maxdepth 2 -path '*/busybox-*/.config' -print -quit)
    [ -r "$busybox_config" ] || {
        printf '%s\n' 'resolved BusyBox configuration missing from graphics output' >&2
        return 1
    }
    grep -qx 'CONFIG_STAT=y' "$busybox_config"
    grep -qx 'CONFIG_FEATURE_STAT_FORMAT=y' "$busybox_config"
    [ -x "$target/usr/bin/stat" ] || [ -x "$target/bin/stat" ] || {
        printf '%s\n' 'BusyBox stat applet missing from graphics target' >&2
        return 1
    }
}

validate_build_output() {
    local target=$output/target resolved=$output/.config libseat backend accounts_image debugfs
    [ -r "$resolved" ]
    if [ "$flavor" = q35-graphics-logind ]; then
        validate_logind_build_output
        return
    fi
    grep -qx 'BR2_PACKAGE_SEATD_DAEMON=y' "$resolved"
    grep -qx 'BR2_TARGET_ROOTFS_EXT2_MKFS_OPTIONS="-O ^64bit,^metadata_csum_seed,^orphan_file"' "$resolved"
    grep -qx 'BR2_PACKAGE_LIBINPUT=y' "$resolved"
    ! grep -qx 'BR2_PACKAGE_SYSTEMD_LOGIND=y' "$resolved"
    [ -x "$target/etc/init.d/S10udevd" ]
    [ -x "$target/etc/init.d/S70seatd" ]
    [ -x "$target/etc/init.d/S80weston" ]
    validate_busybox_stat_output "$target"
    [ -x "$target/usr/local/bin/drm-uapi-oracle" ]
    [ -x "$target/usr/local/bin/evdev-uapi-oracle" ]
    [ -f "$target/etc/udev/rules.d/71-thekernel-graphics.rules" ]
    # Buildroot applies BR2_ROOTFS_USERS_TABLES in its fakeroot filesystem-image
    # staging area, not in O/target.  Validate the produced image, where these
    # account entries actually exist.
    accounts_image=$output/images/rootfs.ext2
    [ -r "$accounts_image" ] || { printf 'generated rootfs image missing: %s\n' "$accounts_image" >&2; return 1; }
    debugfs=$output/host/sbin/debugfs
    [ -x "$debugfs" ] || debugfs=$(command -v debugfs || true)
    [ -n "$debugfs" ] || { printf '%s\n' 'debugfs is required to validate rootfs accounts' >&2; return 1; }
    "$debugfs" -R 'cat /etc/passwd' "$accounts_image" 2>/dev/null | grep -q '^weston:'
    "$debugfs" -R 'cat /etc/group' "$accounts_image" 2>/dev/null | grep -q '^seat:'
    "$debugfs" -R 'cat /etc/group' "$accounts_image" 2>/dev/null \
        | grep -Eq '^seat:[^:]*:[^:]*:([^,]+,)*weston(,[^,]+)*$'
    "$debugfs" -R 'cat /etc/group' "$accounts_image" 2>/dev/null \
        | grep -Eq '^render:[^:]*:[^:]*:([^,]+,)*weston(,[^,]+)*$'
    command -v readelf >/dev/null || { printf '%s\n' 'readelf is required to validate graphics linkage' >&2; return 1; }
    libseat=$(find "$target/usr/lib" -maxdepth 1 -type f -name 'libseat.so.*' -print -quit)
    [ -n "$libseat" ] || { printf '%s\n' 'libseat shared object missing from target' >&2; return 1; }
    readelf -d "$libseat" | grep -q 'SONAME.*libseat\.so'
    ! readelf -d "$libseat" | grep -q 'Shared library: \[libsystemd\.so'
    case "$flavor" in
        headless-abi-smoke) backend=headless-backend.so ;;
        q35-software-desktop)
            backend=drm-backend.so
            grep -qx 'BR2_PACKAGE_WESTON_DRM=y' "$resolved"
            grep -qx 'BR2_PACKAGE_MESA3D_GALLIUM_DRIVER_SOFTPIPE=y' "$resolved"
            grep -qx 'BR2_PACKAGE_MESA3D_GALLIUM_DRIVER_VIRGL=y' "$resolved"
            grep -qx 'BR2_PACKAGE_MESA3D_OPENGL_GLX=y' "$resolved"
            grep -qx 'BR2_PACKAGE_KMSCUBE=y' "$resolved"
            grep -qx 'BR2_PACKAGE_PIGLIT=y' "$resolved"
            grep -qx 'BR2_PACKAGE_XORG7=y' "$resolved"
            grep -qx 'BR2_PACKAGE_LIBEPOXY=y' "$resolved"
            grep -qx 'BR2_TARGET_ROOTFS_EXT2_SIZE="3G"' "$resolved"
            grep -qx 'BR2_PACKAGE_WESTON_XWAYLAND=y' "$resolved"
            [ -x "$target/etc/init.d/S90q35-weston-smoke" ]
            [ -x "$target/usr/local/bin/q35-wayland-color-client" ]
            [ -x "$target/usr/local/bin/q35-virgl-render-oracle" ]
            [ -x "$target/usr/local/bin/q35-piglit-quick" ]
            [ -x "$target/usr/local/bin/q35-piglit-result-check" ]
            [ -x "$target/usr/bin/piglit" ]
            [ -r "$target/usr/lib/piglit/tests/quick.meta.xml" ]
            [ -x "$target/usr/local/bin/q35-virgl-workloads" ]
            [ -x "$target/usr/bin/kmscube" ]
            validate_mesa_gallium_output "$target"
            find "$target/usr/lib" -type f -name 'libEGL.so*' -print -quit | grep -q .
            find "$target/usr/lib" -type f -name 'libGLESv2.so*' -print -quit | grep -q .
            find "$target/usr/lib" -type f -name 'libgbm.so*' -print -quit | grep -q .
            ;;
        q35-graphics-seatd)
            backend=drm-backend.so
            grep -qx 'BR2_PACKAGE_WESTON_DRM=y' "$resolved"
            grep -qx 'BR2_PACKAGE_MESA3D_GALLIUM_DRIVER_SOFTPIPE=y' "$resolved"
            grep -qx 'BR2_PACKAGE_MESA3D_GALLIUM_DRIVER_VIRGL=y' "$resolved"
            grep -qx 'BR2_PACKAGE_MESA3D_OPENGL_GLX=y' "$resolved"
            grep -qx 'BR2_PACKAGE_MESA3D_VULKAN_DRIVER_VIRTIO=y' "$resolved"
            grep -qx 'BR2_PACKAGE_PIGLIT=y' "$resolved"
            grep -qx 'BR2_PACKAGE_XORG7=y' "$resolved"
            grep -qx 'BR2_PACKAGE_LIBEPOXY=y' "$resolved"
            grep -qx 'BR2_TARGET_ROOTFS_EXT2_SIZE="3G"' "$resolved"
            grep -qx 'BR2_PACKAGE_VULKAN_LOADER=y' "$resolved"
            grep -qx 'BR2_PACKAGE_VULKAN_TOOLS=y' "$resolved"
            grep -qx 'BR2_PACKAGE_WESTON_XWAYLAND=y' "$resolved"
            grep -qx 'BR2_PACKAGE_WESTON_SHELL_DESKTOP=y' "$resolved"
            grep -qx 'BR2_PACKAGE_FOOT=y' "$resolved"
            [ -x "$target/etc/init.d/S90q35-weston-smoke" ]
            [ -x "$target/usr/local/bin/q35-wayland-color-client" ]
            [ -x "$target/usr/local/bin/q35-wayland-vulkan-client" ]
            [ -x "$target/usr/local/bin/q35-piglit-quick" ]
            [ -x "$target/usr/local/bin/q35-piglit-result-check" ]
            [ -x "$target/usr/local/bin/q35-virgl-workloads" ]
            [ -x "$target/usr/bin/piglit" ]
            [ -r "$target/usr/lib/piglit/tests/quick.meta.xml" ]
            [ -r "$target/usr/lib/piglit/tests/quick.meta.xml" ]
            [ -x "$target/usr/bin/vulkaninfo" ]
            validate_mesa_gallium_output "$target"
            find "$target/usr" -type f -name 'virtio_icd*.json' -print -quit | grep -q .
            ;;
        q35-graphics-benchmark)
            backend=drm-backend.so
            grep -qx 'BR2_PACKAGE_WESTON_DRM=y' "$resolved"
            grep -qx 'BR2_PACKAGE_MESA3D_VULKAN_DRIVER_VIRTIO=y' "$resolved"
            grep -qx 'BR2_PACKAGE_VULKAN_LOADER=y' "$resolved"
            grep -qx 'BR2_PACKAGE_VULKAN_TOOLS=y' "$resolved"
            grep -qx 'BR2_TARGET_ROOTFS_EXT2_SIZE="512M"' "$resolved"
            [ -x "$target/etc/init.d/S90q35-graphics-benchmark" ]
            [ -x "$target/usr/local/bin/q35-wayland-shm-client" ]
            [ -x "$target/usr/local/bin/q35-wayland-egl-benchmark-client" ]
            [ -x "$target/usr/local/bin/q35-wayland-vulkan-benchmark-client" ]
            [ -x "$target/usr/local/bin/q35-drm-modeset-restore" ]
            [ -x "$target/usr/bin/vulkaninfo" ]
            find "$target/usr" -type f -name 'virtio_icd*.json' -print -quit | grep -q .
            ;;
        q35-venus-desktop)
            backend=drm-backend.so
            grep -qx 'BR2_PACKAGE_MESA3D_VULKAN_DRIVER_VIRTIO=y' "$resolved"
            grep -qx 'BR2_PACKAGE_VULKAN_LOADER=y' "$resolved"
            grep -qx 'BR2_PACKAGE_VULKAN_TOOLS=y' "$resolved"
            ! grep -qx 'BR2_PACKAGE_MESA3D_GALLIUM_DRIVER_VIRGL=y' "$resolved"
            [ -x "$target/etc/init.d/S90q35-venus-smoke" ]
            [ -x "$target/usr/local/bin/q35-wayland-vulkan-client" ]
            [ -x "$target/usr/bin/vulkaninfo" ]
            find "$target/usr" -type f -name 'virtio_icd*.json' -print -quit | grep -q .
            find "$target/usr/lib" -type f -name 'libvulkan.so*' -print -quit | grep -q .
            ;;
    esac
    find "$target/usr/lib" -type f -name "$backend" -print -quit | grep -q .
    printf '%s\n' 'graphics rootfs userspace configuration: OK'
}

validate_logind_build_output() {
    local target=$output/target resolved=$output/.config libseat accounts_image debugfs
    grep -qx 'BR2_INIT_SYSTEMD=y' "$resolved"
    grep -qx 'BR2_TARGET_ROOTFS_EXT2_MKFS_OPTIONS="-O ^64bit,^metadata_csum_seed,^orphan_file"' "$resolved"
    grep -qx 'BR2_PACKAGE_SYSTEMD_LOGIND=y' "$resolved"
    grep -qx 'BR2_PACKAGE_DBUS=y' "$resolved"
    grep -qx 'BR2_PACKAGE_LINUX_PAM=y' "$resolved"
    grep -qx 'BR2_PACKAGE_UTIL_LINUX_LOGIN=y' "$resolved"
    grep -qx 'BR2_PACKAGE_ACL=y' "$resolved"
    grep -qx 'BR2_PACKAGE_SWAY=y' "$resolved"
    grep -qx 'BR2_PACKAGE_MESA3D_GALLIUM_DRIVER_VIRGL=y' "$resolved"
    grep -qx 'BR2_PACKAGE_MESA3D_VULKAN_DRIVER_VIRTIO=y' "$resolved"
    ! grep -qx 'BR2_PACKAGE_SEATD_DAEMON=y' "$resolved"
    ! grep -qx 'BR2_PACKAGE_WESTON=y' "$resolved"
    [ -x "$target/lib/systemd/systemd" ]
    [ -x "$target/usr/lib/systemd/systemd-udevd" ]
    [ -x "$target/usr/lib/systemd/systemd-logind" ]
    [ -x "$target/usr/bin/login" ]
    [ -x "$target/usr/bin/sway" ]
    [ -x "$target/usr/bin/getfacl" ]
    [ -r "$target/etc/pam.d/login" ]
    [ -r "$target/usr/local/bin/thekernel-sway-session" ]
    [ -x "$target/usr/local/bin/thekernel-logind-cycle" ]
    [ -x "$target/usr/local/bin/thekernel-device-lease-probe" ]
    [ -x "$target/usr/local/bin/q35-wayland-shm-client" ]
    validate_busybox_stat_output "$target"
    [ ! -e "$target/usr/bin/seatd" ]
    [ ! -e "$target/etc/init.d/S70seatd" ]
    [ ! -e "$target/etc/init.d/S80weston" ]
    accounts_image=$output/images/rootfs.ext2
    [ -r "$accounts_image" ] || { printf 'generated rootfs image missing: %s\n' "$accounts_image" >&2; return 1; }
    debugfs=$output/host/sbin/debugfs
    [ -x "$debugfs" ] || debugfs=$(command -v debugfs || true)
    [ -n "$debugfs" ] || { printf '%s\n' 'debugfs is required to validate rootfs accounts' >&2; return 1; }
    "$debugfs" -R 'cat /etc/passwd' "$accounts_image" 2>/dev/null | grep -q '^alice:'
    "$debugfs" -R 'cat /etc/passwd' "$accounts_image" 2>/dev/null | grep -q '^bob:'
    "$debugfs" -R 'cat /etc/group' "$accounts_image" 2>/dev/null | grep -q '^render:'
    command -v readelf >/dev/null || { printf '%s\n' 'readelf is required to validate graphics linkage' >&2; return 1; }
    libseat=$(find "$target/usr/lib" -maxdepth 1 -type f -name 'libseat.so.*' -print -quit)
    [ -n "$libseat" ] || { printf '%s\n' 'libseat shared object missing from target' >&2; return 1; }
    readelf -d "$libseat" | grep -q 'Shared library: \[libsystemd\.so'
    validate_mesa_gallium_output "$target"
    find "$target/usr" -type f -name 'virtio_icd*.json' -print -quit | grep -q .
    printf '%s\n' 'graphics logind rootfs userspace configuration: OK'
}

validate_checked_in
if [ "$check_only" -eq 1 ]; then
    printf '%s\n' 'graphics rootfs configuration: OK'
    exit 0
fi
[ -n "$output" ] || { printf '%s\n' '--output is required' >&2; exit 2; }

if [ -z "$buildroot_dir" ]; then
    buildroot_dir=$REPO_ROOT/.state/buildroot/buildroot-$BUILDROOT_VERSION
fi
for command in git make sed; do command -v "$command" >/dev/null || { printf 'required command not found: %s\n' "$command" >&2; exit 1; }; done
if [ "$fetch_buildroot" -eq 1 ] && [ ! -d "$buildroot_dir/.git" ]; then
    mkdir -p "$(dirname -- "$buildroot_dir")"
    git clone --no-checkout "$BUILDROOT_URL" "$buildroot_dir"
fi
[ -d "$buildroot_dir" ] || { printf 'Buildroot source unavailable: %s (pass --buildroot-dir or --fetch-buildroot)\n' "$buildroot_dir" >&2; exit 1; }
output=$(realpath -m "$output")
mkdir -p "$output" "$download_dir"
if [ -z "$tmpdir" ]; then tmpdir=$output/tmp; fi
if [ -n "$perl_module_root" ]; then
    if [ -z "$host_deps_dir" ]; then host_deps_dir=$(dirname -- "$download_dir")/graphics-host-deps; fi
    "$SCRIPT_DIR/setup-graphics-local-deps.sh" --prefix "$host_deps_dir" --module-root "$perl_module_root"
fi
if [ -n "$host_deps_dir" ]; then
    [ -x "$host_deps_dir/bin/perl" ] || {
        printf '%s\n' 'local Buildroot Perl modules unavailable: seed --host-deps-dir or pass --perl-module-root' >&2
        exit 1
    }
    export PERL5LIB="$host_deps_dir/lib/perl5${PERL5LIB:+:$PERL5LIB}"
    export PATH="$host_deps_dir/bin:$PATH"
fi
mkdir -p "$tmpdir"
export TMPDIR="$tmpdir"
generated_config=$output/.thekernel-graphics.config
sed "s|@REPO_ROOT@|$REPO_ROOT|g" "$COMMON" >"$generated_config"
sed "s|@REPO_ROOT@|$REPO_ROOT|g" "$fragment" >>"$generated_config"

make -C "$buildroot_dir" O="$output" BR2_DL_DIR="$download_dir" defconfig BR2_DEFCONFIG="$generated_config"
make -C "$buildroot_dir" O="$output" BR2_DL_DIR="$download_dir" olddefconfig
if [ "$source_only" -eq 1 ]; then
    make -C "$buildroot_dir" O="$output" BR2_DL_DIR="$download_dir" source
    printf 'verified package sources cached in: %s\n' "$download_dir"
else
    make -C "$buildroot_dir" O="$output" BR2_DL_DIR="$download_dir"
    if [ -n "$fault" ]; then
        [ "$flavor" = q35-graphics-benchmark ] || { printf '%s\n' '--fault requires q35-graphics-benchmark' >&2; exit 2; }
        printf '%s\n' "$fault" >"$output/target/etc/thekernel-graphics-fault"
        make -C "$buildroot_dir" O="$output" BR2_DL_DIR="$download_dir" rootfs-ext2-rebuild
    fi
    validate_build_output
    printf 'graphics rootfs output: %s/images/rootfs.ext2\n' "$output"
fi
