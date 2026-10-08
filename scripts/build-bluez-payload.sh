#!/usr/bin/env bash
# Stage signed, pinned Alpine BlueZ tools and their runtime closure.
set -euo pipefail
SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
STATE_ROOT=${THEKERNEL_STATE_DIR:-$HOME/.cache/thekernel-targets}
SOURCE_CACHE=${THEKERNEL_SOURCE_CACHE:-$STATE_ROOT/source-cache}
STATE_ROOT=$(realpath -m "$STATE_ROOT")
case "$STATE_ROOT" in /|"$HOME"|"$REPO_ROOT") echo 'refusing unsafe state directory' >&2; exit 2 ;; esac
OUTPUT=
while (($#)); do
    case "$1" in
        --output) OUTPUT=${2:-}; shift 2 ;;
        --source-cache) SOURCE_CACHE=${2:-}; shift 2 ;;
        -h|--help) echo 'usage: build-bluez-payload.sh --output DIR [--source-cache DIR]'; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done
[ -n "$OUTPUT" ] || { echo '--output is required' >&2; exit 2; }
OUTPUT=$(realpath -m "$OUTPUT")
case "$OUTPUT" in
    "$STATE_ROOT"/guest-tools/bluez) ;;
    *) echo 'output must be the bluez guest-tools payload under THEKERNEL_STATE_DIR' >&2; exit 2 ;;
esac
for command in curl sha256sum tar install realpath; do command -v "$command" >/dev/null; done

MINI=alpine-minirootfs-3.24.1-x86_64.tar.gz
MINI_SHA=41f73e3cf5fa919b8aa5ca6b30dc48f0da2720776d7423e2a7748211456fe081
APK_CACHE="$SOURCE_CACHE/apk-bluez"
BUILD_ROOT="$STATE_ROOT/bluez-payload-build"
mkdir -p "$APK_CACHE" "$BUILD_ROOT"
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
fetch "$MINI" "$MINI_SHA" "https://dl-cdn.alpinelinux.org/alpine/v3.24/releases/x86_64/$MINI" "$APK_CACHE/$MINI"
BOOTSTRAP="$BUILD_ROOT/bootstrap"
rm -rf -- "$BOOTSTRAP"
mkdir -p "$BOOTSTRAP"
tar --no-same-owner -xzf "$APK_CACHE/$MINI" -C "$BOOTSTRAP"
APK=("$BOOTSTRAP/lib/ld-musl-x86_64.so.1" --library-path "$BOOTSTRAP/lib:$BOOTSTRAP/usr/lib" \
    "$BOOTSTRAP/sbin/apk" --root="$BOOTSTRAP" --cache-dir="$APK_CACHE/cache" \
    --keys-dir="$BOOTSTRAP/etc/apk/keys")
PACKAGES=()
while IFS=$'\t' read -r name checksum url license origin; do
    [[ "$name" == \#* || -z "$name" ]] && continue
    fetch "$name" "$checksum" "$url" "$APK_CACHE/$name"
    PACKAGES+=("$APK_CACHE/$name")
done < "$REPO_ROOT/config/guest-bluez-apk-pins.tsv"
[ ${#PACKAGES[@]} -gt 0 ] || { echo 'empty BlueZ package closure' >&2; exit 1; }
# Verify Alpine signatures in addition to the checked-in reproducibility pins.
"${APK[@]}" verify "${PACKAGES[@]}"
STAGE="$BUILD_ROOT/stage"
rm -rf -- "$STAGE" "$OUTPUT"
mkdir -p "$STAGE"
"${APK[@]}" extract --no-chown --destination="$STAGE" "${PACKAGES[@]}"
LOADER="$STAGE/lib/ld-musl-x86_64.so.1"
for binary in "$STAGE/usr/lib/bluetooth/bluetoothd" "$STAGE/usr/bin/btmgmt" \
    "$STAGE/usr/bin/bluetoothctl" "$STAGE/usr/bin/btmon" "$STAGE/usr/bin/hciconfig"; do
    [ -x "$binary" ] || { echo "missing BlueZ executable: $binary" >&2; exit 1; }
done
"$LOADER" --library-path "$STAGE/lib:$STAGE/usr/lib" "$STAGE/usr/lib/bluetooth/bluetoothd" --version | grep -F '5.86'
"$LOADER" --library-path "$STAGE/lib:$STAGE/usr/lib" "$STAGE/usr/bin/btmgmt" --version | grep -F '5.86'
"$LOADER" --library-path "$STAGE/lib:$STAGE/usr/lib" "$STAGE/usr/bin/bluetoothctl" --version | grep -F '5.86'
"$LOADER" --library-path "$STAGE/lib:$STAGE/usr/lib" "$STAGE/usr/bin/btmon" --version | grep -F '5.86'
mkdir -p "$STAGE/opt/thekernel-tools"
install -m 0644 "$REPO_ROOT/config/guest-bluez-apk-pins.tsv" "$STAGE/opt/thekernel-tools/BLUEZ-PACKAGES.tsv"
printf '%s\n' 'Alpine v3.24 x86_64 signed packages; BlueZ 5.86-r2; runtime closure pinned by SHA256.' \
    > "$STAGE/opt/thekernel-tools/BLUEZ-MANIFEST"
mkdir -p "$(dirname -- "$OUTPUT")" "$OUTPUT"
cp -a "$STAGE/." "$OUTPUT/"
echo "build-bluez-payload: verified and staged ${#PACKAGES[@]} signed packages into $OUTPUT"
