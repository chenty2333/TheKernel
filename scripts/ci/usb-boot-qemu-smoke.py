#!/usr/bin/env python3
"""Only USB media: boot, write/sync, real reboot, reread identical contents."""
from pathlib import Path
import os
import re
import subprocess
import sys
import tempfile
ROOT=Path(__file__).resolve().parents[2]
sys.path.insert(0,str(ROOT))
from tools import thekernel as product
from tools.product_state import state_root

def validate(console: str, kernel: str):
    console=console.replace('\r','')
    uptime=re.search(r'(?m)^USB_FRESH_UPTIME=(\d+\.\d+) ',console)
    if uptime is None or float(uptime.group(1))>=10:
        raise RuntimeError('USB test did not reboot to fresh userspace')
    if not re.search(r'(?m)^USB_ROOT_MEDIA_CONTENT$',console) or not re.search(r'(?m)^USB_BOOT_RW_OK$',console):
        raise RuntimeError('fresh USB boot did not reread the written media contents')
    observed=kernel + console
    if 'USB rootfs: GPT start=' not in observed or '"USB rootfs"' not in observed:
        raise RuntimeError('filesystem was not mounted through the USB partition view')
    if 'registered immutable Multiboot rootfs module' in kernel:
        raise RuntimeError('USB boot was accidentally satisfied by a RAM module')
    if len(re.findall(r'BdsDxe: starting .*USB',console))!=2:
        raise RuntimeError('both firmware boots must load from USB, not SATA')

def main():
    env=os.environ.copy();runs=state_root()/'runs';runs.mkdir(parents=True,exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='usb-boot-',dir=runs) as directory:
        directory=Path(directory);image=directory/'usb.img';work=directory/'run'
        parser=product.build_parser();args=parser.parse_args(['build','--platform','n305','--profile','shell'])
        artifacts=product.artifacts_for(args)
        subprocess.run([sys.executable,str(ROOT/'tools/thekernel.py'),'build','--platform','n305','--profile','shell'],cwd=ROOT,env=env,check=True)
        subprocess.run([sys.executable,str(ROOT/'scripts/build-usb-boot.py'),'--kernel',str(artifacts.kernel),
                        '--rootfs',str(artifacts.rootfs),'--output',str(image)],cwd=ROOT,env=env,check=True)
        commands=directory/'commands'
        commands.write_text("printf 'USB_ROOT_MEDIA_CONTENT\\n' > /root/usb-boot-content\nsync\nreboot -f\n"
                            "printf 'USB_FRESH_UPTIME='; cat /proc/uptime\ncat /root/usb-boot-content\ndmesg | grep -E 'USB rootfs|use block device'\ncat /proc/cmdline\necho USB_BOOT_RW_OK\npoweroff -f\n")
        subprocess.run([sys.executable,str(ROOT/'tools/thekernel.py'),'run','--platform','n305','--profile','shell',
                        '--accel','kvm','--no-build','--usb-disk',str(image),'--usb-boot','--allow-reboot',
                        '--commands',str(commands),'--workdir',str(work),'--timeout','150'],cwd=ROOT,env=env,check=True)
        validate((work/'console.log').read_text(),(work/'kernel.log').read_text());print('USB_BOOT_QEMU_PASS')
if __name__=='__main__':main()
