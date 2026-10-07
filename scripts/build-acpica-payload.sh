#!/usr/bin/env bash
# Original static ACPICA user-tool builder, Apache-2.0.
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
OUTPUT= CC_TOOL=
while (($#)); do
    case "$1" in
        --output) OUTPUT=${2:?}; shift 2;;
        --cc) CC_TOOL=${2:?}; shift 2;;
        *) echo 'usage: build-acpica-payload.sh --output DIR --cc C_COMPILER' >&2; exit 2;;
    esac
done
[ -n "$OUTPUT" ] && [ -n "$CC_TOOL" ] || exit 2
STATE=${THEKERNEL_STATE_DIR:-$HOME/.cache/thekernel-targets}
CACHE=${THEKERNEL_SOURCE_CACHE:-$STATE/source-cache}
BUILD=$STATE/acpica-tools
PYTHONPATH="$ROOT" python3 - "$OUTPUT" "$CACHE" "$BUILD" <<'PY'
import sys
from pathlib import Path
from tools.product_state import validate_storage
for path in sys.argv[1:]: validate_storage(Path(path))
PY
VERSION=20260930
NAME=acpica-unix-$VERSION.tar.gz
DIGEST=aa18901b92e30749be0edc3081c8d550c61fce4fa37546fc6a65d367a4ae71a5
mkdir -p "$CACHE" "$BUILD" "$OUTPUT"
if [ ! -f "$CACHE/$NAME" ]; then
    curl -fL --retry 3 "https://github.com/acpica/acpica/releases/download/$VERSION/$NAME" -o "$CACHE/$NAME.part"
    mv "$CACHE/$NAME.part" "$CACHE/$NAME"
fi
printf '%s  %s\n' "$DIGEST" "$CACHE/$NAME" | sha256sum -c --status
SOURCE=$BUILD/acpica-unix-$VERSION
if [ ! -d "$SOURCE" ]; then tar -xzf "$CACHE/$NAME" -C "$BUILD"; fi
if [ ! -f "$BUILD/complete-$VERSION" ]; then
    printf '#!/usr/bin/env bash\nexec %q -static "$@"\n' "$CC_TOOL" > "$BUILD/cc-static"
    chmod +x "$BUILD/cc-static"
    nice -n 10 make -C "$SOURCE" -j"${CARGO_BUILD_JOBS:-6}" iasl acpidump CC="$BUILD/cc-static"
    : > "$BUILD/complete-$VERSION"
fi
for binary in acpidump iasl; do
    source=$SOURCE/generate/unix/bin/$binary
    # Must not accidentally stage a host-linked tool after an earlier build.
    if readelf -l "$source" | grep -q INTERP; then echo "$binary is not static" >&2; exit 1; fi
    install -s -m 0755 "$source" "$OUTPUT/$binary"
done
install -m 0644 "$SOURCE/LICENSE.BSD-3-Clause" "$OUTPUT/ACPICA-LICENSE.BSD-3-Clause"
