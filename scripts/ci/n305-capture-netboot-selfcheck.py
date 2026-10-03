#!/usr/bin/env python3
"""Boot the actual Alpine UEFI/PXE capture path with QEMU user networking."""
import argparse
import functools
import http.server
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import threading
import time

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from tools.n305_netboot import CaptureHandler, prepare
from tools.product_state import validate_storage


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--alpine-dir', type=Path, required=True)
    parser.add_argument('--apks', type=Path, required=True)
    parser.add_argument('--timeout', type=int, default=240)
    args = parser.parse_args()
    args.out = args.out.resolve(); validate_storage(args.out)
    args.out.mkdir(parents=True, exist_ok=True)
    # Port zero is used only by the host listener. GRUB gets the assigned port.
    with http.server.HTTPServer(('127.0.0.1', 0), http.server.BaseHTTPRequestHandler) as server:
        prep = argparse.Namespace(out=args.out, interface='qemu-test', address='10.0.2.2',
            port=server.server_port, mode='capture', kernel=None, rootfs=None,
            alpine_dir=args.alpine_dir, apks=args.apks, loglevel='info', gfxmode='1920x1080x32,auto')
        prepare(prep)
        config = json.loads((args.out/'session.json').read_text())
        server.RequestHandlerClass = functools.partial(CaptureHandler, root=args.out, token=config['token'])
        thread = threading.Thread(target=server.serve_forever); thread.start()
        qemu = None
        try:
            shutil.copyfile('/usr/share/OVMF/OVMF_VARS.fd', args.out/'OVMF_VARS.fd')
            command = ['qemu-system-x86_64', '-machine','q35,accel=kvm','-cpu','host','-m','2G','-smp','2',
                '-drive','if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE.fd',
                '-drive',f'if=pflash,format=raw,file={args.out}/OVMF_VARS.fd',
                '-device','qemu-xhci','-device','usb-kbd','-device','usb-mouse','-device','usb-tablet',
                '-netdev',f'user,id=pxe,tftp={args.out}/tftp,bootfile=BOOTX64.EFI',
                '-device','e1000,netdev=pxe','-boot','order=n','-vga','none','-device','bochs-display',
                '-display','none','-serial',f'file:{args.out}/serial.log','-monitor','none','-no-reboot']
            with (args.out/'qemu.log').open('w') as output:
                qemu = subprocess.Popen(command, stdout=output, stderr=subprocess.STDOUT)
                end = time.monotonic()+args.timeout
                before = set((args.out/'received').glob('n305-*/capture-status.txt'))
                result = None
                while time.monotonic() < end and qemu.poll() is None:
                    paths = set((args.out/'received').glob('n305-*/capture-status.txt')) - before
                    if paths: result = next(iter(paths)); break
                    time.sleep(.2)
                if result is None: raise RuntimeError(f'PXE capture did not upload; inspect {args.out}/serial.log')
                status = {row.split('\t')[1]: row.split('\t')[0] for row in result.read_text().splitlines() if '\t' in row}
                required = ['meta/network-tool-install','pci/lspci-nnvvv.txt','acpi/acpidump.txt','dmi/dmidecode-full.txt',
                    'cpu/lscpu.txt','usb/lsusb.txt','display/modetest.txt','network/ip-link.txt','network/ethtool-eth0.txt']
                missing = [key for key in required if status.get(key) != 'OK']
                if missing: raise RuntimeError(f'capture uploaded but required probes did not succeed: {missing}')
                edids = list((result.parent/'display/edid').glob('*.edid'))
                if not edids: raise RuntimeError('QEMU bochs EDID was not captured')
                print(f'PASS: UEFI PXE -> Alpine -> required probes + EDID -> HTTP upload: {result.parent}')
        finally:
            if qemu is not None and qemu.poll() is None:
                qemu.terminate()
                try: qemu.wait(timeout=10)
                except subprocess.TimeoutExpired: qemu.kill(); qemu.wait()
            server.shutdown(); thread.join(timeout=5)
            if thread.is_alive(): raise RuntimeError('HTTP listener did not stop')


if __name__ == '__main__':
    main()
