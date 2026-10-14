#!/usr/bin/env python3
"""Bounded real Q35 iTCO feeding/reboot regression, through the product runner."""
from pathlib import Path
import os
import re
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT))
from tools.product_state import state_root


def validate(console: str, kernel: str, mode: str) -> None:
    boots = kernel.count('Primary CPU 0 started,')
    if 'ITCO_ABI version=2 timeout=6' not in console:
        raise RuntimeError('guest did not exercise the emulated ICH9 watchdog ioctls')
    if mode == 'feed':
        if boots != 1 or 'ITCO_FEED_AND_MAGIC_CLOSE_OK' not in console:
            raise RuntimeError('feeding/magic-close run rebooted or failed')
    else:
        uptime = re.search(r'(?m)^ITCO_SECOND_KERNEL_UPTIME=(\d+\.\d+) ', console)
        if (boots != 2 or 'ITCO_SECOND_BOOT' not in console or 'ITCO_FAILED_TO_RESET' in console
                or uptime is None or float(uptime.group(1)) >= 6):
            raise RuntimeError('watchdog expiry did not really reboot into a fresh second kernel')


def main() -> None:
    (state_root() / 'runs').mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='itco-', dir=state_root() / 'runs') as directory:
        base = Path(directory)
        for mode in ('feed', 'expire'):
            work = base / mode
            commands = base / (mode + '.commands')
            commands.write_text('/opt/thekernel-tests/bin/thekernel-watchdog-check ' + mode + '\n'
                                + ("printf 'ITCO_SECOND_KERNEL_UPTIME='; cat /proc/uptime\necho ITCO_SECOND_BOOT\n" if mode == 'expire' else '')
                                + 'poweroff -f\n')
            command = [sys.executable, str(ROOT / 'tools/thekernel.py'), 'run', '--platform', 'n305',
                       '--profile', 'shell', '--accel', 'kvm', '--kernel-cmdline', 'watchdog.timeout=6',
                       '--commands', str(commands), '--workdir', str(work), '--timeout', '100']
            if mode == 'expire': command.append('--allow-reboot')
            subprocess.run(command, cwd=ROOT, env=os.environ, check=True)
            validate((work / 'console.log').read_text(), (work / 'kernel.log').read_text(), mode)
            print('ITCO_QEMU_' + mode.upper() + '_PASS', flush=True)

if __name__ == '__main__': main()
