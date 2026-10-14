#!/usr/bin/env python3
"""Real SCI button -> deferred flush -> S5, not QMP quit or a host kill."""
from pathlib import Path
import os
import subprocess
import sys
import tempfile
ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT))
from tools.product_state import state_root

def validate(kernel: str) -> None:
    markers = ('fixed-button=true S5=Some([0, 0])',
               'power button; notifying init with SIGPWR',
               'acpi-power: filesystems flushed; entering S5')
    at = -1
    for marker in markers:
        at = kernel.find(marker, at + 1)
        if at < 0:
            raise RuntimeError('guest did not complete ordered ACPI shutdown: ' + marker)

def main() -> None:
    runs = state_root() / 'runs'
    runs.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='acpi-power-', dir=runs) as directory:
        subprocess.run([sys.executable, str(ROOT / 'tools/thekernel.py'), 'run', '--platform', 'n305',
                        '--profile', 'shell', '--accel', 'kvm', '--powerdown-after-marker',
                        'THEKERNEL_SHELL_READY', '--workdir', directory, '--timeout', '100'],
                       cwd=ROOT, env=os.environ, check=True)
        validate((Path(directory) / 'kernel.log').read_text())
        print('ACPI_POWER_QEMU_PASS', flush=True)
if __name__ == '__main__': main()
