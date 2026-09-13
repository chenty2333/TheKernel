#!/usr/bin/env python3
"""Panther Lake hardware gate: the serial-observed profile of ``dut_gate``.

The gate deliberately knows nothing about a particular lab controller.  A
self-hosted runner supplies three local commands through its protected runner
configuration:

* ``THEKERNEL_DUT_POWER_CYCLE_CMD`` must fully remove and restore DUT power;
* ``THEKERNEL_DUT_BOOT_ONCE_CMD`` must select the supplied ESP for one boot;
* ``THEKERNEL_DUT_SERIAL_CAPTURE_CMD`` must capture that boot's serial output
  and return only after a *normal* guest shutdown.

Each command receives ``THEKERNEL_DUT_*`` paths in its environment.  The
serial command must write the serial log and write ``clean`` to
``THEKERNEL_DUT_SHUTDOWN_STATUS`` after it has independently observed the
normal shutdown.  This intentionally fails rather than treating a serial
timeout, a forced power-off, or absent lab integration as a passing test.

The implementation lives in ``scripts/ci/dut_gate.py``, which also carries the
``network`` and ``screen`` observation channels for a DUT whose only output is
a screen.  This module is the compatibility entry point: the runner command
line and the environment contract above are unchanged.
"""

from __future__ import annotations

import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from scripts.ci.dut_gate import (
    GateError,
    REQUIRED_ARTIFACTS,
    main as _main,
    parse_screen_verdict,
    run_gate as _run_gate,
    validate_artifacts,
    validate_clean_shutdown,
    validate_ktap,
)

__all__ = [
    "GateError",
    "REQUIRED_ARTIFACTS",
    "main",
    "parse_screen_verdict",
    "run_gate",
    "validate_artifacts",
    "validate_clean_shutdown",
    "validate_ktap",
]

DUT = "panther-lake"


def run_gate(artifact_dir: Path, state_dir: Path, *, runs: int) -> None:
    """Run the Panther Lake certification gate over exactly three cold boots."""

    _run_gate(artifact_dir, state_dir, dut=DUT, runs=runs)


def main(argv: list[str] | None = None) -> int:
    return _main(argv, default_dut=DUT)


if __name__ == "__main__":
    raise SystemExit(main())
