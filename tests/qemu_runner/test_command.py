from __future__ import annotations

import unittest
from pathlib import Path

from tools.qemu_runner.command import build_qemu_command, drive_options
from tools.qemu_runner.model import Drive


class CommandTests(unittest.TestCase):
    def test_drive_modes_are_explicit(self) -> None:
        path = Path("/tmp/root,image.img")
        self.assertIn("snapshot=on", drive_options(path, "rootfs", mode="snapshot"))
        self.assertIn("readonly=on", drive_options(path, "rootfs", mode="readonly"))
        self.assertNotIn("snapshot=on", drive_options(path, "rootfs", mode="rw"))
        self.assertIn("root,,image.img", drive_options(path, "rootfs", mode="rw"))

    def test_x86_direct_command_uses_stable_pci_drive_slots(self) -> None:
        command = build_qemu_command(
            arch="x86_64",
            kernel=Path("kernel-x86_64"),
            rootfs=Drive(Path("root.img"), "snapshot"),
            extra_block=Drive(Path("extra.img"), "rw"),
            direct_kernel=True,
        )
        text = " ".join(command)
        self.assertEqual(command[0], "qemu-system-x86_64")
        self.assertIn("-kernel", command)
        self.assertIn("id=rootfs,snapshot=on", text)
        self.assertIn("id=extra", text)
        self.assertIn("virtio-blk-pci,drive=rootfs", text)
        self.assertIn("virtio-blk-pci,drive=extra", text)
        self.assertNotIn("virtio-blk-device", text)

    def test_x86_64_command_uses_ovmf_esp_and_pci_devices(self) -> None:
        command = build_qemu_command(
            arch="x86_64",
            kernel=Path("kernel-x86_64.elf"),
            rootfs=Drive(Path("root.img"), "snapshot"),
            extra_block=Drive(Path("extra.img"), "readonly"),
            esp=Drive(Path("esp.img"), "snapshot"),
            ovmf_code=Path("OVMF_CODE.fd"),
            ovmf_vars=Path("OVMF_VARS.fd"),
        )
        text = " ".join(command)
        self.assertEqual(command[0], "qemu-system-x86_64")
        self.assertIn("-machine q35", text)
        self.assertNotIn("-kernel", command)
        self.assertIn("if=pflash,format=raw,readonly=on,file=OVMF_CODE.fd", text)
        self.assertIn("if=pflash,format=raw,file=OVMF_VARS.fd", text)
        self.assertIn("file=esp.img,if=ide,format=raw,snapshot=on", text)
        self.assertIn("id=rootfs,snapshot=on", text)
        self.assertIn("virtio-blk-pci,drive=rootfs", text)
        self.assertIn("id=extra,readonly=on", text)
        self.assertIn("virtio-blk-pci,drive=extra", text)
        self.assertIn("virtio-rng-pci,rng=rng0", text)
        self.assertIn("virtio-net-pci,netdev=net0", text)
        self.assertIn("-netdev user,id=net0", text)
        self.assertNotIn("-bios", command)
        self.assertNotIn("-net none", text)

    def test_x86_64_direct_kernel_mode_is_explicit_debug_path(self) -> None:
        command = build_qemu_command(
            arch="x86_64",
            kernel=Path("kernel-x86_64.elf"),
            rootfs=Drive(Path("root.img"), "snapshot"),
            direct_kernel=True,
        )
        self.assertIn("-kernel", command)
        self.assertNotIn("if=pflash", " ".join(command))


if __name__ == "__main__":
    unittest.main()
