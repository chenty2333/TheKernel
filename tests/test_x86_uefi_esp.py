from __future__ import annotations

import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
ESP_BUILDER = ROOT / "scripts" / "build-x86-uefi-esp.sh"
MULTIBOOT_GATE = ROOT / "scripts" / "check-x86-multiboot.sh"
GRUB_MKSTANDALONE = shutil.which("grub2-mkstandalone") or shutil.which(
    "grub-mkstandalone"
)
REQUIRED_TOOLS = ("parted", "mkfs.fat", "mcopy", "mmd", "mdir")


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
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            kernel = root / "kernel.elf"
            image = root / "nested" / "esp.img"
            kernel.write_bytes(b"test-multiboot-kernel")

            subprocess.run(
                [
                    str(ESP_BUILDER),
                    "--kernel",
                    str(kernel),
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
            grub_listing = subprocess.run(
                ["mdir", "-i", esp, "::/EFI/BOOT"],
                check=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
            ).stdout
            self.assertIn("BOOTX64", grub_listing)

    def test_builder_preloads_the_required_grub_modules(self) -> None:
        source = ESP_BUILDER.read_text(encoding="utf-8")
        self.assertIn(
            "part_gpt fat search search_fs_file multiboot multiboot2 serial terminal",
            source,
        )
        self.assertIn("config/x86_64/grub.cfg", source)

    def test_default_grub_entry_is_multiboot2_with_multiboot1_fallback(self) -> None:
        config = (ROOT / "config" / "x86_64" / "grub.cfg").read_text(encoding="utf-8")
        self.assertIn('menuentry "TheKernel (Multiboot2)"', config)
        self.assertIn("multiboot2 /TheKernel.elf", config)
        self.assertIn('menuentry "TheKernel (Multiboot1 fallback)"', config)
        self.assertIn("multiboot /TheKernel.elf", config)

    def test_multiboot_gate_selects_grub2_file_or_grub_file(self) -> None:
        source = MULTIBOOT_GATE.read_text(encoding="utf-8")
        self.assertIn("grub2-file", source)
        self.assertIn("grub-file", source)
        self.assertIn("--is-x86-multiboot", source)
        self.assertIn("--is-x86-multiboot2", source)


if __name__ == "__main__":
    unittest.main()
