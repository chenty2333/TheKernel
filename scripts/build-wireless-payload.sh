#!/usr/bin/env bash
# Stage signed Alpine iw, wpa_supplicant, wireless-regdb and pinned closure.
# No host package installation, chroot, package scripts or guest network access.
set -euo pipefail
SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
STATE_ROOT=${THEKERNEL_STATE_DIR:-$HOME/.cache/thekernel-targets}
SOURCE_CACHE=${THEKERNEL_SOURCE_CACHE:-$STATE_ROOT/source-cache}
OUTPUT=
while (($#)); do
    case "$1" in
        --output) OUTPUT=${2:-}; shift 2 ;;
        --source-cache) SOURCE_CACHE=${2:-}; shift 2 ;;
        -h|--help) echo 'usage: build-wireless-payload.sh --output DIR [--source-cache DIR]'; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done
[ -n "$OUTPUT" ] || { echo '--output is required' >&2; exit 2; }
OUTPUT=$(realpath -m "$OUTPUT")
case "$OUTPUT" in /|"$HOME"|"$REPO_ROOT") echo 'refusing unsafe output directory' >&2; exit 2 ;; esac
for command in curl sha256sum tar install realpath; do command -v "$command" >/dev/null; done
mkdir -p "$SOURCE_CACHE/apk-wireless" "$STATE_ROOT/wireless-payload-build"
BUILD_ROOT="$STATE_ROOT/wireless-payload-build"
MINI=alpine-minirootfs-3.24.1-x86_64.tar.gz
MINI_SHA=41f73e3cf5fa919b8aa5ca6b30dc48f0da2720776d7423e2a7748211456fe081
fetch() {
    local name=$1 expected=$2 url=$3 destination=$4
    if [ ! -f "$destination" ]; then
        curl --fail --location --retry 3 --output "$destination.part" "$url"
        printf '%s  %s\n' "$expected" "$destination.part" | sha256sum --check --status || {
            rm -f "$destination.part"; echo "checksum failed: $name" >&2; exit 1;
        }
        mv "$destination.part" "$destination"
    fi
    printf '%s  %s\n' "$expected" "$destination" | sha256sum --check --status || {
        echo "cached source checksum failed: $name" >&2; exit 1;
    }
}
fetch "$MINI" "$MINI_SHA" "https://dl-cdn.alpinelinux.org/alpine/v3.24/releases/x86_64/$MINI" "$SOURCE_CACHE/$MINI"
BOOTSTRAP="$BUILD_ROOT/bootstrap"
mkdir -p "$BOOTSTRAP"
tar --no-same-owner -xzf "$SOURCE_CACHE/$MINI" -C "$BOOTSTRAP"
APK=("$BOOTSTRAP/lib/ld-musl-x86_64.so.1" --library-path "$BOOTSTRAP/lib:$BOOTSTRAP/usr/lib" "$BOOTSTRAP/sbin/apk" --keys-dir "$BOOTSTRAP/etc/apk/keys")
PACKAGES=()
while IFS=$'\t' read -r name checksum url license origin; do
    [[ "$name" == \#* || -z "$name" ]] && continue
    fetch "$name" "$checksum" "$url" "$SOURCE_CACHE/apk-wireless/$name"
    PACKAGES+=("$SOURCE_CACHE/apk-wireless/$name")
done < "$REPO_ROOT/config/guest-wireless-apk-pins.tsv"
[ ${#PACKAGES[@]} -gt 0 ] || { echo 'empty wireless package closure' >&2; exit 1; }
"${APK[@]}" verify "${PACKAGES[@]}"
STAGE="$BUILD_ROOT/stage"
mkdir -p "$STAGE"
"${APK[@]}" extract --no-chown --destination "$STAGE" "${PACKAGES[@]}"
[ -x "$STAGE/usr/sbin/iw" ] && [ -x "$STAGE/sbin/wpa_supplicant" ]
[ -s "$STAGE/lib/firmware/regulatory.db" ] && [ -s "$STAGE/lib/firmware/regulatory.db.p7s" ]
LOADER="$STAGE/lib/ld-musl-x86_64.so.1"
"$LOADER" --library-path "$STAGE/lib:$STAGE/usr/lib" "$STAGE/usr/sbin/iw" --version | grep -F 'iw version 6.17'
"$LOADER" --library-path "$STAGE/lib:$STAGE/usr/lib" "$STAGE/sbin/wpa_supplicant" -v | grep -F 'wpa_supplicant v2.11'
mkdir -p "$STAGE/opt/thekernel-tools"
install -m 0644 "$REPO_ROOT/config/guest-wireless-apk-pins.tsv" "$STAGE/opt/thekernel-tools/WIRELESS-PACKAGES.tsv"
printf 'wireless payload: Alpine 3.24.1 bootstrap, signed pinned v3.24 x86_64 packages\niw 6.17-r0\nwpa_supplicant 2.11-r4\nwireless-regdb 2025.10.07-r0\n' > "$STAGE/opt/thekernel-tools/WIRELESS-MANIFEST"
rm -rf "$OUTPUT"
mkdir -p "$OUTPUT"
cp -a "$STAGE/." "$OUTPUT/"
echo "build-wireless-payload: staged ${#PACKAGES[@]} signed packages into $OUTPUT"
