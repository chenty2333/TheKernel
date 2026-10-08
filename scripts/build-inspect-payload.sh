#!/usr/bin/env bash
# Signed Alpine tools for proc/sys acceptance; not a host installation/container.
set -euo pipefail
export REPO_ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
OUTPUT=
while (($#)); do
    case "$1" in
        --output) OUTPUT=${2:?}; shift 2 ;;
        -h|--help) echo 'build-inspect-payload.sh --output DIR'; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done
[ -n "$OUTPUT" ] || { echo '--output is required' >&2; exit 2; }
STATE=${THEKERNEL_STATE_DIR:-$HOME/.cache/thekernel-targets}
CACHE=${THEKERNEL_SOURCE_CACHE:-$STATE/source-cache}
OUTPUT=$(realpath -m "$OUTPUT")
# Reuse the product storage guard, rather than permitting large tmpfs staging.
PYTHONPATH="$REPO_ROOT" python3 - "$OUTPUT" "$CACHE" <<'PY'
import sys
from pathlib import Path
from tools.product_state import validate_storage
for path in sys.argv[1:]:
    validate_storage(Path(path))
PY
# Only replace an empty directory or our own previous payload, never user files.
if [ -e "$OUTPUT" ]; then
    [ -d "$OUTPUT" ] || { echo 'output is not a directory' >&2; exit 2; }
    if [ -n "$(ls -A "$OUTPUT")" ]; then
        head -1 "$OUTPUT/opt/thekernel-tools/MANIFEST" 2>/dev/null | \
            grep -qx '# Alpine v3.24 x86_64; exact signed package closure for B1 tools.' || {
                echo 'refusing to replace an unrelated output tree' >&2; exit 2;
            }
    fi
fi
mkdir -p "$CACHE" "$(dirname -- "$OUTPUT")"
NAME=alpine-minirootfs-3.24.1-x86_64.tar.gz
ARCHIVE="$CACHE/$NAME"
# Existing nested payload release pin, not a floating distro bootstrap.
if [ ! -f "$ARCHIVE" ]; then
    curl --fail --location --retry 3 --output "$ARCHIVE.part" \
        "https://dl-cdn.alpinelinux.org/alpine/v3.24/releases/x86_64/$NAME"
    mv "$ARCHIVE.part" "$ARCHIVE"
fi
printf '%s  %s\n' 41f73e3cf5fa919b8aa5ca6b30dc48f0da2720776d7423e2a7748211456fe081 \
    "$ARCHIVE" | sha256sum --check --status
WORK=$(mktemp -d "$(dirname -- "$OUTPUT")/.inspect-build.XXXXXX")
trap 'rm -rf -- "$WORK"' EXIT
ROOT="$WORK/root"
mkdir -p "$ROOT" "$CACHE/inspect-apks"
tar -xzf "$ARCHIVE" -C "$ROOT" --exclude='./dev/*'
mapfile -t PINS < <(sed 's/[[:space:]]*#.*//' "$REPO_ROOT/config/inspect-apks.lock" | sed '/^$/d')
# apk verifies repository and package signatures with the pinned release's keys.
# Explicitly disable all host-side scriptlets, triggers and ownership changes.
env -u LD_PRELOAD -u LD_LIBRARY_PATH "$ROOT/lib/ld-musl-x86_64.so.1" \
    --library-path "$ROOT/lib:$ROOT/usr/lib" "$ROOT/sbin/apk" \
    --root "$ROOT" --keys-dir "$ROOT/etc/apk/keys" \
    --repositories-file "$ROOT/etc/apk/repositories" \
    --cache-dir "$CACHE/inspect-apks" --no-scripts --no-chown add "${PINS[@]}"
python3 - "$ROOT" "$WORK/stage" "$REPO_ROOT/config/inspect-apks.lock" <<'PY'
import os, shutil, subprocess, sys
from pathlib import Path
root, out, lock = map(Path, sys.argv[1:])
expected = dict(line.split('#', 1)[0].strip().split('=', 1)
                for line in lock.read_text().splitlines() if line.split('#', 1)[0].strip())
actual = {}
for block in (root/'lib/apk/db/installed').read_text().split('\n\n'):
    fields = dict(line.split(':', 1) for line in block.splitlines()
                  if ':' in line and len(line.split(':', 1)[0]) == 1)
    if 'P' in fields:
        actual[fields['P']] = fields['V']
if actual != expected:
    raise SystemExit('signed package closure does not match the checked-in version lock')
# APK scriptlets stay disabled. Compile only the signed hardware name data,
# with both input directories and output hwdb.bin rooted in this staging tree.
# This is not udevd/control/trigger, and must never change host devices.
env = dict(os.environ)
for name in ['LD_PRELOAD', 'LD_LIBRARY_PATH']:
    env.pop(name, None)
subprocess.run([str(root/'lib/ld-musl-x86_64.so.1'), '--library-path',
                f'{root}/lib:{root}/usr/lib', str(root/'bin/udevadm'),
                'hwdb', '--update', '--root', str(root)], check=True, env=env)
hwdb = root/'etc/udev/hwdb.bin'
if not hwdb.is_file() or hwdb.stat().st_size < 80:
    raise SystemExit('isolated hardware database compilation produced no usable file')
out.mkdir()
(out/'etc/udev').mkdir(parents=True)
shutil.copy2(hwdb, out/'etc/udev/hwdb.bin')
# Only runtime libraries/data live at distro absolute paths. Never replace the
# project's init, accounts, shell, BusyBox, or baseline program symlinks.
for path in ['lib', 'usr/lib', 'usr/share', 'etc/terminfo']:
    shutil.copytree(root/path, out/path, symlinks=True)
for path in ['lib/apk', 'lib/modules-load.d', 'lib/sysctl.d', 'usr/lib/modules-load.d',
             'usr/lib/sysctl.d', 'usr/lib/apk']:
    shutil.rmtree(out/path, ignore_errors=True)
bin_dir = out/'opt/thekernel-tools/bin'
bin_dir.mkdir(parents=True)
shutil.copy2(root/'bin/busybox', bin_dir/'busybox')
programs = ['ps', 'top', 'free', 'vmstat', 'uptime', 'pmap', 'pidstat', 'htop',
            'lsblk', 'findmnt', 'mount', 'df', 'lscpu', 'lsns', 'lspci', 'lsusb',
            'iostat', 'mpstat', 'ip', 'ss', 'netstat', 'unshare', 'nsenter',
            'sfdisk', 'mke2fs', 'mkfs.ext4', 'e2fsck']
for program in programs:
    source = next((root/part/program for part in ['bin', 'sbin', 'usr/bin', 'usr/sbin']
                   if (root/part/program).exists() or (root/part/program).is_symlink()), None)
    if source is None:
        raise SystemExit(f'missing required tool: {program}')
    for _ in range(20):
        if not source.is_symlink():
            break
        target = os.readlink(source)
        source = root/target.lstrip('/') if target.startswith('/') else source.parent/target
    else:
        raise SystemExit(f'symlink loop: {program}')
    if not source.resolve().is_relative_to(root.resolve()):
        raise SystemExit(f'tool escaped staging: {program}')
    if source.name == 'busybox':
        dest = bin_dir/program
        dest.write_text(f'#!/bin/sh\nexec /opt/thekernel-tools/bin/busybox {program} "$@"\n')
        dest.chmod(0o755)
    else:
        shutil.copy2(source, bin_dir/program)
shutil.copy2(lock, out/'opt/thekernel-tools/MANIFEST')
shutil.copy2(Path(os.environ["REPO_ROOT"])/'tests/guest/inspect-tools.sh', out/'opt/thekernel-tools/inspect-tools.sh')
shutil.copy2(Path(os.environ["REPO_ROOT"])/'tests/guest/block-gpt-tools.sh', out/'opt/thekernel-tools/block-gpt-tools.sh')
shutil.copy2(Path(os.environ["REPO_ROOT"])/'tests/guest/block-partition-mkfs-smoke.sh', out/'opt/thekernel-tools/partition-mkfs-smoke.sh')
shutil.copy2(Path(os.environ["REPO_ROOT"])/'tests/guest/container-namespace.sh', out/'opt/thekernel-tools/container-namespace.sh')
PY
# OUTPUT is a dedicated regenerable tool staging tree, not a source directory.
# Refuse dangerous aliases before replacing it.
case "$OUTPUT" in /|/home|"$HOME"|"$REPO_ROOT"|"$CACHE") echo 'unsafe output directory' >&2; exit 2 ;; esac
rm -rf -- "$OUTPUT"
mv "$WORK/stage" "$OUTPUT"
echo "inspect tools staged at $OUTPUT"
