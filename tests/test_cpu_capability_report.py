"""Regression tests for the CPU capability transcript contract."""

from __future__ import annotations

import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from tests.support import load_script_module, test_tmpdir


product = load_script_module("thekernel_cpu_capability", "tools/thekernel.py")


VISIBLE = {
    "hypervisor": "1",
    "apic": "1",
    "pcid": "1",
    "invpcid": "1",
    "xsave": "1",
    "pku": "1",
    "cet_ss": "1",
}
ENABLED = {
    "apic": "1",
    "apic_software": "1",
    "x2apic": "0",
    "pcid": "0",
    "osxsave": "0",
    "xcr0": "0x0",
    "pke": "0",
    "cet_cr4": "0",
    "syscall": "1",
}


def report(cpus: int = 1, *, visible: dict[str, str] | None = None,
           enabled: dict[str, str] | None = None) -> str:
    visible_fields = visible or VISIBLE
    enabled_fields = enabled or ENABLED
    lines = []
    for cpu in range(cpus):
        lines.append(
            "THEKERNEL_CPU_VISIBLE "
            f"cpu={cpu} "
            + " ".join(f"{key}={value}" for key, value in visible_fields.items())
        )
        lines.append(
            "THEKERNEL_CPU_ENABLED "
            f"cpu={cpu} "
            + " ".join(f"{key}={value}" for key, value in enabled_fields.items())
        )
    return "\n".join(lines) + "\n"


class CpuCapabilityReportTests(unittest.TestCase):
    def test_cpu_suite_reads_capabilities_only_from_diagnostic_channel(self) -> None:
        with test_tmpdir() as directory:
            logs = {}
            for cpus in (1, 4):
                run = Path(directory) / str(cpus)
                run.mkdir()
                console = run / "console.log"
                console.write_text("KTAP version 1\n1..1\nok 1 - CPU\n# THEKERNEL_CPU_TEST_COMPLETE\n")
                (run / "kernel.log").write_text(report(cpus))
                logs[cpus] = console
            args = SimpleNamespace(accel="kvm", smp=4)
            with patch.object(product, "run_checked"), patch.object(
                product, "guest_tool_run", side_effect=lambda _a, _c, _m, cpus: (0, logs[cpus])
            ):
                self.assertEqual(product.cpu_test_cmd(args), 0)
                # A userspace transcript must not replace absent boot diagnostics.
                logs[1].write_text(logs[1].read_text() + report(1))
                logs[1].with_name("kernel.log").write_text("")
                with self.assertRaisesRegex(product.ProductError, "missing.*VISIBLE"):
                    product.cpu_test_cmd(args)

    def test_visible_features_may_remain_disabled_by_the_kernel(self) -> None:
        # Hardware visibility is a prerequisite for enablement, not a promise
        # that every optional privileged feature is turned on.
        product._validate_cpu_capability_reports(report(cpus=4), 4)

    def test_enabled_state_requires_its_visible_prerequisites(self) -> None:
        cases = (
            ({**ENABLED, "pcid": "1"}, {**VISIBLE, "pcid": "0"}, "pcid"),
            ({**ENABLED, "osxsave": "1", "xcr0": "0x3"},
             {**VISIBLE, "xsave": "0"}, "osxsave"),
            ({**ENABLED, "osxsave": "0", "xcr0": "0x3"}, VISIBLE, "xcr0"),
            ({**ENABLED, "osxsave": "1", "xcr0": "0x0"}, VISIBLE, "x87/SSE"),
            ({**ENABLED, "pke": "1", "osxsave": "1", "xcr0": "0x203"},
             {**VISIBLE, "pku": "0"}, "pku"),
            ({**ENABLED, "pke": "1", "osxsave": "0", "xcr0": "0x0"},
             VISIBLE, "OSXSAVE"),
            ({**ENABLED, "pke": "1", "osxsave": "1", "xcr0": "0x3"},
             VISIBLE, "PKRU"),
            ({**ENABLED, "cet_cr4": "1"}, {**VISIBLE, "cet_ss": "0"}, "cet_ss"),
            ({**ENABLED, "apic": "0"}, VISIBLE, "apic"),
        )
        for enabled, visible, expected in cases:
            with self.subTest(expected=expected):
                with self.assertRaisesRegex(product.ProductError, expected):
                    product._validate_cpu_capability_reports(
                        report(visible=visible, enabled=enabled), 1
                    )

    def test_reports_require_complete_unique_machine_state(self) -> None:
        missing = {key: value for key, value in VISIBLE.items() if key != "pcid"}
        with self.assertRaisesRegex(product.ProductError, "missing.*pcid"):
            product._validate_cpu_capability_reports(report(visible=missing), 1)

        duplicate = report() + report()
        with self.assertRaisesRegex(product.ProductError, "duplicate"):
            product._validate_cpu_capability_reports(duplicate, 1)

        malformed = report(enabled={**ENABLED, "xcr0": "not-a-number"})
        with self.assertRaisesRegex(product.ProductError, "invalid xcr0"):
            product._validate_cpu_capability_reports(malformed, 1)


if __name__ == "__main__":
    unittest.main()
