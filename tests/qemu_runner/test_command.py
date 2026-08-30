from __future__ import annotations

import unittest
from pathlib import Path

from tools.qemu_runner.command import build_qemu_command, drive_options
from tools.qemu_runner.model import Drive


class CommandTests(unittest.TestCase):
    def test_q35_graphics_profiles_are_explicit_and_virtio_only(self) -> None:
        base = dict(
            arch="x86_64", kernel=Path("kernel"), rootfs=Drive(Path("root.img"), "snapshot"),
            direct_kernel=True,
        )
        headless = build_qemu_command(**base, qmp_socket=Path("run/qmp.sock"))
        interactive = build_qemu_command(**base, graphics_profile="interactive", qmp_socket=Path("run/qmp.sock"))
        headless_text = " ".join(headless)
        interactive_text = " ".join(interactive)
        self.assertNotIn("-nographic", headless)
        self.assertIn("-nodefaults", headless)
        self.assertIn("-serial stdio", headless_text)
        self.assertIn("-display none", headless_text)
        self.assertIn("-display gtk", interactive_text)
        self.assertIn("virtio-gpu-pci,max_outputs=1,xres=800,yres=600", headless_text)
        self.assertIn("virtio-keyboard-pci", headless_text)
        self.assertIn("virtio-tablet-pci", headless_text)
        self.assertIn("-qmp unix:run/qmp.sock,server=on,wait=off", headless_text)
        self.assertIn("-qmp unix:run/qmp.sock,server=on,wait=off", interactive_text)

    def test_q35_virgl_profiles_use_only_explicit_gl_topologies(self) -> None:
        base = dict(
            arch="x86_64", kernel=Path("kernel"), rootfs=Drive(Path("root.img"), "snapshot"),
            direct_kernel=True,
        )
        headless = build_qemu_command(**base, graphics_profile="virgl-headless")
        interactive = build_qemu_command(**base, graphics_profile="virgl-interactive")
        headless_text = " ".join(headless)
        interactive_text = " ".join(interactive)
        self.assertIn("-display egl-headless,gl=on", headless_text)
        self.assertIn("-display gtk,gl=on", interactive_text)
        self.assertIn("virtio-gpu-gl-pci,max_outputs=1,xres=800,yres=600", headless_text)
        self.assertIn("virtio-gpu-gl-pci,max_outputs=1,xres=800,yres=600", interactive_text)
        self.assertNotIn("blob=on", headless_text)
        self.assertNotIn("venus=on", headless_text)
    def test_drive_modes_are_explicit(self) -> None:
        path = Path("/tmp/root,image.img")
        self.assertIn("aio=threads", drive_options(path, "rootfs", mode="rw"))
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
        self.assertIn("if=pflash,format=raw,readonly=on,aio=threads,file=OVMF_CODE.fd", text)
        self.assertIn("if=pflash,format=raw,aio=threads,file=OVMF_VARS.fd", text)
        self.assertIn("file=esp.img,if=ide,format=raw,snapshot=on,aio=threads", text)
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
