#!/usr/bin/env python3
"""Prepare a private, foreground-only N305 PXE session. Preparation needs no root."""
from __future__ import annotations

import argparse
import functools
import http.server
import ipaddress
import json
import os
from pathlib import Path, PurePosixPath
import socket
import threading
import re
import secrets
import shlex
import shutil
import subprocess
import tarfile
import tempfile

REPO = Path(__file__).resolve().parents[1]
MAX_UPLOAD = 96 * 1024 * 1024


def write(path: Path, content: str, executable: bool = False) -> None:
    path.write_text(content)
    path.chmod(0o755 if executable else 0o644)


def grub_config(mode: str, address: str, port: int, loglevel: str, gfxmode: str) -> str:
    header = f'''insmod efinet
insmod net
insmod tftp
insmod all_video
terminal_output console
set gfxpayload={gfxmode}
echo "N305 PXE: requesting DHCP"
net_bootp
set root=(tftp)
'''
    if mode == "kernel":
        return header + f'''echo "N305 PXE: loading TheKernel"
multiboot2 /TheKernel.elf quiet loglevel={loglevel} n305.net=dhcp n305.netconsole={address}:6666
echo "N305 PXE: loading rootfs; this can take a minute"
module2 /rootfs-x86.img rootfs
echo "N305 PXE: starting kernel (hardware drivers unverified)"
boot
'''
    base = f"http://{address}:{port}"
    return header + f'''echo "N305 PXE: loading Alpine capture kernel"
linux /vmlinuz-lts ip=dhcp modules=loop,squashfs,sd-mod,usb-storage alpine_repo={base}/apks/main modloop={base}/modloop-lts apkovl={base}/capture.apkovl.tar.gz console=tty0 console=ttyS0,115200 drm.debug=0x1e loglevel=4
initrd /initramfs-lts
echo "N305 PXE: starting unattended capture"
boot
'''


def prepare(args: argparse.Namespace) -> None:
    # Values enter GRUB and dnsmasq syntax, not just shell arguments.
    if not re.fullmatch(r"[A-Za-z0-9_.:-]+", args.interface):
        raise ValueError("invalid interface name")
    address = str(ipaddress.IPv4Address(args.address))
    if not 1024 <= args.port <= 65535:
        raise ValueError("HTTP port must be unprivileged")
    if not re.fullmatch(r"[1-9][0-9]{2,3}x[1-9][0-9]{2,3}(x32)?(,auto)?|auto", args.gfxmode):
        raise ValueError("use a mode such as 1920x1080x32,auto (never keep)")
    out = args.out.expanduser().resolve()
    if str(out).startswith(("/tmp/", "/dev/shm/")):
        raise ValueError("netboot artifacts must not live on tmpfs")
    out.mkdir(parents=True, exist_ok=True)
    if (out / "session.pid").exists():
        raise ValueError("stop the previous session before preparing again")
    tftp, http = out / "tftp", out / "http"
    tftp.mkdir(exist_ok=True); http.mkdir(exist_ok=True)
    token = secrets.token_hex(24)
    config = {"address": address, "port": args.port, "interface": args.interface,
              "token": token, "mode": args.mode}
    write(out / "session.json", json.dumps(config))
    if args.mode == "kernel":
        if not args.kernel or not args.rootfs:
            raise ValueError("kernel mode requires --kernel ELF and --rootfs IMAGE")
        for source, name in ((args.kernel, "TheKernel.elf"), (args.rootfs, "rootfs-x86.img")):
            shutil.copyfile(source, tftp / name)
            (tftp / name).chmod(0o644)
    else:
        if not args.alpine_dir or not args.apks:
            raise ValueError("capture mode requires --alpine-dir and --apks (see --help)")
        for name, destination in (("vmlinuz-lts", tftp), ("initramfs-lts", tftp), ("modloop-lts", http)):
            shutil.copyfile(args.alpine_dir / name, destination / name)
            (destination / name).chmod(0o644)
        for repo in ("main", "community"):
            source = args.apks / repo
            if (source / "x86_64").is_dir(): source = source / "x86_64"
            shutil.copytree(source, http / "apks" / repo / "x86_64", dirs_exist_ok=True)
        overlay = out / "overlay"
        (overlay / "etc/local.d").mkdir(parents=True, exist_ok=True)
        (overlay / "etc/runlevels/default").mkdir(parents=True, exist_ok=True)
        write(overlay / "etc/.default_boot_services", "")
        (overlay / "etc/apk").mkdir(exist_ok=True)
        write(overlay / "etc/apk/world", "alpine-base\nopenssl\nlinux-firmware-i915\nlinux-firmware-realtek\n")
        write(overlay / "etc/apk/repositories", f"http://{address}:{args.port}/apks/main\nhttp://{address}:{args.port}/apks/community\n")
        shutil.copyfile(REPO / "scripts/ci/n305-capture-payload.sh", overlay / "etc/n305-capture.sh")
        upload = f"http://{address}:{args.port}/upload/{token}"
        write(overlay / "etc/local.d/n305-capture.start", f'''#!/bin/sh
export N305_CAPTURE_ROOT=/var/lib/n305-capture
export N305_PROBE_TIMEOUT=20
export N305_UPLOAD_URL={shlex.quote(upload)}
mkdir -p "$N305_CAPTURE_ROOT/apks"
mount -t debugfs none /sys/kernel/debug 2>/dev/null || true
modprobe i915; modprobe r8169; modprobe igc
# The base APK repository is local too: no internet access on the DUT needed.
if apk add pciutils acpica dmidecode usbutils util-linux-misc lscpu lsblk openssl libdrm-tests drm_info ethtool curl iproute2 > /var/log/n305-tool-install.log 2>&1; then
    export N305_TOOL_INSTALL_STATUS=OK
else
    export N305_TOOL_INSTALL_STATUS=FAIL
fi
sh /etc/n305-capture.sh > /dev/console 2>&1
''', True)
        link = overlay / "etc/runlevels/default/local"
        link.unlink(missing_ok=True); link.symlink_to("/etc/init.d/local")
        with tarfile.open(http / "capture.apkovl.tar.gz", "w:gz") as archive:
            archive.add(overlay / "etc", arcname="etc")
        (http / "capture.apkovl.tar.gz").chmod(0o644)
    cfg = grub_config(args.mode, address, args.port, args.loglevel, args.gfxmode)
    if getattr(args, "kernel_cmdline", None) is not None:
        if args.mode != "kernel":
            raise RuntimeError("--kernel-cmdline applies only to the TheKernel kernel mode")
        import sys
        if str(REPO) not in sys.path:
            sys.path.insert(0, str(REPO))
        from tools.kernel_cmdline import append_kernel_cmdline
        cfg = append_kernel_cmdline(cfg, args.kernel_cmdline)

    write(out / "grub.cfg", cfg)
    maker = shutil.which("grub2-mkstandalone") or shutil.which("grub-mkstandalone")
    if not maker:
        raise ValueError("grub2-mkstandalone is required")
    subprocess.run([maker, "-O", "x86_64-efi", "--modules=efinet net tftp multiboot2 linux all_video", "-o", str(tftp / "BOOTX64.EFI"),
                    f"boot/grub/grub.cfg={out / 'grub.cfg'}"], check=True)
    (tftp / "BOOTX64.EFI").chmod(0o644)
    network = ipaddress.IPv4Network(address + "/24", strict=False)
    # Avoid allocating the server address.
    start, end = network.network_address + 10, network.network_address + 30
    if start <= ipaddress.IPv4Address(address) <= end:
        raise ValueError("server address overlaps DHCP pool (.10 through .30)")
    # bind-dynamic, not bind-interfaces: powering the N305 on bounces the
    # link, NetworkManager re-applies the address, and a bind-interfaces
    # dnsmasq then answers every DISCOVER with "has no address".
    write(out / "dnsmasq.conf", f'''interface={args.interface}
bind-dynamic
except-interface=lo
port=0
no-resolv
no-hosts
user=root
dhcp-authoritative
dhcp-range={start},{end},255.255.255.0,1h
dhcp-option=3
dhcp-option=6
dhcp-boot=BOOTX64.EFI
enable-tftp
tftp-root={tftp}
log-dhcp
pid-file={out / 'dnsmasq.pid'}
''')
    generate_session_scripts(out, args.interface, address)
    write(out / "preview.sh", f"#!/bin/bash\nexec {shlex.quote(str(REPO / 'scripts/ci/n305-screen-capture.sh'))} preview --snapshot {shlex.quote(str(out / 'screen.jpg'))} \"$@\"\n", True)
    print(f"Prepared {args.mode} PXE at {out}; no host network configuration was changed.")
    print(f"Tonight: sudo {out}/start.sh ; preview in another terminal: {out}/preview.sh")


def generate_session_scripts(out: Path, interface: str, address: str) -> None:
    qout, qiface, qaddress = map(shlex.quote, (str(out), interface, address))
    # No pre-existing profile is modified. A new profile owns the trusted zone;
    # reactivating the old UUID restores its properties. It must autoconnect at
    # the highest priority: the DUT's OS resets its NIC, the link bounces, and
    # NetworkManager otherwise re-activates the old DHCP profile, leaving the
    # interface without the server address. Cleanup deletes it on every exit.
    write(out / "start.sh", f'''#!/bin/bash
set -euo pipefail
[[ $(id -u) == 0 ]] || {{ echo "Run with sudo" >&2; exit 1; }}
ROOT={qout}; IFACE={qiface}; ADDRESS={qaddress}
[[ ! -e "$ROOT/session.pid" ]] || {{ echo "Session already active; stop it first" >&2; exit 1; }}
[[ $(nmcli -g GENERAL.TYPE device show "$IFACE") == ethernet ]] || {{ echo "Choose a dedicated Ethernet interface" >&2; exit 1; }}
OLD=$(nmcli -g GENERAL.CON-UUID device show "$IFACE")
printf '%s\\n' "$OLD" > "$ROOT/previous-connection"
PROFILE="TheKernel N305 PXE $$"
printf '%s\\n' "$$" > "$ROOT/session.pid"
DNS=''; HTTP=''; NEW=''; CREATED=0; OLD_ZONE=''
if command -v firewall-cmd >/dev/null && firewall-cmd --state >/dev/null 2>&1; then
    OLD_ZONE=$(firewall-cmd --get-zone-of-interface="$IFACE" 2>/dev/null || true)
fi
cleanup() {{
    trap - EXIT INT TERM HUP
    [[ -z "$DNS" ]] || {{ kill "$DNS" 2>/dev/null || true; wait "$DNS" 2>/dev/null || true; }}
    [[ -z "$HTTP" ]] || {{ kill "$HTTP" 2>/dev/null || true; wait "$HTTP" 2>/dev/null || true; }}
    if [[ "$CREATED" == 1 ]]; then
        nmcli connection down "$PROFILE" >/dev/null 2>&1 || true
        nmcli connection delete "$PROFILE" >/dev/null 2>&1 || true
    fi
    if [[ -n "$OLD" && "$OLD" != -- ]]; then
        nmcli connection up uuid "$OLD" >/dev/null || echo "RESTORE FAILED: nmcli connection up uuid $OLD" >&2
    else
        nmcli device disconnect "$IFACE" >/dev/null 2>&1 || true
    fi
    # NM normally restores the profile's zone. Preserve a pre-existing
    # runtime interface-zone override too, without changing permanent rules.
    if [[ -n "$OLD_ZONE" && "$OLD_ZONE" != "no zone" ]]; then
        CURRENT_ZONE=$(firewall-cmd --get-zone-of-interface="$IFACE" 2>/dev/null || true)
        if [[ "$CURRENT_ZONE" != "$OLD_ZONE" ]]; then
            firewall-cmd --zone="$OLD_ZONE" --change-interface="$IFACE" >/dev/null || echo "RESTORE FAILED: firewall zone $OLD_ZONE on $IFACE" >&2
        fi
    fi
    rm -f "$ROOT/session.pid" "$ROOT/dnsmasq.pid"
}}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP
nmcli connection add type ethernet ifname "$IFACE" con-name "$PROFILE" connection.autoconnect yes connection.autoconnect-priority 999 ipv4.method manual ipv4.addresses "$ADDRESS/24" ipv4.never-default yes ipv6.method disabled connection.zone trusted >/dev/null
CREATED=1
NEW=$(nmcli -g connection.uuid connection show "$PROFILE")
nmcli connection up uuid "$NEW" >/dev/null
if command -v firewall-cmd >/dev/null && firewall-cmd --state >/dev/null 2>&1; then
    [[ $(firewall-cmd --get-zone-of-interface="$IFACE") == trusted ]] || {{ echo "Interface is not in trusted zone" >&2; exit 1; }}
fi
# Both children belong to this foreground session and are reaped on exit.
python3 {shlex.quote(str(REPO / 'tools/n305_netboot.py'))} serve --out "$ROOT" --bind "$ADDRESS" &
HTTP=$!
dnsmasq --no-daemon --log-facility=- --conf-file="$ROOT/dnsmasq.conf" &
DNS=$!
echo "N305 PXE active on $IFACE. Ctrl-C restores the previous connection."
wait -n "$DNS" "$HTTP"
''', True)
    write(out / "stop.sh", f'''#!/bin/bash
set -euo pipefail
[[ $(id -u) == 0 ]] || {{ echo "Run with sudo" >&2; exit 1; }}
ROOT={qout}
[[ -f "$ROOT/session.pid" ]] || exit 0
PID=$(cat "$ROOT/session.pid")
[[ "$PID" =~ ^[0-9]+$ && -r /proc/$PID/cmdline ]] || {{ echo "Stale session PID; inspect manually" >&2; exit 1; }}
grep -zF "$ROOT/start.sh" /proc/$PID/cmdline >/dev/null || {{ echo "PID no longer belongs to this session" >&2; exit 1; }}
kill -TERM "$PID"
for _ in {{1..100}}; do [[ -e "$ROOT/session.pid" ]] || exit 0; sleep .1; done
echo "Cleanup still in progress; inspect the start terminal" >&2
exit 1
''', True)


def fetch_alpine(root: Path) -> tuple[Path, Path]:
    """Stage the release kernel and dependency closure; the DUT needs no WAN."""
    from concurrent.futures import ThreadPoolExecutor
    base = "https://dl-cdn.alpinelinux.org/alpine/v3.24"
    root.mkdir(parents=True, exist_ok=True)
    def fetch(url: str, path: Path) -> None:
        path.parent.mkdir(parents=True, exist_ok=True)
        if path.is_file() and path.stat().st_size: return
        temporary = path.with_suffix(path.suffix + ".part")
        subprocess.run(["curl", "--fail", "--location", "--retry", "2", "--connect-timeout", "15", "--max-time", "600", "--output", str(temporary), url], check=True)
        temporary.replace(path)
    with ThreadPoolExecutor(max_workers=3) as pool:
        list(pool.map(lambda name: fetch(f"{base}/releases/x86_64/netboot-3.24.1/{name}", root / name),
                      ("vmlinuz-lts", "initramfs-lts", "modloop-lts")))
    packages = root / "apks"
    entries = {}
    for repo in ("main", "community"):
        path = packages / repo / "x86_64" / "APKINDEX.tar.gz"
        fetch(f"{base}/{repo}/x86_64/APKINDEX.tar.gz", path)
        with tarfile.open(path) as archive:
            content = archive.extractfile("APKINDEX").read().decode()
        for block in content.strip().split("\n\n"):
            entry = dict(line.split(":", 1) for line in block.splitlines() if len(line) > 2 and line[1] == ":")
            if "P" in entry:
                entry["repo"] = repo; entries.setdefault(entry["P"], entry)
    providers = {}
    for name, entry in entries.items():
        for token in entry.get("p", "").split():
            providers.setdefault(token.split("=")[0], []).append(name)
    # APK also selects install_if packages and repairs packages already in
    # the release initramfs. Stage these known base/network companions too;
    # ordinary dependency closure alone misses them in an offline boot.
    queue = "busybox-binsh ssl_client ifupdown-ng-ethtool ifupdown-ng-iproute2 alpine-base pciutils acpica dmidecode usbutils util-linux-misc lscpu lsblk openssl libdrm-tests drm_info ethtool curl iproute2 linux-firmware-i915 linux-firmware-realtek".split()
    selected = set()
    while queue:
        token = queue.pop()
        if token.startswith("!"): continue
        name = re.split(r"[<>=~]", token, maxsplit=1)[0]
        if name not in entries:
            candidates = providers.get(name, [])
            if not candidates: raise ValueError(f"unresolved APK dependency: {token}")
            name = min(candidates, key=lambda n: (len(n), n))
        if name in selected: continue
        selected.add(name); queue.extend(entries[name].get("D", "").split())
    def download(name):
        entry = entries[name]; repo = entry["repo"]
        filename = f"{name}-{entry['V']}.apk"
        fetch(f"{base}/{repo}/x86_64/{filename}", packages / repo / "x86_64" / filename)
    with ThreadPoolExecutor(max_workers=3) as pool:
        list(pool.map(download, sorted(selected)))
    return root, packages


class CaptureHandler(http.server.SimpleHTTPRequestHandler):
    """Serve only staged files; accept a bounded, token-scoped capture upload."""
    def __init__(self, *args, root: Path, token: str, **kwargs):
        self.root, self.token = root, token
        super().__init__(*args, directory=str(root / "http"), **kwargs)

    def do_POST(self):
        if self.path != "/upload/" + self.token:
            self.send_error(403); return
        try:
            size = int(self.headers.get("Content-Length", "0"))
            if not 0 < size <= MAX_UPLOAD:
                raise ValueError("missing or excessive Content-Length")
            self.connection.settimeout(60)
            received = self.root / "received"
            received.mkdir(exist_ok=True)
            with tempfile.TemporaryDirectory(prefix="upload-", dir=received) as staging:
                archive_path = Path(staging) / "capture.tar.gz"
                with archive_path.open("wb") as destination:
                    remaining = size
                    while remaining:
                        chunk = self.rfile.read(min(65536, remaining))
                        if not chunk: raise ValueError("short upload")
                        destination.write(chunk); remaining -= len(chunk)
                with tarfile.open(archive_path, "r:gz") as archive:
                    members = archive.getmembers()
                    if not members or sum(m.size for m in members) > MAX_UPLOAD or len(members) > 20000:
                        raise ValueError("empty or oversized archive")
                    names = set()
                    for member in members:
                        path = PurePosixPath(member.name)
                        if path.is_absolute() or ".." in path.parts or not path.parts:
                            raise ValueError("unsafe archive path")
                        if not (member.isfile() or member.isdir()):
                            raise ValueError("links and special files are refused")
                        names.add(path.parts[0])
                    if len(names) != 1 or not re.fullmatch(r"n305-[0-9]{8}T[0-9]{6}Z", next(iter(names))):
                        raise ValueError("invalid capture directory")
                    name = next(iter(names))
                    destination = received / name
                    if destination.exists(): raise ValueError("capture already received")
                    unpack = Path(staging) / "unpack"; unpack.mkdir()
                    archive.extractall(unpack, filter="data")
                    if not (unpack / name / "capture-status.txt").is_file():
                        raise ValueError("capture status missing")
                    (unpack / name).rename(destination)
            self.send_response(201); self.end_headers(); self.wfile.write(b"CAPTURE RECEIVED\n")
            print(f"n305-netboot: CAPTURE RECEIVED: {destination}", flush=True)
        except (ValueError, OSError, tarfile.TarError) as error:
            self.send_error(400, str(error))


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    prep = sub.add_parser("prepare")
    prep.add_argument("--out", type=Path, required=True)
    prep.add_argument("--interface", required=True, help="dedicated wired interface for tonight")
    prep.add_argument("--address", default="192.168.10.1")
    prep.add_argument("--port", type=int, default=8080)
    prep.add_argument("--fetch", action="store_true", help="download Alpine 3.24.1 netboot and a signed offline APK repository")
    prep.add_argument("--mode", choices=("kernel", "capture"), default="kernel")
    prep.add_argument("--kernel", type=Path); prep.add_argument("--rootfs", type=Path)
    prep.add_argument("--alpine-dir", type=Path, help="Alpine netboot directory with vmlinuz-lts/initramfs-lts/modloop-lts")
    prep.add_argument("--apks", type=Path, help="local signed APK repository, with APKINDEX.tar.gz")
    prep.add_argument("--loglevel", choices=("error", "warn", "info", "debug", "trace"), default="info")
    prep.add_argument("--kernel-cmdline", help="kernel mode: append literal diagnostic arguments")
    prep.add_argument("--gfxmode", default="1920x1080x32,auto")
    serve = sub.add_parser("serve"); serve.add_argument("--out", type=Path, required=True)
    serve.add_argument("--bind", required=True)
    args = parser.parse_args()
    if args.command == "prepare":
        if args.fetch:
            args.alpine_dir, args.apks = fetch_alpine(args.out.expanduser().resolve() / "alpine")
        prepare(args)
    else:
        config = json.loads((args.out / "session.json").read_text())
        handler = functools.partial(CaptureHandler, root=args.out, token=config["token"])
        with http.server.HTTPServer((args.bind, config["port"]), handler) as server:
            print(f"N305 HTTP foreground receiver on {args.bind}:{config['port']}", flush=True)
            stop = threading.Event()
            with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as udp:
                udp.bind((args.bind, 6666)); udp.settimeout(.5)
                def receive_logs():
                    with (args.out / "kernel-udp.log").open("ab", buffering=0) as log:
                        while not stop.is_set():
                            try: data, sender = udp.recvfrom(65535)
                            except socket.timeout: continue
                            log.write(data)
                            print(data.decode("utf-8", errors="replace"), end="", flush=True)
                thread = threading.Thread(target=receive_logs); thread.start()
                try: server.serve_forever()
                finally: stop.set(); thread.join(timeout=2)


if __name__ == "__main__":
    main()
