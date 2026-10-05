#!/usr/bin/env bash
# Stage unmodified signed Alpine cpupower/sensors and their pinned runtime closure.
# No host installation, chroot, package scripts or guest network access.
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
        -h|--help) echo 'usage: build-power-payload.sh --output DIR [--source-cache DIR]'; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done
[ -n "$OUTPUT" ] || { echo '--output is required' >&2; exit 2; }
OUTPUT=$(realpath -m "$OUTPUT")
case "$OUTPUT" in /|"$HOME"|"$REPO_ROOT") echo 'refusing unsafe output directory' >&2; exit 2 ;; esac
for command in curl sha256sum tar install realpath; do command -v "$command" >/dev/null; done
mkdir -p "$SOURCE_CACHE/apk-power" "$STATE_ROOT/power-payload-build"
BUILD_ROOT="$STATE_ROOT/power-payload-build"
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
rm -rf "$BOOTSTRAP"
mkdir -p "$BOOTSTRAP"
tar --no-same-owner -xzf "$SOURCE_CACHE/$MINI" -C "$BOOTSTRAP"
APK=("$BOOTSTRAP/lib/ld-musl-x86_64.so.1" --library-path "$BOOTSTRAP/lib:$BOOTSTRAP/usr/lib" "$BOOTSTRAP/sbin/apk" --keys-dir "$BOOTSTRAP/etc/apk/keys")
PACKAGES=()
while IFS=$'\t' read -r name checksum url license origin; do
    [[ "$name" == \#* || -z "$name" ]] && continue
    fetch "$name" "$checksum" "$url" "$SOURCE_CACHE/apk-power/$name"
    PACKAGES+=("$SOURCE_CACHE/apk-power/$name")
done < "$REPO_ROOT/config/guest-power-apk-pins.tsv"
[ ${#PACKAGES[@]} -gt 0 ] || { echo 'empty power package closure' >&2; exit 1; }
# APK verifies Alpine signatures in addition to the reproducibility pins.
"${APK[@]}" verify "${PACKAGES[@]}"
STAGE="$BUILD_ROOT/stage"
rm -rf "$STAGE"
mkdir -p "$STAGE"
"${APK[@]}" extract --no-chown --destination "$STAGE" "${PACKAGES[@]}"
[ -x "$STAGE/usr/bin/cpupower" ] && [ -x "$STAGE/usr/bin/sensors" ]
LOADER="$STAGE/lib/ld-musl-x86_64.so.1"
"$LOADER" --library-path "$STAGE/lib:$STAGE/usr/lib" "$STAGE/usr/bin/cpupower" --version | grep -F 'cpupower 7.1.5'
"$LOADER" --library-path "$STAGE/lib:$STAGE/usr/lib" "$STAGE/usr/bin/sensors" --version | grep -F 'sensors version 3.6.0'
mkdir -p "$STAGE/opt/thekernel-tools"
install -m 0644 "$REPO_ROOT/config/guest-power-apk-pins.tsv" "$STAGE/opt/thekernel-tools/POWER-PACKAGES.tsv"
printf 'power payload: Alpine 3.24.1 bootstrap, signed pinned v3.24 packages\ncpupower 7.1.5-r0\nlm-sensors 3.6.0-r5\n' > "$STAGE/opt/thekernel-tools/POWER-MANIFEST"
rm -rf "$OUTPUT"
mkdir -p "$OUTPUT"
cp -a "$STAGE/." "$OUTPUT/"
echo "build-power-payload: staged ${#PACKAGES[@]} signed packages into $OUTPUT"
