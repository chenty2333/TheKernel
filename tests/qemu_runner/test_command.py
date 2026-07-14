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

    def test_riscv_command_has_stable_mmio_drive_slots(self) -> None:
        command = build_qemu_command(
            arch="rv",
            kernel=Path("kernel-rv"),
            rootfs=Drive(Path("root.img"), "snapshot"),
            extra_block=Drive(Path("extra.img"), "rw"),
        )
        text = " ".join(command)
        self.assertEqual(command[0], "qemu-system-riscv64")
        self.assertIn("id=rootfs,snapshot=on", text)
        self.assertIn("id=extra", text)
        self.assertIn("bus=virtio-mmio-bus.0", text)
        self.assertIn("bus=virtio-mmio-bus.1", text)
        self.assertNotIn("bus=virtio-mmio-bus.2", text)
        self.assertLess(
            command.index("virtio-blk-device,drive=extra,bus=virtio-mmio-bus.1"),
            command.index("virtio-net-device,netdev=net0"),
        )

    def test_loongarch_command_uses_pci_devices(self) -> None:
        command = build_qemu_command(
            arch="la",
            kernel=Path("kernel-la"),
            rootfs=Drive(Path("root.img"), "readonly"),
            qemu_binary="custom-qemu",
            cpus=2,
            memory="2G",
        )
        text = " ".join(command)
        self.assertEqual(command[0], "custom-qemu")
        self.assertIn("-machine virt", text)
        self.assertIn("virtio-blk-pci,drive=rootfs", text)
        self.assertIn("virtio-rng-pci", text)
        self.assertIn("-smp 2", text)
        self.assertIn("-m 2G", text)


if __name__ == "__main__":
    unittest.main()
