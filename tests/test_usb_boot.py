import binascii
import contextlib
import importlib.util
import io
import os
from pathlib import Path
import stat
import struct
from types import SimpleNamespace
import unittest
from unittest.mock import patch
import uuid
from tests.support import test_tmpdir
from tools.qemu_runner.command import CommandError, build_qemu_command
from tools.qemu_runner.model import Drive

ROOT=Path(__file__).resolve().parents[1]
def module(name,path):
    spec=importlib.util.spec_from_file_location(name,path);value=importlib.util.module_from_spec(spec);spec.loader.exec_module(value);return value
build=module('usb_build',ROOT/'scripts/build-usb-boot.py')
write=module('usb_write',ROOT/'scripts/write-usb-boot.py')

class UsbBootTests(unittest.TestCase):
    def test_gpt_primary_backup_crc_and_root_guid(self):
        entries=build.table([(build.ESP_TYPE,2048,8191,'ESP'),(build.ROOT_TYPE,8192,16383,'ROOT')])
        disk=uuid.uuid4()
        for at,other,array_lba in [(1,32767,2),(32767,1,32735)]:
            header=build.header(at,other,32768,array_lba,disk,entries)
            self.assertEqual(header[:8],b'EFI PART')
            expected=struct.unpack_from('<I',header,16)[0];header[16:20]=b'\0'*4
            self.assertEqual(binascii.crc32(header[:92]),expected)
            self.assertEqual(struct.unpack_from('<I',header,88)[0],binascii.crc32(entries))
        self.assertEqual(entries[128:144],build.ROOT_TYPE.bytes_le)
    def test_usb_boot_has_no_sata_esp_or_virtio_root(self):
        args=dict(arch='x86_64',kernel=Path('kernel'),rootfs=None,esp=Drive(Path('esp'),'snapshot'),
                  ovmf_code=Path('code'),ovmf_vars=Path('vars'),usb_disk=Drive(Path('usb'),'snapshot'),usb_boot=True)
        command=build_qemu_command(**args)
        joined=' '.join(command)
        self.assertNotIn('if=ide',joined);self.assertNotIn('virtio-blk',joined)
        self.assertIn('usb-storage,id=usb-storage,bus=xhci.0,drive=usb-disk,bootindex=1',command)
        for changes in [dict(usb_disk=None),dict(rootfs=Drive(Path('root'),'snapshot')),dict(direct_kernel=True)]:
            with self.assertRaises(CommandError):build_qemu_command(**(args|changes))
    def fixture(self,base):
        device=base/'dev/sdb';device.parent.mkdir();device.touch()
        sysfs=base/'sys';node=sysfs/'devices/pci/usb1/1-1/block/sdb';node.mkdir(parents=True)
        (node/'removable').write_text('1');(node/'size').write_text('8192');(node/'holders').mkdir()
        devlink=sysfs/'dev/block';devlink.mkdir(parents=True);(devlink/'8:16').symlink_to(node)
        mountinfo=base/'mountinfo';mountinfo.write_text('')
        old=Path.stat
        def fake_stat(path,*args,**kwargs):
            if path==device:return SimpleNamespace(st_mode=stat.S_IFBLK|0o600,st_rdev=os.makedev(8,16))
            return old(path,*args,**kwargs)
        return device,sysfs,node,mountinfo,fake_stat
    def test_writer_refuses_internal_partition_mounted_held_small_and_nvme_aliases(self):
        with test_tmpdir() as directory:
            base=Path(directory);dev,sysfs,node,mountinfo,fake=self.fixture(base)
            with patch.object(Path,'stat',new=fake):
                self.assertEqual(write.inspect(dev,4096,sysfs=sysfs,mountinfo=mountinfo)[1],os.makedev(8,16))
                for file,value in [('removable','0'),('partition','1'),('size','1')]:
                    old=(node/file).read_text() if (node/file).exists() else None
                    (node/file).write_text(value)
                    with self.assertRaises(write.UnsafeDevice):write.inspect(dev,4096,sysfs=sysfs,mountinfo=mountinfo)
                    if old is None:(node/file).unlink()
                    else:(node/file).write_text(old)
                mountinfo.write_text('1 0 8:16 / /mnt rw - ext4 /dev/sdb rw\n')
                with self.assertRaises(write.UnsafeDevice):write.inspect(dev,4096,sysfs=sysfs,mountinfo=mountinfo)
                mountinfo.write_text('');(node/'holders/dm-0').touch()
                with self.assertRaises(write.UnsafeDevice):write.inspect(dev,4096,sysfs=sysfs,mountinfo=mountinfo)
            nvme=base/'dev/nvme0n1';nvme.touch();alias=base/'alias';alias.symlink_to(nvme)
            with self.assertRaises(write.UnsafeDevice):write.inspect(alias,1,sysfs=sysfs,mountinfo=mountinfo)
    def test_dry_run_never_opens_target_and_yes_requires_sudo(self):
        with test_tmpdir() as directory:
            base=Path(directory);image=base/'image';image.write_bytes(b'image');dev=base/'device'
            for yes in (False,True):
                with patch('sys.argv',['writer','--image',str(image),'--device',str(dev)]+(['--yes'] if yes else [])), \
                     patch.object(write,'inspect',return_value=(dev,1)),patch.object(write.os,'geteuid',return_value=1000), \
                     patch.object(write.os,'open') as opened,patch.object(write.subprocess,'run') as ran, \
                     contextlib.redirect_stdout(io.StringIO()):
                    if yes:
                        with self.assertRaises(SystemExit):write.main()
                    else:write.main()
                    opened.assert_not_called();ran.assert_not_called()
