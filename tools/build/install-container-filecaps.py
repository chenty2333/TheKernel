#!/usr/bin/env python3
"""Restore authenticated subordinate-ID helper attributes in an offline image."""
import argparse
import json
from pathlib import Path
import struct
import subprocess
import tempfile

TARGETS = {'/opt/thekernel-tools/bin/newuidmap': 7,
           '/opt/thekernel-tools/bin/newgidmap': 6}


def validate_filecaps(records):
    if not isinstance(records, dict) or records.keys() != TARGETS.keys():
        raise ValueError('exact subordinate-ID helper attribute set required')
    result = {}
    for path, cap in TARGETS.items():
        value = bytes.fromhex(records[path])
        # Linux v2 file-capability ABI: only the signed package's single
        # permitted bit plus effective flag, never root's complete capability set.
        expected = struct.pack('<IIIII', 0x02000001, 1 << cap, 0, 0, 0)
        if value != expected:
            raise ValueError(f'unexpected package capabilities: {path}')
        result[path] = value
    return result


def install(image, payload):
    records = json.loads((payload/'opt/thekernel-tools/file-capabilities.json').read_text())
    caps = validate_filecaps(records)
    # This temporary directory lives beside the persistent image, not on tmpfs.
    with tempfile.TemporaryDirectory(prefix='.container-caps-', dir=image.parent) as work:
        work = Path(work)
        for index, (path, value) in enumerate(caps.items()):
            if not (payload/path.lstrip('/')).is_file():
                raise ValueError(f'absent packaged helper: {path}')
            source, observed = work/f'cap-{index}', work/f'read-{index}'
            source.write_bytes(value)
            subprocess.run(['debugfs', '-w', '-R',
                            f'ea_set -f "{source}" {path} security.capability', str(image)],
                           check=True, capture_output=True)
            subprocess.run(['debugfs', '-R',
                            f'ea_get -f "{observed}" {path} security.capability', str(image)],
                           check=True, capture_output=True)
            if not observed.is_file() or observed.read_bytes() != value:
                raise ValueError(f'image capability insertion did not persist: {path}')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--image', required=True, type=Path)
    parser.add_argument('--payload', required=True, type=Path)
    args = parser.parse_args()
    install(args.image, args.payload)


if __name__ == '__main__':
    main()
