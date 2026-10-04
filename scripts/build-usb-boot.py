#!/usr/bin/env python3
"""Build GPT ESP + x86-64 root partition without loop mounts or privileges."""
import argparse
import binascii
from pathlib import Path
import shutil
import struct
import subprocess
import tempfile
import uuid

ROOT = Path(__file__).resolve().parents[1]
SECTOR = 512
ESP_TYPE = uuid.UUID('c12a7328-f81f-11d2-ba4b-00a0c93ec93b')
ROOT_TYPE = uuid.UUID('4f68bce3-e8cd-4db1-96e7-fbcaf984b709')

def table(entries):
    result = bytearray(128 * 128)
    for i, (kind, first, last, name) in enumerate(entries):
        struct.pack_into('<16s16sQQQ72s', result, i * 128, kind.bytes_le, uuid.uuid4().bytes_le,
                         first, last, 0, name.encode('utf-16le').ljust(72, b'\0'))
    return result

def header(here, other, total, array_lba, disk_id, array):
    result = bytearray(SECTOR)
    struct.pack_into('<8sIIIIQQQQ16sQIII', result, 0, b'EFI PART', 0x10000, 92, 0, 0,
                     here, other, 34, total - 34, disk_id.bytes_le, array_lba, 128, 128,
                     binascii.crc32(array))
    struct.pack_into('<I', result, 16, binascii.crc32(result[:92]))
    return result

def build(kernel: Path, rootfs: Path, output: Path, kernel_cmdline: str=""):
    kernel = kernel.resolve(strict=True); rootfs = rootfs.resolve(strict=True)
    output = output.resolve()
    if output in (kernel, rootfs) or output.exists():
        raise ValueError('output must be a new file, not an existing artifact or device')
    # Same on-disk validation as the product; never put VM images on tmpfs.
    import sys
    sys.path.insert(0, str(ROOT))
    from tools.product_state import validate_storage
    validate_storage(output.parent)
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='usb-build-', dir=output.parent) as directory:
        work = Path(directory)
        config = work / 'grub.cfg'
        from tools.kernel_cmdline import append_kernel_cmdline
        if any(word.startswith('root=') for word in kernel_cmdline.split()):
            raise ValueError('USB boot owns root=usb; do not supply another root=')
        config.write_text(append_kernel_cmdline((ROOT / 'config/x86_64/grub-drive.cfg').read_text(),
                                               'root=usb ' + kernel_cmdline))
        esp = work / 'esp.img'
        subprocess.run(['bash', str(ROOT / 'scripts/build-x86-uefi-esp.sh'), '--mode', 'multiboot-drive',
                        '--kernel', str(kernel), '--output', str(esp), '--grub-config', str(config)], check=True)
        with esp.open('rb') as source:
            source.seek(SECTOR); old_header = source.read(SECTOR)
            source.seek(struct.unpack_from('<Q',old_header,72)[0] * SECTOR)
            first_entry = source.read(128)
            start, end = struct.unpack_from('<QQ', first_entry,32)
            root_start = ((end + 1 + 2047) // 2048) * 2048
            root_size = (rootfs.stat().st_size + SECTOR - 1) // SECTOR
            if root_size == 0: raise ValueError('empty rootfs')
            total = ((root_start + root_size + 33 + 2047) // 2048) * 2048
            array = table([(ESP_TYPE,start,end,'THEKERNEL-ESP'),
                           (ROOT_TYPE,root_start,root_start + root_size - 1,'THEKERNEL-ROOT')])
            disk_id = uuid.uuid4()
            image = work / 'usb.img'
            with image.open('w+b') as dest:
                dest.truncate(total * SECTOR)
                mbr=bytearray(SECTOR); mbr[446:462]=struct.pack('<B3sB3sII',0,b'\0\x02\0',0xee,b'\xff'*3,1,min(total-1,0xffffffff));mbr[510:]=b'\x55\xaa'
                dest.write(mbr); dest.write(header(1,total-1,total,2,disk_id,array));dest.write(array)
                dest.seek(start * SECTOR);source.seek(start * SECTOR)
                remaining = (end-start+1)*SECTOR
                while remaining:
                    chunk=source.read(min(1024*1024,remaining))
                    if not chunk:raise ValueError('truncated generated ESP')
                    dest.write(chunk);remaining-=len(chunk)
                dest.seek(root_start * SECTOR)
                with rootfs.open('rb') as src: shutil.copyfileobj(src,dest,1024*1024)
                dest.seek((total-33)*SECTOR);dest.write(array)
                dest.write(header(total-1,1,total,total-33,disk_id,array))
            image.rename(output)
    print(f'USB_BOOT_IMAGE={output} root_lba={root_start} root_sectors={root_size}')

def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--kernel',type=Path,required=True);parser.add_argument('--rootfs',type=Path,required=True)
    parser.add_argument('--output',type=Path,required=True)
    parser.add_argument('--kernel-cmdline',default='',help='append safe arguments to USB GRUB (root=usb is fixed)')
    args=parser.parse_args()
    try: build(args.kernel,args.rootfs,args.output,args.kernel_cmdline)
    except (ValueError,OSError,subprocess.CalledProcessError) as error:parser.exit(1,f'USB image: {error}\n')
if __name__=='__main__':main()
