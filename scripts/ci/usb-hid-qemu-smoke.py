#!/usr/bin/env python3
"""Actual USB keyboard/report arrays, mouse and tablet evdev input via QMP."""
from pathlib import Path
import sys
import os
import subprocess
import tempfile
ROOT=Path(__file__).resolve().parents[2]
sys.path.insert(0,str(ROOT))
from tools import thekernel as product
from tools.qemu_runner.model import QmpCheckpoint
from tools.product_state import state_root, selected_tool_payload

def main():
    # Match the public CLI normalization before artifact fingerprint validation.
    os.environ["THEKERNEL_TOOLCHAIN"]=selected_tool_payload()
    args=product.build_parser().parse_args(['build','--platform','n305','--profile','shell'])
    artifacts=product.artifacts_for(args)
    subprocess.run([sys.executable,str(ROOT/'tools/thekernel.py'),'build','--platform','n305','--profile','shell'],cwd=ROOT,check=True)
    runs=state_root()/'runs';runs.mkdir(parents=True,exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='usb-hid-',dir=runs) as directory:
        directory=Path(directory);commands=directory/'commands'
        commands.write_text('/opt/thekernel-tests/bin/thekernel-usb-input-smoke\n\x15poweroff -f\n')
        def key(name,down):return {'type':'key','data':{'down':down,'key':{'type':'qcode','data':name}}}
        batches=(
            (key('a',True),), (key('a',False),),
            ({'type':'rel','data':{'axis':'x','value':17}},{'type':'rel','data':{'axis':'y','value':-9}}),
            ({'type':'abs','data':{'axis':'x','value':16384}},{'type':'abs','data':{'axis':'y','value':24576}}),
        )
        spec=product.RunSpec(accel='kvm',timeout=120,workdir=directory/'run',interactive=False,
            input_after_marker='THEKERNEL_SHELL_READY',stop_after_marker=None,commands=commands,
            extra_block=None,run_cpus=4,input_backend='usb',
            qmp_checkpoints=(QmpCheckpoint(input_after_marker='N305_USB_INPUT_WAIT',input_events=batches),),qmp_timeout_secs=100)
        if product.run_product(artifacts,spec):raise RuntimeError('USB HID guest did not shut down cleanly')
        console=(directory/'run/console.log').read_text().replace('\r','')
        if '\nN305_USB_INPUT_PASS keyboard relative absolute\n' not in console:
            raise RuntimeError('no actual USB keyboard/mouse/tablet event verification')
        print('USB_HID_QEMU_PASS')
if __name__=='__main__':main()
