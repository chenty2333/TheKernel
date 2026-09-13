"""Structural checks for the x86_64 machine profiles.

A machine profile is a set of *compile-time* claims about a machine.  Nothing
here can boot the machine it describes, so these tests only enforce the
properties that are decidable from the files themselves:

* every profile the product can select parses and generates a configuration;
* the per-CPU secret window still tiles the top of the kernel address space
  for the profile's own CPU admission limit;
* the fallback PCI ECAM window is inside an MMIO range the profile maps;
* the BAR allocation windows are ordered and non-overlapping.

The point is to fail loudly when a profile is edited into an inconsistent
state, not to substitute for a boot on the real machine.  See
`docs/design/n305-platform.md` for what only hardware can settle.
"""

from __future__ import annotations

import sys
import tomllib
import unittest
from pathlib import Path

WORKTREE = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(WORKTREE))

from tools import thekernel as product  # noqa: E402
from tools.product_state import MACHINE_PROFILES, REPO_ROOT, machine_profile  # noqa: E402

PAGE_SIZE = 0x1000
# The exclusive end of the kernel address space: kernel-aspace-base plus
# kernel-aspace-size.  Every profile must agree with it.
KERNEL_ASPACE_END = 0xFFFF_8000_0000_0000 + 0x0000_7FFF_FFFF_F000


def load_profile(name: str) -> dict:
    path = REPO_ROOT / machine_profile(name).config
    return tomllib.loads(path.read_text(encoding="utf-8"))


def contains(outer: tuple[int, int], inner: tuple[int, int]) -> bool:
    start, size = outer
    inner_start, inner_size = inner
    return start <= inner_start and inner_start + inner_size <= start + size


class MachineProfileTests(unittest.TestCase):
    def test_every_selectable_profile_exists_and_generates_a_config(self) -> None:
        self.assertEqual(set(MACHINE_PROFILES), {"q35-uefi", "n305"})
        for name, profile in MACHINE_PROFILES.items():
            self.assertEqual(profile.name, name)
            self.assertTrue((REPO_ROOT / profile.config).is_file(), profile.config)
            self.assertGreaterEqual(profile.max_cpus, 1)
        # The parser exposes exactly the profiles the artifact layout knows.
        parser = product.build_parser()
        for name in MACHINE_PROFILES:
            args = parser.parse_args(["build", "--platform", name])
            self.assertEqual(args.platform, name)

    def test_default_platform_stays_q35_uefi(self) -> None:
        # Every existing command line and artifact path must be unaffected by
        # the introduction of a second profile.
        args = product.build_parser().parse_args(["build"])
        self.assertEqual(args.platform, "q35-uefi")
        self.assertEqual(product.artifacts_for(args).machine.name, "q35-uefi")

    def test_smp_bound_follows_the_selected_profile(self) -> None:
        parser = product.build_parser()
        with self.assertRaises(product.ProductError):
            product.parse_variant(parser.parse_args(["build", "--smp", "8"]))
        product.parse_variant(
            parser.parse_args(["build", "--platform", "n305", "--smp", "8"])
        )
        with self.assertRaises(product.ProductError):
            product.parse_variant(
                parser.parse_args(["build", "--platform", "n305", "--smp", "9"])
            )

    def test_n305_admits_eight_cpus_and_documents_the_target(self) -> None:
        profile = load_profile("n305")
        # The i3-N305 is eight E-cores with no SMT.
        self.assertEqual(profile["plat"]["max-cpu-num"], 8)
        self.assertEqual(profile["arch"], "x86_64")
        self.assertEqual(profile["platform"], "x86-pc")
        # The header must say the profile is unverified on hardware.
        text = (REPO_ROOT / machine_profile("n305").config).read_text(encoding="utf-8")
        self.assertIn("NEVER BEEN BOOTED", text)

    def test_secret_window_tiles_the_top_of_the_kernel_address_space(self) -> None:
        for name in MACHINE_PROFILES:
            profile = load_profile(name)["plat"]
            base = int(profile["secret-window-base"], 0)
            size = profile["secret-window-size"]
            with self.subTest(profile=name):
                self.assertEqual(
                    base + size,
                    KERNEL_ASPACE_END,
                    "the secret window must end at the exclusive kernel-aspace end",
                )
                self.assertEqual(
                    size,
                    profile["max-cpu-num"] * PAGE_SIZE,
                    "the secret window must have exactly one page per admitted CPU",
                )

    def test_fallback_ecam_window_is_mapped_by_the_profile(self) -> None:
        for name in MACHINE_PROFILES:
            devices = load_profile(name)["devices"]
            base = devices["pci-ecam-base"]
            # Buses 0..=pci-bus-end, one MiB per bus.
            window = (base, (devices["pci-bus-end"] + 1) * (1 << 20))
            with self.subTest(profile=name):
                self.assertTrue(
                    any(contains(tuple(range_), window) for range_ in devices["mmio-ranges"]),
                    "the configured ECAM fallback must lie inside a mapped MMIO range",
                )

    def test_pci_allocation_ranges_exclude_bus_zero_and_do_not_overlap(self) -> None:
        for name in MACHINE_PROFILES:
            devices = load_profile(name)["devices"]
            ranges = [tuple(range_) for range_ in devices["pci-ranges"]]
            with self.subTest(profile=name):
                self.assertEqual(
                    ranges[0],
                    (0, 0),
                    "bus 0 is firmware-assigned and must not be reallocated",
                )
                windows = sorted(ranges[1:])
                for (start, size), (next_start, _) in zip(windows, windows[1:]):
                    self.assertLessEqual(start + size, next_start)
                for range_ in devices["mmio-ranges"]:
                    self.assertGreater(range_[1], 0)

    def test_phys_memory_size_is_not_a_ram_description(self) -> None:
        # No Rust code reads `plat.phys-memory-size`; installed RAM comes from
        # the Multiboot2 memory map at boot, so the key cannot describe the
        # machine.  It is retained only because the generated schema declares
        # it, and both profiles must keep saying so.
        for name in MACHINE_PROFILES:
            profile = load_profile(name)["plat"]
            with self.subTest(profile=name):
                self.assertEqual(profile["phys-memory-size"], 0x0800_0000)

        offenders = [
            path
            for root in ("crates", "kernel")
            for path in (REPO_ROOT / root).rglob("*.rs")
            if "PHYS_MEMORY" in path.read_text(encoding="utf-8", errors="replace")
        ]
        self.assertEqual(
            offenders,
            [],
            "plat.phys-memory-size is documented as inert but is now read",
        )


if __name__ == "__main__":
    unittest.main()
