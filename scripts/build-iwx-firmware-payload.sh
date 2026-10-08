#!/usr/bin/env bash
set -euo pipefail

SOURCE_DIR=
OUTPUT=

usage() {
    cat <<'EOF'
Usage: scripts/build-iwx-firmware-payload.sh --source-dir LINUX_FIRMWARE_TREE --output ROOTFS_STAGE

Stage only the AX211 So/GF API 89 ucode and matching PNVM into a rootfs tree.
The caller supplies a redistributable linux-firmware checkout/package tree;
this script does not fetch or retain firmware binaries.
EOF
}

while (($#)); do
    case "$1" in
        --source-dir) SOURCE_DIR=${2:-}; shift 2 ;;
        --output) OUTPUT=${2:-}; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) printf 'unknown argument: %s\n' "$1" >&2; exit 2 ;;
    esac
done

[ -n "$SOURCE_DIR" ] || { printf '%s\n' '--source-dir is required' >&2; exit 2; }
[ -n "$OUTPUT" ] || { printf '%s\n' '--output is required' >&2; exit 2; }
SOURCE_DIR=$(realpath -e "$SOURCE_DIR")
OUTPUT=$(realpath -m "$OUTPUT")

UCODE=iwlwifi-so-a0-gf-a0-89.ucode
PNVM=iwlwifi-so-a0-gf-a0.pnvm
LICENSE=LICENCE.iwlwifi_firmware
for name in "$UCODE" "$PNVM" "$LICENSE"; do
    [ -s "$SOURCE_DIR/$name" ] || {
        printf 'missing/nonempty linux-firmware input: %s/%s\n' "$SOURCE_DIR" "$name" >&2
        exit 1
    }
done

python3 - "$SOURCE_DIR/$UCODE" "$SOURCE_DIR/$PNVM" <<'PY'
import pathlib
import sys

ucode = pathlib.Path(sys.argv[1]).read_bytes()
pnvm = pathlib.Path(sys.argv[2]).stat().st_size
if len(ucode) < 88 or int.from_bytes(ucode[4:8], "little") != 0x0A4C5749:
    raise SystemExit("iwlwifi API 89 ucode has an invalid TLV header")
api = (int.from_bytes(ucode[72:76], "little") >> 8) & 0xFF
if api != 89:
    raise SystemExit(f"expected iwlwifi firmware API 89, found API {api}")
if len(ucode) > 4 * 1024 * 1024 or pnvm > 2 * 1024 * 1024:
    raise SystemExit("firmware input exceeds driver request size cap")
PY

install -d "$OUTPUT/lib/firmware" "$OUTPUT/usr/share/licenses/linux-firmware"
install -m 0644 "$SOURCE_DIR/$UCODE" "$OUTPUT/lib/firmware/$UCODE"
install -m 0644 "$SOURCE_DIR/$PNVM" "$OUTPUT/lib/firmware/$PNVM"
install -m 0644 "$SOURCE_DIR/$LICENSE" \
    "$OUTPUT/usr/share/licenses/linux-firmware/$LICENSE"
printf 'iwx firmware staged: API 89 ucode, matching So/GF PNVM, Intel firmware grant\n' >&2
