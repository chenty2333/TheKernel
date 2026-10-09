#!/usr/bin/env python3
"""Actual USB keyboard/report arrays, mouse and tablet evdev input via QMP."""
import argparse
from contextlib import nullcontext
from pathlib import Path
import sys
import os
import subprocess
import tempfile
ROOT=Path(__file__).resolve().parents[2]
sys.path.insert(0,str(ROOT))
from tools import thekernel as product
from tools.qemu_runner.model import QmpCheckpoint, QmpUsbHotplug
from tools.product_state import state_root, selected_tool_payload

def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--hotplug',action='store_true',help='also unplug/replug the keyboard and verify old-fd revocation')
    parser.add_argument('--workdir',type=Path,help='retain this run in an explicit debugging directory')
    options=parser.parse_args()
    # Match the public CLI normalization before artifact fingerprint validation.
    os.environ["THEKERNEL_TOOLCHAIN"]=selected_tool_payload()
    args=product.build_parser().parse_args(['build','--platform','n305','--profile','shell'])
    artifacts=product.artifacts_for(args)
    subprocess.run([sys.executable,str(ROOT/'tools/thekernel.py'),'build','--platform','n305','--profile','shell'],cwd=ROOT,check=True)
    runs=state_root()/'runs';runs.mkdir(parents=True,exist_ok=True)
    context=(nullcontext(options.workdir.expanduser().resolve()) if options.workdir else
        tempfile.TemporaryDirectory(prefix='usb-hid-',dir=runs))
    with context as directory:
        directory=Path(directory);directory.mkdir(parents=True,exist_ok=True);commands=directory/'commands'
        commands.write_text(('/opt/thekernel-tests/bin/thekernel-usb-input-smoke --hotplug\n\x15' if options.hotplug else '')+
            '/opt/thekernel-tests/bin/thekernel-usb-input-smoke\n\x15poweroff -f\n')
        def key(name,down):return {'type':'key','data':{'down':down,'key':{'type':'qcode','data':name}}}
        batches=(
            (key('a',True),), (key('a',False),),
            ({'type':'rel','data':{'axis':'x','value':17}},{'type':'rel','data':{'axis':'y','value':-9}}),
            ({'type':'abs','data':{'axis':'x','value':16384}},{'type':'abs','data':{'axis':'y','value':24576}}),
        )
        checkpoints=()
        if options.hotplug:
            checkpoints=(
                QmpCheckpoint(input_after_marker='N305_USB_HOTPLUG_UNPLUG_WAIT',usb_hotplug=(QmpUsbHotplug('del','input-kbd'),)),
                QmpCheckpoint(input_after_marker='N305_USB_HOTPLUG_REPLUG_WAIT',usb_hotplug=(QmpUsbHotplug('add','input-kbd','usb-kbd','xhci.0','1'),)),
                QmpCheckpoint(input_after_marker='N305_USB_HOTPLUG_KEY_WAIT',input_events=((key('a',True),),(key('a',False),))),
            )
        spec=product.RunSpec(accel='kvm',timeout=120,workdir=directory/'run',interactive=False,
            input_after_marker='THEKERNEL_SHELL_READY',stop_after_marker=None,commands=commands,
            extra_block=None,run_cpus=4,input_backend='usb',
            qmp_checkpoints=checkpoints+(QmpCheckpoint(input_after_marker='N305_USB_INPUT_WAIT',input_events=batches),),qmp_timeout_secs=100)
        if product.run_product(artifacts,spec):raise RuntimeError('USB HID guest did not shut down cleanly')
        console=(directory/'run/console.log').read_text().replace('\r','')
        if '\nN305_USB_INPUT_PASS keyboard relative absolute\n' not in console:
            raise RuntimeError('no actual USB keyboard/mouse/tablet event verification')
        if options.hotplug and '\nN305_USB_HOTPLUG_PASS old_fd=revoked route=stable new_keyboard=verified\n' not in console:
            raise RuntimeError('no actual USB unplug/replug and stale-fd verification')
        print('USB_HID_HOTPLUG_QEMU_PASS' if options.hotplug else 'USB_HID_QEMU_PASS')
if __name__=='__main__':main()
