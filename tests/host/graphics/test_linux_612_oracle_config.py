"""Static contract tests for the self-hosted Linux graphics oracle."""

from __future__ import annotations

import pathlib
import unittest

from tools.qemu_runner.graphics_benchmark import (
    BENCHMARK_INPUT_HOTPLUG_READY_MARKER,
    BENCHMARK_INPUT_HOTPLUG_REMOVED_MARKER,
    BENCHMARK_INPUT_SAMPLES,
    BENCHMARK_READY_MARKER,
    benchmark_checkpoints,
)


ROOT = pathlib.Path(__file__).resolve().parents[3]


class Linux612OracleConfigTests(unittest.TestCase):
    def test_reference_config_keeps_the_boot_critical_graphics_path_builtin(self) -> None:
        config = (ROOT / "config/linux/6.12.107-q35-graphics.config").read_text(encoding="utf-8")
        for setting in (
            "# CONFIG_UNWINDER_ORC is not set",
            "CONFIG_UNWINDER_FRAME_POINTER=y",
            "CONFIG_VIRTIO_PCI=y",
            "CONFIG_VIRTIO_BLK=y",
            "CONFIG_VIRTIO_INPUT=y",
            "CONFIG_PCIEPORTBUS=y",
            "CONFIG_HOTPLUG_PCI=y",
            "CONFIG_HOTPLUG_PCI_ACPI=y",
            "CONFIG_ACPI=y",
            "CONFIG_ACPI_PCI_SLOT=y",
            "CONFIG_EFI=y",
            "CONFIG_EFI_STUB=y",
            "CONFIG_EFIVAR_FS=y",
            "CONFIG_DRM_VIRTIO_GPU=y",
            "CONFIG_DRM_FBDEV_EMULATION=y",
            "CONFIG_INPUT_EVDEV=y",
            "CONFIG_VT=y",
            "CONFIG_SERIAL_8250_CONSOLE=y",
            "CONFIG_EXT4_FS=y",
            "CONFIG_DEVTMPFS_MOUNT=y",
            "CONFIG_EXT4_FS_POSIX_ACL=y",
            "CONFIG_DEBUG_FS=y",
            "CONFIG_SYSFS=y",
            "CONFIG_PROC_FS=y",
            "CONFIG_INOTIFY_USER=y",
            "# CONFIG_MODULES is not set",
        ):
            self.assertIn(setting, config)

    def test_linux_grub_entry_has_only_kernel_and_virtio_root(self) -> None:
        config = (ROOT / "config/x86_64/grub-linux.cfg").read_text(encoding="utf-8")
        self.assertIn("linux /vmlinuz root=/dev/vda console=ttyS0", config)
        self.assertNotIn("module", config)
        self.assertNotIn("rootfs-x86.img", config)

    def test_oracle_build_and_runner_use_persistent_cache_and_shared_protocol(self) -> None:
        build = (ROOT / "scripts/build-linux-612-oracle.sh").read_text(encoding="utf-8")
        runner = (ROOT / "scripts/graphics-linux-oracle-runner.py").read_text(encoding="utf-8")
        wrapper = (ROOT / "scripts/graphics-linux-oracle.sh").read_text(encoding="utf-8")
        self.assertIn("LINUX_VERSION=6.12.107", build)
        self.assertIn("O=\"$output\"", build)
        self.assertIn("/home/ava/.cache/thekernel-targets/linux-6.12.107-oracle", build)
        self.assertIn("--tarball PATH       explicit", build)
        self.assertIn('source_dir="$cache/linux-${LINUX_VERSION}-source"', build)
        self.assertIn('tar -xJf "$tarball" -C "$staging_dir"', build)
        self.assertIn('if [[ -e "$source_dir" ]]; then', build)
        self.assertIn('chmod -R u+w -- "$source_dir"', build)
        self.assertIn('chmod -R a-w "$source_dir"', build)
        self.assertIn('lock_file="$cache/.linux-${LINUX_VERSION}-oracle-build.lock"', build)
        self.assertIn('flock -x "$build_lock_fd"', build)
        self.assertLess(build.index('flock -x "$build_lock_fd"'), build.index('tar -xJf "$tarball" -C "$staging_dir"'))
        self.assertLess(build.index('flock -x "$build_lock_fd"'), build.index('make -C "$source_dir" O="$output" ARCH=x86_64 -j"$jobs" bzImage'))
        self.assertLess(build.index('flock -x "$build_lock_fd"'), build.index('chmod -R u+w -- "$source_dir"'))
        self.assertLess(build.index('chmod -R u+w -- "$source_dir"'), build.index('rm -rf "$source_dir"'))
        self.assertLess(build.index('rm -rf "$source_dir"'), build.index('chmod -R a-w "$source_dir"'))
        self.assertIn('CONFIG_HOTPLUG_PCI_ACPI=y', build)
        self.assertIn('CONFIG_EFI_STUB=y', build)
        self.assertIn('CONFIG_INOTIFY_USER=y', build)
        self.assertIn('# CONFIG_UNWINDER_ORC is not set', build)
        self.assertIn('CONFIG_UNWINDER_FRAME_POINTER=y', build)
        self.assertIn('kernelrelease=$(make -s', build)
        self.assertIn('[[ "$kernelrelease" == "$LINUX_VERSION" ]]', build)
        self.assertIn("/home/ava/.cache/thekernel-targets/linux-6.12.107-oracle", wrapper)
        self.assertIn("rootfs_transport=\"drive\"", runner)
        self.assertIn("rootfs_mode=\"snapshot\"", runner)
        self.assertIn("benchmark_checkpoints(args.fault)", runner)
        self.assertIn("--mode linux", wrapper)
        self.assertIn("--tarball LINUX-TARBALL", wrapper)
        self.assertIn("build+=(--tarball", wrapper)
        self.assertIn("--fault", wrapper)

    def test_shared_checkpoint_preserves_the_hotplug_fault_only_for_that_fault(self) -> None:
        standard = benchmark_checkpoints()
        hotplug = benchmark_checkpoints("input-hotplug")
        self.assertEqual(standard[0].input_after_marker, BENCHMARK_READY_MARKER)
        self.assertFalse(standard[0].pci_hotplug)
        self.assertEqual(len(hotplug), BENCHMARK_INPUT_SAMPLES + 2)
        self.assertEqual(hotplug[0].pci_hotplug[0].action, "del")
        self.assertEqual(hotplug[1].input_after_marker, BENCHMARK_INPUT_HOTPLUG_REMOVED_MARKER)
        self.assertEqual(hotplug[1].pci_hotplug[0].action, "add")
        self.assertEqual(hotplug[2].input_after_marker, BENCHMARK_INPUT_HOTPLUG_READY_MARKER)
        self.assertTrue(hotplug[2].input_events)
        with self.assertRaises(ValueError):
            benchmark_checkpoints("not-a-fault")


if __name__ == "__main__":
    unittest.main()
