#!/usr/bin/env python3
"""DbC-enabled QEMU xHCI absence: actual USB inputs and intact screen/shell."""
from pathlib import Path
import sys
import os
import subprocess
import tempfile
import dataclasses
ROOT=Path(__file__).resolve().parents[2]
sys.path.insert(0,str(ROOT))
from tools import thekernel as product
from tools.qemu_runner.model import QmpCheckpoint, QmpConsoleLine
from tools.qemu_runner.console_font import ConsoleFont
from tools.product_state import state_root, selected_tool_payload

def main():
    # Match the public CLI normalization before artifact fingerprint validation.
    os.environ["THEKERNEL_TOOLCHAIN"]=selected_tool_payload()
    args=product.build_parser().parse_args(['build','--platform','n305','--profile','shell','--usb-dbc'])
    artifacts=product.artifacts_for(args)
    subprocess.run([sys.executable,str(ROOT/'tools/thekernel.py'),'build','--platform','n305','--profile','shell','--usb-dbc'],cwd=ROOT,check=True)
    runs=state_root()/'runs';runs.mkdir(parents=True,exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='usb-dbc-',dir=runs) as directory:
        directory=Path(directory);commands=directory/'commands'
        commands.write_text('/opt/thekernel-tests/bin/thekernel-usb-input-smoke\n\x15dmesg | grep usb-dbc\necho DBC_ABSENT_OK\nsleep 6\npoweroff -f\n')
        def key(name,down):return {'type':'key','data':{'down':down,'key':{'type':'qcode','data':name}}}
        batches=(
            (key('a',True),), (key('a',False),),
            ({'type':'rel','data':{'axis':'x','value':17}},{'type':'rel','data':{'axis':'y','value':-9}}),
            ({'type':'abs','data':{'axis':'x','value':16384}},{'type':'abs','data':{'axis':'y','value':24576}}),
        )
        oracle=dataclasses.replace(product.FBCON_TEXT_CELLS,expected_lines=(QmpConsoleLine('intact shell screen',ConsoleFont.load().cells('DBC_ABSENT_OK')),))
        spec=product.RunSpec(accel='kvm',timeout=120,workdir=directory/'run',interactive=False,
            input_after_marker='THEKERNEL_SHELL_READY',stop_after_marker=None,commands=commands,
            extra_block=None,run_cpus=4,input_backend='usb', graphics_profile='firmware-fb',
            qmp_checkpoints=(QmpCheckpoint(input_after_marker='N305_USB_INPUT_WAIT',input_events=batches,
                screenshot=directory/'screen.ppm',screenshot_after_marker='DBC_ABSENT_OK',screenshot_text_cells=oracle),),qmp_timeout_secs=100)
        if product.run_product(artifacts,spec):raise RuntimeError('USB HID guest did not shut down cleanly')
        console=(directory/'run/console.log').read_text().replace('\r','')
        if '\nN305_USB_INPUT_PASS keyboard relative absolute\n' not in console:
            raise RuntimeError('no actual USB keyboard/mouse/tablet event verification')
        if 'usb-dbc: capability absent; normal consoles unchanged' not in console or '\nDBC_ABSENT_OK\n' not in console:
            raise RuntimeError('DbC absence verdict or live shell missing')
        print('USB_DBC_QEMU_ABSENCE_PASS actual-usb-inputs=verified framebuffer-glyphs=verified clean-exit=0')
        print('Not a real DbC transfer/link test; QEMU has no debug capability.')
if __name__=='__main__':main()
