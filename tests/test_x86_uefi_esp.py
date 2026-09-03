from __future__ import annotations

import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
ESP_BUILDER = ROOT / "scripts" / "build-x86-uefi-esp.sh"
GRUB_MKSTANDALONE = shutil.which("grub2-mkstandalone") or shutil.which(
    "grub-mkstandalone"
)
REQUIRED_TOOLS = ("parted", "mkfs.fat", "mcopy", "mmd", "mdir")
TEST_TMP_ROOT = Path.home() / ".cache" / "thekernel-test-tmp"


def _missing_tools() -> list[str]:
    missing = [tool for tool in REQUIRED_TOOLS if shutil.which(tool) is None]
    if GRUB_MKSTANDALONE is None:
        missing.append("grub2-mkstandalone/grub-mkstandalone")
    return missing


@unittest.skipUnless(
    not _missing_tools(),
    f"x86 UEFI image tools are not installed: {_missing_tools()}",
)
class X86UefiEspTests(unittest.TestCase):
    def test_builder_creates_gpt_fat32_esp_with_fallback_loader_and_kernel(self) -> None:
        TEST_TMP_ROOT.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=TEST_TMP_ROOT) as directory:
            root = Path(directory)
            kernel = root / "kernel.elf"
            rootfs = root / "rootfs.img"
            image = root / "nested" / "esp.img"
            kernel.write_bytes(b"test-multiboot-kernel")
            rootfs.write_bytes(b"test-rootfs-image")

            subprocess.run(
                [
                    str(ESP_BUILDER),
                    "--kernel",
                    str(kernel),
                    "--rootfs",
                    str(rootfs),
                    "--output",
                    str(image),
                ],
                cwd=ROOT,
                check=True,
                stdout=subprocess.PIPE,
                text=True,
            )

            table = subprocess.run(
                ["parted", "-s", str(image), "unit", "s", "print"],
                check=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
            ).stdout
            self.assertIn("Partition Table: gpt", table)
            self.assertIn("fat32", table)
            esp = f"{image}@@1M"
            root_listing = subprocess.run(
                ["mdir", "-i", esp, "::/"],
                check=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
            ).stdout
            self.assertIn("TheKernel", root_listing)
            self.assertIn("rootfs-x86", root_listing)
            grub_listing = subprocess.run(
                ["mdir", "-i", esp, "::/EFI/BOOT"],
                check=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
            ).stdout
            self.assertIn("BOOTX64", grub_listing)

    def test_builder_rejects_an_explicit_esp_too_small_for_the_rootfs(self) -> None:
        TEST_TMP_ROOT.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=TEST_TMP_ROOT) as directory:
            root = Path(directory)
            kernel = root / "kernel.elf"
            rootfs = root / "rootfs.img"
            kernel.write_bytes(b"kernel")
            with rootfs.open("wb") as stream:
                stream.truncate(256 * 1024 * 1024)
            result = subprocess.run(
                [
                    str(ESP_BUILDER),
                    "--kernel", str(kernel),
                    "--rootfs", str(rootfs),
                    "--output", str(root / "esp.img"),
                    "--size-mib", "128",
                ],
                cwd=ROOT,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("too small for the supplied payloads", result.stderr)

    def test_linux_mode_stages_only_vmlinuz_and_uses_a_drive_backed_root(self) -> None:
        source = ESP_BUILDER.read_text(encoding="utf-8")
        linux_config = (ROOT / "config/x86_64/grub-linux.cfg").read_text(encoding="utf-8")
        TEST_TMP_ROOT.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=TEST_TMP_ROOT) as directory:
            root = Path(directory)
            kernel = root / "vmlinuz"
            image = root / "linux.esp"
            kernel.write_bytes(b"test-linux-bzimage")
            subprocess.run(
                [
                    str(ESP_BUILDER),
                    "--mode", "linux",
                    "--kernel", str(kernel),
                    "--output", str(image),
                ],
                cwd=ROOT,
                check=True,
                stdout=subprocess.PIPE,
                text=True,
            )
            listing = subprocess.run(
                ["mdir", "-i", f"{image}@@1M", "::/"],
                check=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
            ).stdout
            self.assertIn("vmlinuz", listing)
            self.assertNotIn("rootfs-x86", listing)
        self.assertIn("--mode {multiboot|multiboot-drive|linux}", source)
        self.assertIn('mcopy -i "$esp" "$kernel" ::/vmlinuz', source)
        self.assertIn("--rootfs is only valid with --mode multiboot", source)
        self.assertIn("linux /vmlinuz root=/dev/vda console=ttyS0", linux_config)
        self.assertNotIn("rootfs-x86.img", linux_config)

    def test_multiboot_drive_mode_stages_no_rootfs_module(self) -> None:
        drive_config = (ROOT / "config/x86_64/grub-drive.cfg").read_text(encoding="utf-8")
        TEST_TMP_ROOT.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=TEST_TMP_ROOT) as directory:
            root = Path(directory)
            kernel = root / "kernel.elf"
            image = root / "drive.esp"
            kernel.write_bytes(b"test-multiboot-kernel")
            subprocess.run(
                [
                    str(ESP_BUILDER), "--mode", "multiboot-drive",
                    "--kernel", str(kernel), "--output", str(image),
                ],
                cwd=ROOT,
                check=True,
                stdout=subprocess.PIPE,
                text=True,
            )
            listing = subprocess.run(
                ["mdir", "-i", f"{image}@@1M", "::/"],
                check=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
            ).stdout
            self.assertIn("TheKernel", listing)
            self.assertNotIn("rootfs-x86", listing)
        self.assertIn("multiboot2 /TheKernel.elf", drive_config)
        self.assertNotIn("module2", drive_config)
        self.assertIn("insmod all_video", drive_config)


if __name__ == "__main__":
    unittest.main()
