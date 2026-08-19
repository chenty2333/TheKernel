from __future__ import annotations

import unittest
import sys
from pathlib import Path

from tools.qemu_runner.command import CommandError, build_qemu_command, drive_options
from tools.qemu_runner.model import Drive


class CommandTests(unittest.TestCase):
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

    def test_performance_topology_selects_rootless_passt(self) -> None:
        command = build_qemu_command(
            arch="x86_64",
            kernel=Path("kernel-x86_64"),
            rootfs=Drive(Path("root.img"), "snapshot"),
            extra_block=Drive(Path("data.img"), "rw"),
            direct_kernel=True,
            iothread_id="perf-io",
            network="passt",
        )
        text = " ".join(command)
        self.assertIn("-netdev passt,id=net0", text)
        self.assertIn("cache=none", text)
        self.assertIn("aio=threads", text)
        self.assertIn("num-queues=1", text)
        self.assertIn("queue-size=128", text)
        self.assertIn("request-merging=off", text)
        self.assertIn("discard=off", text)
        self.assertIn("write-zeroes=off", text)
        self.assertIn("ioeventfd=on", text)
        self.assertIn("event_idx=on", text)
        self.assertIn("virtio-blk-pci,drive=extra,iothread=perf-io", text)

    def test_tap_vhost_has_explicit_tap_backend(self) -> None:
        command = build_qemu_command(
            arch="x86_64",
            kernel=Path("kernel-x86_64"),
            rootfs=Drive(Path("root.img"), "snapshot"),
            direct_kernel=True,
            network_mode="tap-vhost",
            tap_name="tk0",
        )
        self.assertIn("tap,id=net0,vhost=on,ifname=tk0", " ".join(command))

    def test_tap_names_allow_common_net_names_and_reject_injection(self) -> None:
        base = dict(
            arch="x86_64",
            kernel=Path("kernel-x86_64"),
            rootfs=Drive(Path("root.img"), "snapshot"),
            direct_kernel=True,
            network="tap-vhost",
        )
        self.assertIn("ifname=tap-net0", " ".join(build_qemu_command(**base, tap_name="tap-net0")))
        for invalid in ("tap,net0", "tap=net0", "tap\nnet0", "tap\rnet0"):
            with self.assertRaises(CommandError):
                build_qemu_command(**base, tap_name=invalid)

    def test_python_launcher_is_a_full_argv_prefix(self) -> None:
        command = build_qemu_command(
            arch="x86_64",
            kernel=Path("kernel-x86_64"),
            rootfs=Drive(Path("root.img"), "snapshot"),
            direct_kernel=True,
            qemu_binary="/usr/bin/qemu-system-x86_64",
            qemu_launcher=(sys.executable, "tools/kvm_scheduler_pinner.py"),
        )
        self.assertEqual(command[:2], (sys.executable, "tools/kvm_scheduler_pinner.py"))
        self.assertNotIn("/usr/bin/qemu-system-x86_64", command)


if __name__ == "__main__":
    unittest.main()
