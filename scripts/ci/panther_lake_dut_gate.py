#!/usr/bin/env python3
"""Fail-closed Panther Lake hardware gate.

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
"""

from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys
from pathlib import Path


class GateError(RuntimeError):
    """A missing DUT contract or failing hardware test."""


REQUIRED_ARTIFACTS = ("kernel-x86_64", "kernel-x86_64.esp", "rootfs-x86.img")
TEST_LINE = re.compile(r"^(ok|not ok)\s+([1-9][0-9]*)\b", re.IGNORECASE)
PLAN_LINE = re.compile(r"^1\.\.([1-9][0-9]*)\s*$")
SKIP_LINE = re.compile(r"^ok\s+[1-9][0-9]*\b.*\s#\s*SKIP(?:\s|$)", re.IGNORECASE)


def absolute_non_tmpfs(path: Path, *, name: str) -> Path:
    resolved = path.expanduser().resolve()
    if not resolved.is_absolute():
        raise GateError(f"{name} must be an absolute path")
    if resolved == Path("/tmp") or Path("/tmp") in resolved.parents:
        raise GateError(f"{name} must not be under /tmp")
    if resolved == Path("/dev/shm") or Path("/dev/shm") in resolved.parents:
        raise GateError(f"{name} must not be under /dev/shm")
    return resolved


def validate_artifacts(artifact_dir: Path) -> dict[str, Path]:
    artifact_dir = artifact_dir.resolve()
    if not artifact_dir.is_dir():
        raise GateError(f"product artifact directory does not exist: {artifact_dir}")
    artifacts: dict[str, Path] = {}
    for filename in REQUIRED_ARTIFACTS:
        path = (artifact_dir / filename).resolve()
        if path.parent != artifact_dir or not path.is_file() or path.is_symlink():
            raise GateError(f"required product artifact is missing or unsafe: {filename}")
        if path.stat().st_size == 0:
            raise GateError(f"required product artifact is empty: {filename}")
        artifacts[filename] = path
    return artifacts


def required_command(name: str) -> str:
    command = os.environ.get(name, "").strip()
    if not command:
        raise GateError(f"required protected runner hook is unset: {name}")
    return command


def validate_ktap(log_path: Path) -> None:
    try:
        lines = log_path.read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError as error:
        raise GateError(f"cannot read DUT serial log {log_path}: {error}") from error
    if not any(line.strip() == "KTAP version 1" for line in lines):
        raise GateError("serial output does not contain KTAP version 1")
    if any(SKIP_LINE.match(line) for line in lines):
        raise GateError("serial output contains a KTAP SKIP result")
    if any(line.lower().startswith("not ok ") for line in lines):
        raise GateError("serial output contains a failing KTAP result")
    if any("KTAP suite failed" in line for line in lines):
        raise GateError("serial output reports a KTAP suite failure")
    plans = [int(match.group(1)) for line in lines if (match := PLAN_LINE.match(line.strip()))]
    if len(plans) != 1:
        raise GateError("serial output must contain exactly one KTAP plan")
    records = [int(match.group(2)) for line in lines if (match := TEST_LINE.match(line.strip()))]
    expected = plans[0]
    if sorted(records) != list(range(1, expected + 1)):
        raise GateError("KTAP records do not exactly satisfy the declared plan")
    if "# THEKERNEL_SYSTEM_TEST_COMPLETE" not in lines:
        raise GateError("serial output lacks the system-test completion marker")


def validate_clean_shutdown(status_path: Path) -> None:
    try:
        status = status_path.read_text(encoding="utf-8").strip()
    except OSError as error:
        raise GateError(f"DUT serial hook did not write shutdown status: {error}") from error
    if status != "clean":
        raise GateError("DUT did not report a normal guest shutdown")


def run_hook(command: str, *, environment: dict[str, str], description: str) -> None:
    completed = subprocess.run(
        command,
        shell=True,
        executable="/bin/bash",
        env=environment,
        check=False,
    )
    if completed.returncode:
        raise GateError(f"{description} hook failed with exit status {completed.returncode}")


def run_gate(artifact_dir: Path, state_dir: Path, *, runs: int) -> None:
    if runs != 3:
        raise GateError("Panther Lake certification requires exactly three cold boots")
    artifacts = validate_artifacts(artifact_dir)
    state_dir.mkdir(parents=True, exist_ok=True)
    if not state_dir.is_dir():
        raise GateError(f"cannot create DUT state directory: {state_dir}")
    power_cycle = required_command("THEKERNEL_DUT_POWER_CYCLE_CMD")
    boot_once = required_command("THEKERNEL_DUT_BOOT_ONCE_CMD")
    serial_capture = required_command("THEKERNEL_DUT_SERIAL_CAPTURE_CMD")

    for number in range(1, runs + 1):
        serial_log = state_dir / f"cold-boot-{number}.serial.log"
        shutdown_status = state_dir / f"cold-boot-{number}.shutdown"
        for path in (serial_log, shutdown_status):
            path.unlink(missing_ok=True)
        environment = {
            **os.environ,
            "THEKERNEL_DUT_ARTIFACT_DIR": str(artifact_dir),
            "THEKERNEL_DUT_KERNEL": str(artifacts["kernel-x86_64"]),
            "THEKERNEL_DUT_ESP": str(artifacts["kernel-x86_64.esp"]),
            "THEKERNEL_DUT_ROOTFS": str(artifacts["rootfs-x86.img"]),
            "THEKERNEL_DUT_RUN": str(number),
            "THEKERNEL_DUT_SERIAL_LOG": str(serial_log),
            "THEKERNEL_DUT_SHUTDOWN_STATUS": str(shutdown_status),
        }
        run_hook(power_cycle, environment=environment, description=f"cold boot {number} power-cycle")
        run_hook(boot_once, environment=environment, description=f"cold boot {number} one-shot boot")
        run_hook(serial_capture, environment=environment, description=f"cold boot {number} serial capture")
        validate_ktap(serial_log)
        validate_clean_shutdown(shutdown_status)


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--artifact-dir", required=True, type=Path)
    parser.add_argument("--state-dir", required=True, type=Path)
    parser.add_argument("--runs", type=int, default=3)
    return parser


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        artifact_dir = absolute_non_tmpfs(args.artifact_dir, name="--artifact-dir")
        state_dir = absolute_non_tmpfs(args.state_dir, name="--state-dir")
        run_gate(artifact_dir, state_dir, runs=args.runs)
    except GateError as error:
        print(f"panther-lake-dut-gate: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
