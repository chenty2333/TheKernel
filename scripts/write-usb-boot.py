#!/usr/bin/env python3
"""Removable whole-USB-disk writer. Dry run by default; sudo + --yes required."""
import argparse
import os
from pathlib import Path
import shlex
import stat
import subprocess

class UnsafeDevice(ValueError): pass

def inspect(device: Path, image_bytes: int, *, sysfs: Path=Path('/sys'), mountinfo: Path=Path('/proc/self/mountinfo')):
    device = device.resolve(strict=True)
    if device.name.startswith('nvme') or any('nvme' in part for part in device.parts):
        raise UnsafeDevice('NVMe is explicitly forbidden, including aliases')
    info = device.stat()
    if not stat.S_ISBLK(info.st_mode): raise UnsafeDevice('target must be a block device, not a file')
    node=(sysfs/'dev/block'/f'{os.major(info.st_rdev)}:{os.minor(info.st_rdev)}').resolve(strict=True)
    if any(part.startswith('nvme') for part in node.parts): raise UnsafeDevice('NVMe is explicitly forbidden')
    if (node/'partition').exists(): raise UnsafeDevice('target must be a whole disk, not a partition')
    if (node/'removable').read_text().strip() != '1': raise UnsafeDevice('non-removable target refused')
    # USB transport is required as well: no SD, loop, mapper or internal drive.
    if not any(part.startswith('usb') for part in node.parts): raise UnsafeDevice('target is not on a USB bus')
    if int((node/'size').read_text())*512 < image_bytes: raise UnsafeDevice('target is smaller than image')
    numbers={f'{os.major(info.st_rdev)}:{os.minor(info.st_rdev)}'}
    for child in node.iterdir():
        if child.is_dir() and (child/'partition').exists():numbers.add((child/'dev').read_text().strip())
    for line in mountinfo.read_text().splitlines():
        parts=line.split()
        if len(parts)>2 and parts[2] in numbers: raise UnsafeDevice('disk or a partition is mounted')
    if (node/'holders').exists() and any((node/'holders').iterdir()): raise UnsafeDevice('disk has active holders')
    for child in node.iterdir():
        if child.is_dir() and (child/'partition').exists() and (child/'holders').exists() and any((child/'holders').iterdir()):
            raise UnsafeDevice('partition has active holders')
    # A block swap device need not appear in mountinfo.
    for line in Path('/proc/swaps').read_text().splitlines()[1:]:
        source=Path(line.split()[0])
        try:
            st=source.stat()
            if stat.S_ISBLK(st.st_mode) and f'{os.major(st.st_rdev)}:{os.minor(st.st_rdev)}' in numbers:
                raise UnsafeDevice('disk or partition is active swap')
        except FileNotFoundError: pass
    return device, info.st_rdev

def main():
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('--image',type=Path,required=True)
    p.add_argument('--device',type=Path,required=True);p.add_argument('--yes',action='store_true');a=p.parse_args()
    try:
        image=a.image.resolve(strict=True)
        if not image.is_file() or image.stat().st_size==0:raise UnsafeDevice('image must be a nonempty regular file')
        dev, identity=inspect(a.device,image.stat().st_size)
        command=['dd',f'if={image}',f'of={dev}','bs=4M','conv=fsync','status=progress']
        print('DESTRUCTIVE: all contents of '+str(dev)+' will be overwritten.')
        print('sudo '+shlex.join(command))
        if not a.yes:
            print('Dry run only. Re-run this script with sudo and --yes to write.');return
        if os.geteuid()!=0:raise UnsafeDevice('--yes requires the user to run this script with sudo')
        # Revalidate immediately before writing; pin and exclusively open the
        # chosen block inode so pathname replacement cannot redirect dd.
        again, current=inspect(dev,image.stat().st_size)
        if current!=identity:raise UnsafeDevice('target identity changed')
        fd=os.open(again,os.O_WRONLY|os.O_EXCL|os.O_NOFOLLOW)
        try:
            if os.fstat(fd).st_rdev!=identity:raise UnsafeDevice('target identity changed after open')
            subprocess.run(['dd',f'if={image}',f'of=/proc/self/fd/{fd}','bs=4M','conv=fsync','status=progress'],pass_fds=(fd,),check=True)
        finally:os.close(fd)
    except (ValueError,OSError,subprocess.CalledProcessError) as error:p.exit(1,f'USB write refused/failed: {error}\n')
if __name__=='__main__':main()
