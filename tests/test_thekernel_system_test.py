"""Focused product-system-test completion and KTAP gate tests."""

from __future__ import annotations

import importlib.util
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
    def test_product_defaults_and_compile_time_network_match_q35_gate(self) -> None:
        product = load_product()
        args = product.build_parser().parse_args(["system-test"])
        self.assertEqual((args.machine, args.firmware, args.smp, args.memory), (
            "q35", "uefi", 4, "1G"
        ))
        with tempfile.TemporaryDirectory() as directory:
            artifacts = product.Artifacts(
                Path(directory), product.parse_variant(args), "system"
            )
            environment = product.command_env(artifacts)
        self.assertEqual(environment["AX_IP"], "10.0.2.15")
        self.assertEqual(environment["AX_GW"], "10.0.2.2")
        self.assertEqual(environment["SMOLTCP_IFACE_MAX_ADDR_COUNT"], "4")
        platform = tomllib.loads(
            (REPO_ROOT / "config/x86_64/q35-uefi.toml").read_text(encoding="utf-8")
        )
        self.assertEqual(platform["devices"]["pci-ecam-base"], 0xB000_0000)
        self.assertIn(
            [0xB000_0000, 0x1000_0000], platform["devices"]["mmio-ranges"]
        )

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

    def test_system_test_configures_marker_gated_shutdown_not_runner_stop(self) -> None:
        product = load_product()
        args = product.build_parser().parse_args(["system-test"])
        calls: dict[str, object] = {}

        def fake_build(_artifacts):
            return None

        def fake_run_product(*_args, **kwargs):
            calls.update(kwargs)
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

        self.assertTrue(calls["shutdown_after_marker"])
        self.assertTrue(calls["reject_ktap_skips"])
        self.assertIsNone(calls["stop_after_marker"])

    def test_explicit_new_workdir_exists_before_shutdown_commands_are_written(self) -> None:
        product = load_product()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            artifacts = product.Artifacts(
                root / "state", product.Variant(cpus=4, memory="1G"), "system"
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
                    "log_path": config.log_path,
                    "guest_clean_shutdown": True,
                    "intentionally_stopped": False,
                })()

            original_run = product.run
            try:
                product.run = fake_run
                self.assertEqual(product.run_product(
                    artifacts,
                    accel="tcg",
                    timeout=30,
                    workdir=workdir,
                    interactive=False,
                    input_after_marker=None,
                    stop_after_marker=None,
                    commands=None,
                    extra_block=None,
                    shutdown_after_marker=True,
                ), 0)
            finally:
                product.run = original_run

            self.assertEqual(observed["config"].workdir, workdir.resolve())


if __name__ == "__main__":
    unittest.main()
