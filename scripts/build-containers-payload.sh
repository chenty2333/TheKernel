#!/usr/bin/env bash
# Signed Alpine tools for container acceptance; not a host installation/container.
set -euo pipefail
export REPO_ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
OUTPUT=
while (($#)); do
    case "$1" in
        --output) OUTPUT=${2:?}; shift 2 ;;
        -h|--help) echo 'build-containers-payload.sh --output DIR'; exit 0 ;;
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
            grep -qx '# Alpine v3.24 x86_64; exact signed package closure for B2 container tools.' || {
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
WORK=$(mktemp -d "$(dirname -- "$OUTPUT")/.containers-build.XXXXXX")
trap 'rm -rf -- "$WORK"' EXIT
ROOT="$WORK/root"
mkdir -p "$ROOT" "$CACHE/containers-apks"
tar -xzf "$ARCHIVE" -C "$ROOT" --exclude='./dev/*'
mapfile -t PINS < <(sed 's/[[:space:]]*#.*//' "$REPO_ROOT/config/containers-apks.lock" | sed '/^$/d')
# Explicit signed cached APKs remain available even after the live repository
# index removes an older pinned version. Keep every world constraint unchanged.
shopt -s nullglob
EXACT_APKS=()
for pin in "${PINS[@]}"; do
    stem=${pin/=/-}
    matches=("$CACHE/containers-apks/$stem".*.apk)
    if [ -f "$CACHE/containers-apks/$stem.apk" ]; then
        matches+=("$CACHE/containers-apks/$stem.apk")
    fi
    if ((${#matches[@]} > 1)); then
        echo "ambiguous cached source for $pin" >&2
        exit 2
    fi
    if ((${#matches[@]} == 1)); then
        EXACT_APKS+=("${matches[0]}")
    fi
done
shopt -u nullglob
# apk verifies repository and package signatures with the pinned release's keys.
# Explicitly disable all host-side scriptlets, triggers and ownership changes.
env -u LD_PRELOAD -u LD_LIBRARY_PATH "$ROOT/lib/ld-musl-x86_64.so.1" \
    --library-path "$ROOT/lib:$ROOT/usr/lib" "$ROOT/sbin/apk" \
    --root "$ROOT" --keys-dir "$ROOT/etc/apk/keys" \
    --repositories-file "$ROOT/etc/apk/repositories" \
    --cache-dir "$CACHE/containers-apks" --no-scripts --no-chown add "${PINS[@]}" "${EXACT_APKS[@]}"
python3 - "$ROOT" "$WORK/stage" "$REPO_ROOT/config/containers-apks.lock" "$ARCHIVE" <<'PY'
import gzip, hashlib, io, json, os, shutil, sys, tarfile
from pathlib import Path
root, out, lock, archive = map(Path, sys.argv[1:])
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
out.mkdir()
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
programs = ['bwrap', 'crun', 'podman', 'conmon', 'fuse-overlayfs', 'fusermount3',
            'newuidmap', 'newgidmap', 'catatonit', 'ps', 'mount', 'unshare', 'nsenter', 'lsns']
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
shutil.copy2(Path(os.environ['REPO_ROOT'])/'tests/guest/container-bwrap.sh', out/'opt/thekernel-containers-bwrap.sh')
for name in ['newuidmap', 'newgidmap', 'fusermount3', 'catatonit']:
    dest = out/'usr/bin'/name
    dest.parent.mkdir(parents=True, exist_ok=True)
    dest.symlink_to(f'/opt/thekernel-tools/bin/{name}')
if (root/'etc/containers').exists():
    shutil.copytree(root/'etc/containers', out/'etc/containers', symlinks=True)
conf = out/'etc/containers/containers.conf'
conf.parent.mkdir(parents=True, exist_ok=True)
conf.write_text('[engine]\ncgroup_manager="cgroupfs"\nevents_logger="file"\nruntime="crun"\n'
                'helper_binaries_dir=["/opt/thekernel-tools/bin", "/usr/libexec/podman"]\n'
                '[engine.runtimes]\ncrun=["/opt/thekernel-tools/bin/crun"]\n')
if (root/'usr/libexec/podman').exists():
    shutil.copytree(root/'usr/libexec/podman', out/'usr/libexec/podman', symlinks=True)
if (root/'etc/fuse.conf').exists():
    shutil.copy2(root/'etc/fuse.conf', out/'etc/fuse.conf')
# Keep a minimal BusyBox OCI root separate from the guest's init/accounts.
oci = out/'opt/thekernel-containers/busybox-root'
(oci/'bin').mkdir(parents=True)
for directory in ['dev', 'proc', 'sys', 'tmp', 'etc', 'root', 'fixture']:
    (oci/directory).mkdir()
shutil.copy2(root/'bin/busybox', oci/'bin/busybox')
shutil.copytree(root/'lib', oci/'lib', symlinks=True)
shutil.rmtree(oci/'lib/apk', ignore_errors=True)
for name in ['sh', 'echo', 'cat', 'id', 'sleep', 'true', 'false', 'ls']:
    (oci/'bin'/name).symlink_to('busybox')
# Offline image bytes are prepared by the existing pinned release archive,
# never pulled from a registry inside the guest.
images = out/'opt/thekernel-containers/images'
images.mkdir(parents=True)
with gzip.open(archive, 'rb') as source:
    layer = source.read()
diff = hashlib.sha256(layer).hexdigest()
config = json.dumps({
    'created': '2026-10-01T00:00:00Z', 'architecture': 'amd64', 'os': 'linux',
    'config': {'Env': ['PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin'], 'Cmd': ['/bin/sh']},
    'rootfs': {'type': 'layers', 'diff_ids': ['sha256:' + diff]},
}, sort_keys=True, separators=(',', ':')).encode()
config_name = hashlib.sha256(config).hexdigest() + '.json'
manifest = json.dumps([{'Config': config_name, 'RepoTags': ['alpine:3.24.1', 'alpine:latest'],
                         'Layers': ['layer.tar']}], sort_keys=True).encode()
with tarfile.open(images/'alpine-3.24.1.tar', 'w', format=tarfile.USTAR_FORMAT) as saved:
    for name, data in [('layer.tar', layer), (config_name, config), ('manifest.json', manifest)]:
        member = tarfile.TarInfo(name)
        member.size, member.mode, member.mtime = len(data), 0o644, 0
        saved.addfile(member, io.BytesIO(data))


PY
# OUTPUT is a dedicated regenerable tool staging tree, not a source directory.
# Refuse dangerous aliases before replacing it.
case "$OUTPUT" in /|/home|"$HOME"|"$REPO_ROOT"|"$CACHE") echo 'unsafe output directory' >&2; exit 2 ;; esac
rm -rf -- "$OUTPUT"
mv "$WORK/stage" "$OUTPUT"
echo "container tools staged at $OUTPUT"
