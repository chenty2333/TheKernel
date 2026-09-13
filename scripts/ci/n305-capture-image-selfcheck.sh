#!/usr/bin/env bash
# Boot the capture image in QEMU and check that the unattended capture works.
#
# This is the only end-to-end evidence available without the target machine.
# It proves the pipeline: the vendor ISO still boots, the apkovl is found, the
# offline tool set installs, every probe runs, the bundle lands on the payload
# partition in a filesystem the development host can read, and the guest
# powers itself off.  It proves nothing about the N305's hardware: QEMU is not
# that machine, and the facts in the bundle it produces are QEMU's.
#
# Requires: qemu-system-x86_64, OVMF, mtools, and an image built by
# scripts/ci/n305-capture-image.sh.  Boot QEMU through the repository's
# heavy-run wrapper on a shared host.

set -euo pipefail

CACHE="${HOME}/.cache/thekernel-n305-capture"
IMAGE=""
KEEP=no
BOOT_TIMEOUT=1800
FRAME_INTERVAL=3

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)

die() {
	printf 'n305-selfcheck: %s\n' "$*" >&2
	exit 1
}

note() {
	printf 'n305-selfcheck: %s\n' "$*"
}

while [ $# -gt 0 ]; do
	case "$1" in
	--image)
		IMAGE="$2"
		shift 2
		;;
	--cache)
		CACHE="$2"
		shift 2
		;;
	--keep)
		KEEP=yes
		shift
		;;
	--boot-timeout)
		BOOT_TIMEOUT="$2"
		shift 2
		;;
	-h | --help)
		sed -n '2,20p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
		exit 0
		;;
	*)
		die "unknown argument: $1"
		;;
	esac
done

for tool in qemu-system-x86_64 mcopy mdir python3 mktemp; do
	command -v "$tool" >/dev/null 2>&1 || die "missing $tool"
done

CODE=""
VARS=""
for candidate in /usr/share/edk2/ovmf /usr/share/OVMF /usr/share/qemu; do
	if [ -f "$candidate/OVMF_CODE.fd" ] && [ -f "$candidate/OVMF_VARS.fd" ]; then
		CODE="$candidate/OVMF_CODE.fd"
		VARS="$candidate/OVMF_VARS.fd"
		break
	fi
	if [ -f "$candidate/OVMF_CODE_4M.fd" ] && [ -f "$candidate/OVMF_VARS_4M.fd" ]; then
		CODE="$candidate/OVMF_CODE_4M.fd"
		VARS="$candidate/OVMF_VARS_4M.fd"
		break
	fi
done
[ -n "$CODE" ] || die "no OVMF firmware found (install edk2-ovmf)"

if [ -z "$IMAGE" ]; then
	IMAGE="$CACHE/n305-capture.img"
	if [ ! -s "$IMAGE" ]; then
		note "no image at $IMAGE; building one"
		"$SCRIPT_DIR/n305-capture-image.sh" --out "$IMAGE" --cache "$CACHE"
	fi
fi
[ -s "$IMAGE" ] || die "image not found: $IMAGE"

# The payload partition is the last one; find it from the partition table
# rather than assuming an offset.
read -r offset sectors < <(python3 - "$IMAGE" <<'PY'
import struct, sys

with open(sys.argv[1], "rb") as disk:
    disk.seek(446)
    table = disk.read(64)
best = None
for index in range(4):
    entry = table[index * 16:(index + 1) * 16]
    if entry[4] == 0:
        continue
    start, count = struct.unpack_from("<II", entry, 8)
    if start and (best is None or start > best[0]):
        best = (start, count)
print(best[0], best[1])
PY
)
[ -n "${offset:-}" ] || die "cannot find the payload partition in the image"
note "payload partition at sector $offset ($((sectors / 2048)) MiB)"

RUN=$(mktemp -d "$CACHE/qemu-selfcheck-XXXXXX")
cp "$VARS" "$RUN/vars.fd"
note "booting with $CODE (run directory $RUN)"

ACCEL=kvm
[ -w /dev/kvm ] || ACCEL=tcg
note "accelerator: $ACCEL"

qemu-system-x86_64 \
	-machine q35 -accel "$ACCEL" -cpu max -smp 4 -m 2048 \
	-drive "if=pflash,format=raw,readonly=on,file=$CODE" \
	-drive "if=pflash,format=raw,file=$RUN/vars.fd" \
	-drive "if=none,id=d0,format=raw,file=$IMAGE" \
	-device ide-hd,drive=d0,bus=ide.0 \
	-display none -vga std \
	-monitor "unix:$RUN/monitor.sock,server,nowait" \
	-serial "file:$RUN/serial.log" \
	-no-reboot > "$RUN/qemu.log" 2>&1 &
QPID=$!

python3 - "$RUN" "$QPID" "$FRAME_INTERVAL" <<'PY' &
import os, socket, sys, time

run, pid, interval = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
sock = socket.socket(socket.AF_UNIX)
for _ in range(120):
    try:
        sock.connect(os.path.join(run, "monitor.sock"))
        break
    except OSError:
        time.sleep(0.5)
else:
    sys.exit(0)


def screendump(index):
    sock.sendall(("screendump %s/frame-%04d.ppm\n" % (run, index)).encode())
    time.sleep(0.2)
    try:
        sock.settimeout(0.2)
        sock.recv(65536)
    except OSError:
        pass


for index in range(int(1800 / interval)):
    if not os.path.exists("/proc/%d" % pid):
        break
    screendump(index)
    time.sleep(interval)
PY
MONITOR=$!

deadline=$((SECONDS + BOOT_TIMEOUT))
while kill -0 "$QPID" 2>/dev/null && [ "$SECONDS" -lt "$deadline" ]; do
	sleep 2
done
status=0
if kill -0 "$QPID" 2>/dev/null; then
	note "FAIL: the guest did not power itself off within ${BOOT_TIMEOUT}s"
	kill "$QPID" 2>/dev/null || true
	sleep 2
	kill -9 "$QPID" 2>/dev/null || true
	status=1
else
	wait "$QPID" || true
	note "the guest powered itself off (this is the capture's success signal)"
fi
kill "$MONITOR" 2>/dev/null || true

# Look at the screen the machine was showing, exactly as the N305 procedure
# will: the last frame before the machine went off.
last_frame=$(find "$RUN" -maxdepth 1 -name 'frame-*.ppm' | sort | tail -1)
if [ -n "$last_frame" ]; then
	note "last screen frame: $last_frame"
fi

# Read the bundle back out of the image the way a person would copy it off a
# stick, and insist that the probes that matter actually produced something.
note "reading the bundle back out of the image"
rm -rf "$RUN/bundle"
mkdir -p "$RUN/bundle"
latest=$(mdir -i "$IMAGE@@$((offset * 512))" ::/dump 2>/dev/null |
	awk '/^N305-/ { print $NF }' | tail -1)
[ -n "$latest" ] || die "the image contains no dump/n305-* directory: the capture did not write a bundle"
mcopy -i "$IMAGE@@$((offset * 512))" -s "::/dump/$latest" "$RUN/bundle/" || die "cannot copy the bundle out"

BUNDLE="$RUN/bundle/$latest"
note "bundle: $BUNDLE"

missing=0
required="README.txt SUMMARY.txt MANIFEST.sha256 capture-status.txt
acpi/tables/MCFG proc/iomem.txt proc/ioports.txt logs/dmesg.txt
pci/lspci-nnvvv.txt pci/lspci-config-space.txt meta/tool-install.txt"
for file in $required; do
	if [ ! -s "$BUNDLE/$file" ]; then
		note "FAIL: bundle is missing or has an empty $file"
		missing=1
	fi
done
[ -s "$BUNDLE/acpi/mcfg-decoded.txt" ] || note "note: no acpi/mcfg-decoded.txt (no MCFG table?)"

# lspci, acpidump and lscpu only exist if the offline package set installed.
for tool in lspci acpidump lscpu; do
	if ! grep -q "^present: $tool$" "$BUNDLE/meta/tool-availability.txt" 2>/dev/null; then
		note "FAIL: $tool was not installed from the offline tool set"
		missing=1
	fi
done
if grep -q '^FAIL' "$BUNDLE/capture-status.txt" 2>/dev/null; then
	note "probes that failed (not fatal here, but they must be explained):"
	awk -F'\t' '$1 == "FAIL" { printf "    %s %s\n", $2, $3 }' "$BUNDLE/capture-status.txt" | head -20
fi

note "probe status:"
awk -F'\t' '{ count[$1]++ } END { for (key in count) printf "    %-12s %d\n", key, count[key] }' \
	"$BUNDLE/capture-status.txt" | sort

if [ -s "$BUNDLE/acpi/mcfg-decoded.txt" ]; then
	note "MCFG as decoded on the DUT:"
	sed 's/^/    /' "$BUNDLE/acpi/mcfg-decoded.txt" | head -12
fi

if [ "$KEEP" = no ]; then
	rm -rf "$RUN"
else
	note "kept $RUN"
fi

if [ "$status" != 0 ] || [ "$missing" != 0 ]; then
	note "FAIL: the capture pipeline did not complete"
	exit 1
fi
note "PASS: the capture pipeline works end to end (on QEMU, not on the N305)"
