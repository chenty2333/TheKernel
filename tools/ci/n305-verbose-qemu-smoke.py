#!/usr/bin/env python3
"""Bounded 8-vCPU/8GiB firmware-FB verbose boot with real DHCP/UDP log delivery."""
from pathlib import Path
import dataclasses
import os
import socket
import re
import subprocess
import sys
import tempfile
import threading
ROOT=Path(__file__).resolve().parents[2]
sys.path.insert(0,str(ROOT))
from tools import thekernel as product
from tools.product_state import state_root,selected_tool_payload
from tools.qemu_runner.model import QmpConsoleLine
from tools.qemu_runner.console_font import ConsoleFont
PROBE=b'Initialize alarm'

def main():
    os.environ['THEKERNEL_TOOLCHAIN']=selected_tool_payload()
    build=['build','--platform','n305','--profile','shell','--smp','8','--memory','8G']
    args=product.build_parser().parse_args(build);artifacts=product.artifacts_for(args)
    subprocess.run([sys.executable,str(ROOT/'tools/thekernel.py'),*build],cwd=ROOT,check=True)
    runs=state_root()/'runs';runs.mkdir(parents=True,exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='verbose-',dir=runs) as directory, socket.socket(socket.AF_INET,socket.SOCK_DGRAM) as receiver:
        directory=Path(directory);work=directory/'run';commands=directory/'commands'
        receiver.bind(('127.0.0.1',0));receiver.settimeout(.2);port=receiver.getsockname()[1]
        commands.write_text("cat /proc/cmdline\nsleep 1\ncat /proc/boot-progress\necho A2_USERSPACE_READY\nsleep 6\npoweroff -f\n")
        stopped=threading.Event();seen=threading.Event();packets=[0]
        def receive():
            while not stopped.is_set():
                try:data,_=receiver.recvfrom(4096)
                except socket.timeout:continue
                packets[0]+=1
                if PROBE in data:seen.set()
        listener=threading.Thread(target=receive);listener.start()
        oracle=dataclasses.replace(product.FBCON_TEXT_CELLS,expected_lines=(QmpConsoleLine('verbose userspace marker',ConsoleFont.load().cells('A2_USERSPACE_READY')),))
        try:
            result=product.run_product(artifacts,product.RunSpec(accel='kvm',timeout=150,workdir=work,interactive=False,
                input_after_marker='THEKERNEL_SHELL_READY',stop_after_marker=None,commands=commands,extra_block=None,
                run_cpus=8,graphics_profile='firmware-fb',kernel_cmdline=f'loglevel=7 boot.progress=1 n305.net=dhcp n305.netconsole=10.0.2.2:{port}',
                qmp_screenshot=work/'screen.ppm',qmp_screenshot_after_marker='A2_USERSPACE_READY',qmp_screenshot_text_cells=oracle,qmp_timeout_secs=120))
        finally:stopped.set();listener.join(timeout=1)
        if result:raise RuntimeError('verbose boot did not finish cleanly')
        console=(work/'console.log').read_text().replace('\r','')
        if '\nA2_USERSPACE_READY\n' not in console or 'N305_DHCP_BOUND' not in console or 'N305_NETCONSOLE_READY' not in console or not seen.is_set():
            raise RuntimeError('verbose init/DHCP/real UDP log probe missing')
        if 'BOOT_PROGRESS enabled=true' not in console:
            raise RuntimeError('opt-in progress snapshot missing')
        for cpu in range(8):
            found=re.search(rf'cpu={cpu} phase=7 timer_irqs=(\d+)',console)
            if found is None or int(found.group(1))==0:
                raise RuntimeError(f'no live timer/online progress for CPU {cpu}')
        print(f'N305_VERBOSE_QEMU_PASS cpus=8 memory=8G framebuffer-glyphs=verified udp-alarm-record=received packets={packets[0]}')
        print('No hardware root-cause or fix claim; this is an emulated non-reproduction.')
if __name__=='__main__':main()
