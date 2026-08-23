"""Focused product-system-test completion and KTAP gate tests."""

from __future__ import annotations

import importlib.util
import sys
import tempfile
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


if __name__ == "__main__":
    unittest.main()
