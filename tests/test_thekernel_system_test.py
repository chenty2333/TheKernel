"""Focused product-system-test completion and KTAP gate tests."""

from __future__ import annotations

import importlib.util
import os
import sys
import tempfile
import tomllib
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]


def load_product():
    spec = importlib.util.spec_from_file_location(
        "thekernel_product_system_test", REPO_ROOT / "tools" / "thekernel.py"
    )
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


class SystemTestGateTests(unittest.TestCase):
    def test_product_state_defaults_to_the_host_cache(self) -> None:
        product = load_product()
        previous = os.environ.pop("THEKERNEL_STATE_DIR", None)
        try:
            self.assertEqual(
                product.state_root(), Path.home() / ".cache" / "thekernel-targets"
            )
        finally:
            if previous is not None:
                os.environ["THEKERNEL_STATE_DIR"] = previous

    def test_product_feature_aggregation_is_the_standard_build_baseline(self) -> None:
        product = load_product()
        root_manifest = tomllib.loads((REPO_ROOT / "Cargo.toml").read_text(encoding="utf-8"))

        self.assertEqual(
            root_manifest["features"][product.PRODUCT_FEATURE],
        ["qemu", "smp", "hwp-uclamp", "pmu", "perf-sampling"],
        )
        args = product.build_parser().parse_args(["build"])
        self.assertEqual(
            product.kernel_features(product.Artifacts(Path("state"), product.parse_variant(args))),
            "x86-product",
        )

    def test_product_feature_combines_variant_features_without_repeating_baseline(self) -> None:
        product = load_product()
        args = product.build_parser().parse_args(
            ["build", "--smp", "1", "--asid-fast-switch", "--profile", "shell"]
        )

        self.assertEqual(
            product.kernel_features(product.Artifacts(Path("state"), product.parse_variant(args), args.profile)),
            "x86-product boot-shell asid-fast-switch",
        )

    def test_product_defaults_and_compile_time_network_match_q35_gate(self) -> None:
        product = load_product()
        args = product.build_parser().parse_args(["system-test"])
        self.assertEqual((args.smp, args.memory), (4, "1G"))
        with tempfile.TemporaryDirectory() as directory:
            artifacts = product.Artifacts(
                Path(directory), product.parse_variant(args), "system"
            )
            environment = product.command_env(artifacts)
        self.assertEqual(environment["AX_IP"], "10.0.2.15")
        self.assertEqual(environment["AX_GW"], "10.0.2.2")
        self.assertEqual(environment["SMOLTCP_IFACE_MAX_ADDR_COUNT"], "4")
        self.assertIn("--cfg aes_force_soft", environment["RUSTFLAGS"])
        platform = tomllib.loads(
            (REPO_ROOT / "config/x86_64/q35-uefi.toml").read_text(encoding="utf-8")
        )
        self.assertEqual(platform["devices"]["pci-ecam-base"], 0xE000_0000)
        self.assertIn(
            [0xE000_0000, 0x1000_0000], platform["devices"]["mmio-ranges"]
        )

    def test_q35_accepts_high_ram_without_expanding_the_low_ram_window(self) -> None:
        product = load_product()
        args = product.build_parser().parse_args(["build", "--memory", "4G"])
        variant = product.parse_variant(args)
        self.assertEqual(variant.memory_bytes, 4 * product.GIB)
        self.assertEqual(product.Q35_PCI_HOLE_LOW_RAM_LIMIT, 2 * product.GIB)
        self.assertEqual(product.Q35_HIGH_MEMORY_BASE, 4 * product.GIB)

    def test_ktap_skip_is_rejected_by_default_gate(self) -> None:
        product = load_product()
        with tempfile.TemporaryDirectory() as directory:
            log = Path(directory) / "console.log"
            log.write_text(
                "KTAP version 1\nok 1 - supported\nok 2 - unavailable # SKIP guest ABI\n",
                encoding="utf-8",
            )
            with self.assertRaisesRegex(product.ProductError, "KTAP SKIP"):
                product.reject_ktap_skips_in_log(log)

    def test_ktap_without_skip_remains_acceptable(self) -> None:
        product = load_product()
        with tempfile.TemporaryDirectory() as directory:
            log = Path(directory) / "console.log"
            log.write_text("KTAP version 1\nok 1 - supported\n", encoding="utf-8")
            product.reject_ktap_skips_in_log(log)

    def test_graphics_smoke_flavors_come_from_the_dual_parsed_manifest(self) -> None:
        product = load_product()
        flavors = product.graphics_flavors()
        self.assertEqual(flavors, (
            "headless-abi-smoke",
            "q35-graphics-seatd",
            "q35-software-desktop",
            "q35-graphics-benchmark",
            "q35-venus-desktop",
            "q35-graphics-logind",
        ))
        self.assertEqual(product.graphics_smoke_flavors(), (
            "headless-abi-smoke",
            "q35-graphics-seatd",
            "q35-graphics-logind",
        ))
        args = product.build_parser().parse_args([
            "graphics-smoke", "--no-build", "--rootfs", "rootfs.ext2",
            "--screenshot", "graphics.ppm", "--flavor", "q35-graphics-seatd",
        ])
        self.assertEqual(args.flavor, "q35-graphics-seatd")
        with self.assertRaises(SystemExit):
            product.build_parser().parse_args([
                "graphics-smoke", "--no-build", "--rootfs", "rootfs.ext2",
                "--screenshot", "graphics.ppm", "--flavor", "q35-graphics-benchmark",
            ])

    def test_system_test_configures_marker_gated_shutdown_not_runner_stop(self) -> None:
        product = load_product()
        args = product.build_parser().parse_args(["system-test"])
        calls: dict[str, object] = {}

        def fake_build(_artifacts):
            return None

        def fake_run_product(_artifacts, spec):
            calls["spec"] = spec
            return 0

        original_build_kernel = product.build_kernel
        original_build_rootfs = product.build_rootfs
        original_run_product = product.run_product
        try:
            product.build_kernel = fake_build
            product.build_rootfs = fake_build
            product.run_product = fake_run_product
            self.assertEqual(product.system_test_cmd(args), 0)
        finally:
            product.build_kernel = original_build_kernel
            product.build_rootfs = original_build_rootfs
            product.run_product = original_run_product

        spec = calls["spec"]
        self.assertTrue(spec.shutdown_after_marker)
        self.assertTrue(spec.reject_ktap_skips)
        self.assertEqual(spec.rootfs_transport, "module")
        self.assertIsNone(spec.stop_after_marker)

    def test_system_test_run_cpus_selects_the_qemu_cpu_count(self) -> None:
        product = load_product()
        args = product.build_parser().parse_args(
            ["system-test", "--smp", "4", "--run-cpus", "1", "--no-build"]
        )
        observed: dict[str, object] = {}

        def fake_run_product(artifacts, spec):
            observed["variant_name"] = artifacts.variant.name
            observed["run_cpus"] = spec.run_cpus
            return 0

        original_run_product = product.run_product
        try:
            product.run_product = fake_run_product
            self.assertEqual(product.system_test_cmd(args), 0)
        finally:
            product.run_product = original_run_product

        self.assertEqual(observed["variant_name"], "mem1g")
        self.assertEqual(observed["run_cpus"], 1)

    def test_run_product_uses_run_cpu_override_for_qemu_command(self) -> None:
        product = load_product()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            artifacts = product.Artifacts(
                root / "state", product.Variant(memory="1G"), "system"
            )
            for path in (artifacts.kernel, artifacts.esp, artifacts.rootfs):
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(b"artifact")
            observed = {}

            def fake_run(config):
                observed["cpus"] = config.cpus
                return type("Result", (), {
                    "returncode": 0,
                    "error_message": None,
                    "log_path": config.log_path,
                    "guest_clean_shutdown": True,
                    "intentionally_stopped": False,
                })()

            original_run = product.run
            try:
                product.run = fake_run
                self.assertEqual(product.run_product(
                    artifacts,
                    product.RunSpec(
                        accel="tcg",
                        timeout=30,
                        workdir=root / "run",
                        interactive=False,
                        input_after_marker=None,
                        stop_after_marker=None,
                        commands=None,
                        extra_block=None,
                        run_cpus=1,
                    ),
                ), 0)
            finally:
                product.run = original_run

        self.assertEqual(observed["cpus"], 1)

    def test_run_product_drive_uses_the_separate_drive_esp(self) -> None:
        product = load_product()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            artifacts = product.Artifacts(
                root / "state", product.Variant(memory="1G"), "system"
            )
            for path in (artifacts.kernel, artifacts.drive_esp, artifacts.rootfs):
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(b"artifact")
            observed = {}

            def fake_run(config):
                observed["config"] = config
                return type("Result", (), {
                    "returncode": 0,
                    "error_message": None,
                    "log_path": config.log_path,
                    "guest_clean_shutdown": True,
                    "intentionally_stopped": False,
                })()

            original_run = product.run
            try:
                product.run = fake_run
                self.assertEqual(product.run_product(
                    artifacts,
                    product.RunSpec(
                        accel="tcg",
                        timeout=30,
                        workdir=root / "run",
                        interactive=False,
                        input_after_marker=None,
                        stop_after_marker=None,
                        commands=None,
                        extra_block=None,
                        rootfs_transport="drive",
                        run_cpus=4,
                    ),
                ), 0)
            finally:
                product.run = original_run

        self.assertEqual(observed["config"].esp, artifacts.drive_esp)
        self.assertEqual(observed["config"].rootfs_transport, "drive")

    def test_run_cpus_rejects_values_outside_the_smp_bound(self) -> None:
        product = load_product()
        with self.assertRaisesRegex(product.ProductError, "--run-cpus"):
            product.resolve_run_cpus(4, 0)
        with self.assertRaisesRegex(product.ProductError, "--run-cpus"):
            product.resolve_run_cpus(4, 5)

    def test_explicit_new_workdir_exists_before_shutdown_commands_are_written(self) -> None:
        product = load_product()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            artifacts = product.Artifacts(
                root / "state", product.Variant(memory="1G"), "system"
            )
            for path in (artifacts.kernel, artifacts.esp, artifacts.rootfs):
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(b"artifact")
            workdir = root / "new" / "system-test"
            observed = {}

            def fake_run(config):
                observed["config"] = config
                self.assertTrue(config.workdir.is_dir())
                self.assertEqual(
                    config.input_path.read_text(encoding="utf-8"),
                    product.SYSTEM_TEST_SHUTDOWN_COMMANDS,
                )
                return type("Result", (), {
                    "returncode": 0,
                    "error_message": None,
                    "log_path": config.log_path,
                    "guest_clean_shutdown": True,
                    "intentionally_stopped": False,
                })()

            original_run = product.run
            try:
                product.run = fake_run
                self.assertEqual(product.run_product(
                    artifacts,
                    product.RunSpec(
                        accel="tcg",
                        timeout=30,
                        workdir=workdir,
                        interactive=False,
                        input_after_marker=None,
                        stop_after_marker=None,
                        commands=None,
                        extra_block=None,
                        shutdown_after_marker=True,
                        run_cpus=4,
                    ),
                ), 0)
            finally:
                product.run = original_run

            self.assertEqual(observed["config"].workdir, workdir.resolve())


if __name__ == "__main__":
    unittest.main()
