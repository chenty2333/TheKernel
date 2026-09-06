from __future__ import annotations

import unittest
from pathlib import Path

from tools.qemu_runner.command import CommandError, build_qemu_command, drive_options
from tools.qemu_runner.model import Drive
from tools.qemu_runner.profiles import (
    BENCHMARK_PROFILES,
    GRAPHICS_PROFILES,
    graphics_device,
)


class GraphicsProfileTableTests(unittest.TestCase):
    def test_profile_table_is_the_single_profile_list(self) -> None:
        self.assertEqual(tuple(GRAPHICS_PROFILES), (
            "headless",
            "interactive",
            "virgl-headless",
            "virgl-interactive",
            "venus-interactive",
        ))
        for name, topology in GRAPHICS_PROFILES.items():
            with self.subTest(profile=name):
                self.assertTrue(topology.display)
                self.assertTrue(topology.device.startswith("virtio-gpu"))
                self.assertIn(topology.renderer, (None, "software", "virgl", "venus"))

    def test_benchmark_profiles_are_exactly_the_renderer_backed_profiles(self) -> None:
        self.assertEqual(BENCHMARK_PROFILES, (
            "headless",
            "virgl-headless",
            "virgl-interactive",
            "venus-interactive",
        ))

    def test_graphics_device_appends_the_requested_scanout_geometry(self) -> None:
        self.assertEqual(
            graphics_device("headless", 800, 600),
            "virtio-gpu-pci,max_outputs=1,xres=800,yres=600",
        )
        self.assertEqual(
            graphics_device("venus-interactive", 3840, 2160),
            "virtio-gpu-gl-pci,blob=on,venus=on,hostmem=1G,"
            "max_hostmem=1G,max_outputs=1,xres=3840,yres=2160",
        )


class CommandTests(unittest.TestCase):
    def test_diagnostics_use_second_serial_with_keyval_escaped_path(self):
        command = build_qemu_command(arch="x86_64", kernel=Path("kernel"),
            rootfs=None, direct_kernel=True,
            diagnostic_log_path=Path("/home/logs,a b/kernel.log"))
        serials = [command[i + 1] for i, arg in enumerate(command) if arg == "-serial"]
        self.assertEqual(serials, ["stdio", "chardev:kernel-log"])
        self.assertIn("file,id=kernel-log,path=/home/logs,,a b/kernel.log,append=on", command)

    def test_serial_topology_cannot_be_overridden(self):
        for option in ("-serial", "-serial=stdio", "-chardev", "-nographic"):
            with self.subTest(option=option), self.assertRaises(CommandError):
                build_qemu_command(arch="x86_64", kernel=Path("kernel"),
                    rootfs=None, direct_kernel=True, extra_args=(option,))

    def test_accelerated_x86_commands_select_a_matching_cpu_model(self) -> None:
        base = dict(
            arch="x86_64",
            kernel=Path("kernel"),
            rootfs=Drive(Path("root.img"), "snapshot"),
            direct_kernel=True,
        )
        for accel, cpu_model in (("kvm", "host"), ("tcg", "max")):
            with self.subTest(accel=accel):
                command = build_qemu_command(**base, accel=accel)
                self.assertEqual(command.count("-accel"), 1)
                self.assertEqual(command[command.index("-accel") + 1], accel)
                self.assertEqual(command.count("-cpu"), 1)
                self.assertEqual(command[command.index("-cpu") + 1], cpu_model)

    def test_unaccelerated_x86_command_keeps_the_generic_cpu_default(self) -> None:
        command = build_qemu_command(
            arch="x86_64",
            kernel=Path("kernel"),
            rootfs=Drive(Path("root.img"), "snapshot"),
            direct_kernel=True,
        )
        self.assertNotIn("-cpu", command)

    def test_extra_args_cannot_override_accelerator_or_cpu_model(self) -> None:
        base = dict(
            arch="x86_64",
            kernel=Path("kernel"),
            rootfs=Drive(Path("root.img"), "snapshot"),
            direct_kernel=True,
            accel="kvm",
        )
        for argument in ("-cpu", "-cpu=max", "-accel", "-accel=tcg"):
            with self.subTest(argument=argument):
                with self.assertRaisesRegex(CommandError, "runner-owned"):
                    build_qemu_command(**base, extra_args=(argument, "tcg"))

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

    def test_q35_venus_4g_places_ram_above_the_pci_hole(self) -> None:
        command = build_qemu_command(
            arch="x86_64",
            kernel=Path("kernel-x86_64.elf"),
            rootfs=Drive(Path("root.img"), "snapshot"),
            direct_kernel=True,
            memory="4G",
            cpus=4,
            graphics_profile="venus-interactive",
        )
        text = " ".join(command)
        self.assertIn("-machine q35,max-ram-below-4g=2G", text)
        self.assertIn("-m 4G", text)
        self.assertIn(
            "virtio-gpu-gl-pci,blob=on,venus=on,hostmem=1G,"
            "max_hostmem=1G,max_outputs=1,xres=800,yres=600",
            text,
        )

    def test_q35_venus_preserves_requested_scanout_geometry(self) -> None:
        command = build_qemu_command(
            arch="x86_64",
            kernel=Path("kernel-x86_64.elf"),
            rootfs=Drive(Path("root.img"), "snapshot"),
            direct_kernel=True,
            graphics_profile="venus-interactive",
            graphics_width=3840,
            graphics_height=2160,
        )
        self.assertIn(
            "virtio-gpu-gl-pci,blob=on,venus=on,hostmem=1G,"
            "max_hostmem=1G,max_outputs=1,xres=3840,yres=2160",
            " ".join(command),
        )

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
