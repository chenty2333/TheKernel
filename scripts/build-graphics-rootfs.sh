#!/usr/bin/env bash
# Build a pinned Buildroot graphics rootfs.  It never changes the normal
# test-rootfs builder and keeps downloads in a caller-visible cache.
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
PINS=$REPO_ROOT/config/graphics/pins.env
COMMON=$REPO_ROOT/config/graphics/common.config

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

usage() {
    cat <<'EOF'
Usage: scripts/build-graphics-rootfs.sh --flavor {headless-abi-smoke|q35-software-desktop} --output DIR [options]

Options:
  --buildroot-dir DIR   pre-checked-out Buildroot 2025.02.2 source tree
  --fetch-buildroot     fetch the exact pinned revision into .state/buildroot
  --download-dir DIR    verified Buildroot package-download cache
  --source-only         download verified package sources, do not build
  --host-deps-dir DIR   optional task-local Perl dependency prefix
  --perl-module-root DIR trusted installed Perl-module tree used to seed the prefix
  --tmpdir DIR          Buildroot temporary directory (defaults below --output)
  --check               validate this wrapper and checked-in configurations only

Buildroot package downloads are rejected unless their checked-in .hash file
matches (BR2_DOWNLOAD_FORCE_CHECK_HASHES=y).  --fetch-buildroot is explicit;
it checks the fetched checkout against config/graphics/pins.env.
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
    q35-software-desktop) flavor_overlay=q35-software-desktop; fragment=$REPO_ROOT/config/graphics/$flavor.fragment ;;
    '') [ "$check_only" -eq 1 ] || { printf '%s\n' '--flavor is required' >&2; exit 2; }; flavor=headless-abi-smoke; flavor_overlay=headless; fragment=$REPO_ROOT/config/graphics/headless-abi-smoke.fragment ;;
    *) printf 'unsupported graphics flavor: %s\n' "$flavor" >&2; exit 2 ;;
esac

validate_checked_in() {
    [[ "$BUILDROOT_REVISION" =~ ^[0-9a-f]{40}$ ]] || { printf '%s\n' 'Buildroot revision is not immutable' >&2; return 1; }
    for path in \
        "$fragment" \
        "$REPO_ROOT/config/graphics/users.table" \
        "$REPO_ROOT/config/graphics/overlay/common/usr/local/bin/graphics-abi-smoke" \
        "$REPO_ROOT/config/graphics/overlay/common/usr/local/bin/graphics-session" \
        "$REPO_ROOT/config/graphics/overlay/common/etc/init.d/S70seatd" \
        "$REPO_ROOT/config/graphics/overlay/common/etc/init.d/S80weston" \
        "$REPO_ROOT/config/graphics/overlay/common/etc/udev/rules.d/71-thekernel-graphics.rules" \
        "$REPO_ROOT/config/graphics/overlay/common/etc/weston/weston-headless.ini" \
        "$REPO_ROOT/config/graphics/overlay/common/etc/weston/weston-drm.ini" \
        "$REPO_ROOT/config/graphics/q35-wayland-color-client.c" \
        "$REPO_ROOT/config/graphics/build-q35-wayland-client.sh" \
        "$REPO_ROOT/config/graphics/overlay/$flavor_overlay/etc/thekernel-graphics-flavor"; do
        [ -r "$path" ] || { printf 'missing graphics input: %s\n' "$path" >&2; return 1; }
    done
    for path in \
        "$REPO_ROOT/config/graphics/overlay/common/usr/local/bin/graphics-abi-smoke" \
        "$REPO_ROOT/config/graphics/overlay/common/usr/local/bin/graphics-session" \
        "$REPO_ROOT/config/graphics/overlay/common/etc/init.d/S70seatd" \
        "$REPO_ROOT/config/graphics/overlay/common/etc/init.d/S80weston"; do
        [ -x "$path" ] || { printf 'graphics executable is not executable: %s\n' "$path" >&2; return 1; }
    done
    grep -qx 'BR2_DOWNLOAD_FORCE_CHECK_HASHES=y' "$COMMON"
    grep -qx 'BR2_INIT_BUSYBOX=y' "$COMMON"
    ! grep -qx 'BR2_INIT_NONE=y' "$COMMON"
    grep -qx 'BR2_ROOTFS_USERS_TABLES="@REPO_ROOT@/config/graphics/users.table"' "$COMMON"
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
    grep -qx 'SUBSYSTEM=="input", KERNEL=="event\[0-9\]\*", GROUP="input", MODE="0660"' "$REPO_ROOT/config/graphics/overlay/common/etc/udev/rules.d/71-thekernel-graphics.rules"
    grep -qx '.*-g seat' "$REPO_ROOT/config/graphics/overlay/common/etc/init.d/S70seatd"
    grep -qx '.*-c "\$USER".*' "$REPO_ROOT/config/graphics/overlay/common/etc/init.d/S80weston"
    grep -qx 'FLAVOR_FILE=/etc/thekernel-graphics-flavor' "$REPO_ROOT/config/graphics/overlay/common/etc/init.d/S80weston"
    ! grep -q 'THEKERNEL_GRAPHICS_FLAVOR' "$REPO_ROOT/config/graphics/overlay/common/etc/init.d/S80weston"
    grep -qx 'export XDG_RUNTIME_DIR="\$runtime_dir"' "$REPO_ROOT/config/graphics/overlay/common/usr/local/bin/graphics-session"
    case "$flavor" in
        headless-abi-smoke)
            grep -qx 'BR2_PACKAGE_WESTON_DEFAULT_HEADLESS=y' "$fragment"
            grep -qx '# BR2_PACKAGE_WESTON_DRM is not set' "$fragment"
            grep -qx 'headless-abi-smoke' "$REPO_ROOT/config/graphics/overlay/headless/etc/thekernel-graphics-flavor"
            [ -x "$REPO_ROOT/config/graphics/overlay/headless/etc/init.d/S90graphics-abi-smoke" ]
            ;;
        q35-software-desktop)
            grep -qx 'BR2_PACKAGE_WESTON_DEFAULT_DRM=y' "$fragment"
            grep -qx 'BR2_PACKAGE_MESA3D_GALLIUM_DRIVER_SWRAST=y' "$fragment"
            grep -qx 'BR2_PACKAGE_MESA3D_GALLIUM_DRIVER_VIRGL=y' "$fragment"
            grep -qx 'q35-software-desktop' "$REPO_ROOT/config/graphics/overlay/q35-software-desktop/etc/thekernel-graphics-flavor"
            ;;
    esac
}

validate_build_output() {
    local target=$output/target resolved=$output/.config libseat backend accounts_image debugfs
    [ -r "$resolved" ]
    grep -qx 'BR2_PACKAGE_SEATD_DAEMON=y' "$resolved"
    grep -qx 'BR2_PACKAGE_LIBINPUT=y' "$resolved"
    ! grep -qx 'BR2_PACKAGE_SYSTEMD_LOGIND=y' "$resolved"
    [ -x "$target/etc/init.d/S10udev" ]
    [ -x "$target/etc/init.d/S70seatd" ]
    [ -x "$target/etc/init.d/S80weston" ]
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
    "$debugfs" -R 'cat /etc/group' "$accounts_image" 2>/dev/null | grep -q '^seat:.*:weston\(,\|$\)'
    "$debugfs" -R 'cat /etc/group' "$accounts_image" 2>/dev/null | grep -q '^render:.*:weston\(,\|$\)'
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
            grep -qx 'BR2_PACKAGE_MESA3D_GALLIUM_DRIVER_SWRAST=y' "$resolved"
            grep -qx 'BR2_PACKAGE_MESA3D_GALLIUM_DRIVER_VIRGL=y' "$resolved"
            [ -x "$target/etc/init.d/S90q35-weston-smoke" ]
            [ -x "$target/usr/local/bin/q35-wayland-color-client" ]
            find "$target/usr/lib" -type f -name 'kms_swrast_dri.so' -print -quit | grep -q .
            find "$target/usr/lib" -type f -name 'virgl_dri.so' -print -quit | grep -q .
            find "$target/usr/lib" -type f -name 'libEGL.so*' -print -quit | grep -q .
            find "$target/usr/lib" -type f -name 'libGLESv2.so*' -print -quit | grep -q .
            find "$target/usr/lib" -type f -name 'libgbm.so*' -print -quit | grep -q .
            ;;
    esac
    find "$target/usr/lib" -type f -name "$backend" -print -quit | grep -q .
    printf '%s\n' 'graphics rootfs userspace configuration: OK'
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
actual_revision=$(git -C "$buildroot_dir" rev-parse HEAD 2>/dev/null || true)
if [ "$actual_revision" != "$BUILDROOT_REVISION" ]; then
    if [ "$fetch_buildroot" -eq 1 ]; then
        git -C "$buildroot_dir" fetch --depth=1 origin "$BUILDROOT_REVISION"
        git -C "$buildroot_dir" checkout --detach "$BUILDROOT_REVISION"
        actual_revision=$(git -C "$buildroot_dir" rev-parse HEAD)
    fi
fi
[ "$actual_revision" = "$BUILDROOT_REVISION" ] || { printf 'Buildroot revision mismatch: expected %s, got %s\n' "$BUILDROOT_REVISION" "$actual_revision" >&2; exit 1; }
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
    validate_build_output
    printf 'graphics rootfs output: %s/images/rootfs.ext2\n' "$output"
fi
