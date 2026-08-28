#!/usr/bin/env python3
"""Create the evidence manifest for the q35-preview-v0 product gate.

This module deliberately owns gate policy rather than teaching the product
runner about CI aggregation.  A manifest is written for every attempted run,
including a failed preflight, so a failed gate has inspectable evidence too.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path
from typing import Any, Callable, Sequence

from scripts.ci import source_combination

from .evidence import EvidenceError, file_evidence, validate_file_evidence
from .receipt import atomic_write_receipt


SCHEMA_VERSION = 3
GATE_NAME = "q35-preview-v0"
KTAP_RESULT = re.compile(r"^(ok|not ok)\s+([1-9][0-9]*)\b(.*)$", re.MULTILINE)
KTAP_PLAN = re.compile(r"^1\.\.([0-9]+)(?:\s*#.*)?$", re.MULTILINE)
KTAP_SKIP = re.compile(r"#\s*SKIP(?:\s|$)", re.IGNORECASE)
PANIC = re.compile(r"\b(?:kernel[ -])?panic\b", re.IGNORECASE)
RUNNER_STOPPED = re.compile(r"(?:stopped after marker|runner[- ]initiated stop)", re.IGNORECASE)


class GateError(ValueError):
    """Raised when gate evidence cannot support a pass verdict."""


@dataclass(frozen=True)
class CompletedCommand:
    returncode: int
    stdout: bytes
    stderr: bytes


CommandRunner = Callable[[Sequence[str], Path], CompletedCommand]


def _timestamp() -> str:
    return datetime.now(UTC).isoformat(timespec="microseconds").replace("+00:00", "Z")


def _git(source: Path, *args: str) -> str:
    completed = subprocess.run(
        ("git", "-C", str(source), *args), capture_output=True, text=True, check=False
    )
    if completed.returncode:
        detail = completed.stderr.strip() or "unknown git failure"
        raise GateError(f"cannot inspect {source}: {detail}")
    return completed.stdout.strip()


def checkout_identity(source: Path, *, expected_commit: str | None) -> dict[str, Any]:
    """Capture a checkout's commit, tree and cleanliness without changing it."""

    source = source.resolve()
    root = Path(_git(source, "rev-parse", "--show-toplevel")).resolve()
    if root != source:
        raise GateError(f"checkout root differs from expected path: {source} != {root}")
    commit = _git(source, "rev-parse", "HEAD^{commit}")
    tree = _git(source, "rev-parse", "HEAD^{tree}")
    dirty = bool(_git(source, "status", "--porcelain=v1", "--untracked-files=all"))
    return {
        "repository_root": str(root),
        "commit": commit,
        "tree": tree,
        "worktree_dirty": dirty,
        "match_declared": expected_commit is None or commit == expected_commit,
    }


def preflight(root: Path) -> dict[str, Any]:
    """Verify TheKernel and both declared sibling checkouts precisely."""

    root = root.resolve()
    config = root / "config" / "source-combination.toml"
    try:
        declared = source_combination.load(config)
    except source_combination.SourceCombinationError as error:
        raise GateError(f"cannot load source combination: {error}") from error
    sources: dict[str, dict[str, Any]] = {
        "thekernel": checkout_identity(root, expected_commit=None)
    }
    for name, source in sorted(declared.items()):
        sources[name] = checkout_identity(root.parent / source.path, expected_commit=source.ref)
    failures = [
        name
        for name, identity in sources.items()
        if identity["worktree_dirty"] or not identity["match_declared"]
    ]
    return {
        "config": file_evidence(config),
        "combination_id": source_combination.combination_id(declared, sources["thekernel"]["commit"]),
        "sources": sources,
        "valid": not failures,
        "failures": failures,
    }


def validate_guest_log(log: Path, *, system_test_returncode: int = 0) -> dict[str, Any]:
    """Require a complete KTAP plan and an actual clean runner exit.

    Guest diagnostics can legitimately mention timeout-related test cases.  The
    runner's return code and explicit runner-stop evidence, rather than such
    free-form text, establish whether QEMU shut down cleanly.
    """

    text = log.read_text(encoding="utf-8", errors="replace")
    results = list(KTAP_RESULT.finditer(text))
    plans = list(KTAP_PLAN.finditer(text))
    failures: list[str] = []
    if "KTAP version 1" not in text:
        failures.append("missing KTAP header")
    planned = int(plans[0].group(1)) if len(plans) == 1 else None
    if len(plans) != 1:
        failures.append("KTAP requires exactly one plan")
    elif planned == 0:
        failures.append("KTAP plan is empty")
    not_ok = sum(match.group(1) == "not ok" for match in results)
    skips = sum(bool(KTAP_SKIP.search(match.group(0))) for match in results)
    numbers = [int(match.group(2)) for match in results]
    unique_numbers = len(numbers) == len(set(numbers))
    expected_numbers = list(range(1, planned + 1)) if planned is not None else []
    plan_matches_results = planned is not None and sorted(numbers) == expected_numbers
    if not_ok:
        failures.append(f"KTAP failures={not_ok}")
    if skips:
        failures.append(f"KTAP skips={skips}")
    if not unique_numbers:
        failures.append("KTAP result numbers are not unique")
    if not plan_matches_results:
        failures.append("KTAP results do not match plan")
    if PANIC.search(text):
        failures.append("guest panic")
    runner_terminated = bool(RUNNER_STOPPED.search(text))
    if runner_terminated:
        failures.append("runner terminated guest")
    guest_clean_shutdown = system_test_returncode == 0 and not runner_terminated
    if not guest_clean_shutdown:
        failures.append(f"system test did not cleanly shut down (returncode={system_test_returncode})")
    ktap_complete = (
        "KTAP version 1" in text
        and planned is not None
        and planned > 0
        and plan_matches_results
        and unique_numbers
        and not_ok == 0
        and skips == 0
    )
    completion_marker_seen = "# THEKERNEL_SYSTEM_TEST_COMPLETE" in text
    if not completion_marker_seen:
        failures.append("missing post-suite completion marker")
    return {
        "log": file_evidence(log),
        "system_test_returncode": system_test_returncode,
        "guest_clean_shutdown": guest_clean_shutdown,
        "runner_terminated": runner_terminated,
        "ktap_plan": planned,
        "ktap_result_numbers": numbers,
        "ktap_complete": ktap_complete,
        "ktap_results": len(results),
        "ktap_failures": not_ok,
        "ktap_skips": skips,
        "completion_marker_seen": completion_marker_seen,
        # The marker only causes the product runner to inject shutdown input;
        # it is necessary evidence of post-suite shutdown, never a verdict.
        "valid": ktap_complete and completion_marker_seen and guest_clean_shutdown and not failures,
        "failures": failures,
    }


def _default_runner(command: Sequence[str], _cwd: Path) -> CompletedCommand:
    completed = subprocess.run(command, cwd=_cwd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)
    return CompletedCommand(completed.returncode, completed.stdout, completed.stderr)


def _command_record(
    name: str, command: Sequence[str], cwd: Path, logs: Path, runner: CommandRunner
) -> dict[str, Any]:
    started = _timestamp()
    try:
        completed = runner(command, cwd)
    except Exception as error:  # evidence must survive an orchestration failure
        completed = CompletedCommand(125, b"", f"gate command launcher failed: {error}\n".encode())
    ended = _timestamp()
    stdout = logs / f"{name}.stdout.log"
    stderr = logs / f"{name}.stderr.log"
    stdout.write_bytes(completed.stdout)
    stderr.write_bytes(completed.stderr)
    return {
        "name": name,
        "command": list(command),
        "started_at": started,
        "ended_at": ended,
        "returncode": completed.returncode,
        "stdout": file_evidence(stdout),
        "stderr": file_evidence(stderr),
        "status": "passed" if completed.returncode == 0 else "failed",
    }


def _final_artifact_evidence(paths: Sequence[Path]) -> dict[str, dict[str, str | int]]:
    """Capture the exact image triplet used by the completed system test."""

    if len(paths) != 3:
        raise GateError("system-test final artifacts must be kernel, ESP, and rootfs")
    evidence: dict[str, dict[str, str | int]] = {}
    for role, path in zip(("kernel", "esp", "rootfs"), paths, strict=True):
        path = path.resolve()
        if not path.is_file():
            raise GateError(f"required final {role} artifact is missing: {path}")
        evidence[role] = file_evidence(path)
    return evidence


def _launch_receipt_inputs(
    receipt_path: Path,
    *,
    workdir: Path,
    log_path: Path,
    artifacts: Sequence[Path],
    source_identity: dict[str, Any],
) -> dict[str, Any]:
    """Validate QEMU's launch-time binding to the system-test image triplet."""

    try:
        payload = json.loads(receipt_path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise GateError(f"cannot read system-test launch receipt: {error}") from error
    if payload.get("state") != "recorded":
        raise GateError("system-test launch receipt is not recorded")
    if payload.get("returncode") != 0 or payload.get("runner_terminated") is not False:
        raise GateError("system-test launch receipt does not describe a clean runner exit")
    if payload.get("workdir") != str(workdir.resolve()):
        raise GateError("system-test launch receipt has unexpected workdir")
    if payload.get("log_path") != str(log_path.resolve()):
        raise GateError("system-test launch receipt has unexpected log path")
    expected_source_identity = {
        "schema": 1,
        "combination_id": source_identity["combination_id"],
        "sources": source_identity["sources"],
    }
    if payload.get("source_identity") != expected_source_identity:
        raise GateError("system-test launch receipt source identity differs from gate preflight")
    interaction = payload.get("interaction")
    if not isinstance(interaction, dict) or interaction != {
        "interactive": True,
        "input_after_marker": "# THEKERNEL_SYSTEM_TEST_COMPLETE",
        "stop_after_marker": None,
    }:
        raise GateError("launch receipt was not produced by marker-triggered system-test shutdown")
    handles = payload.get("launch_handles")
    if not isinstance(handles, dict):
        raise GateError("system-test launch receipt lacks inherited-handle bindings")
    launch_inputs: dict[str, Any] = {}
    # UEFI launches the ESP and rootfs; the standalone kernel ELF is embedded
    # in the ESP and is therefore a final producer artifact, not a QEMU input.
    for role, expected_path in (("esp", artifacts[1]), ("rootfs", artifacts[2])):
        binding = handles.get(role)
        if not isinstance(binding, dict):
            raise GateError(f"system-test launch receipt lacks {role} handle binding")
        try:
            evidence = validate_file_evidence(binding.get("source"), f"receipt {role} handle")
        except EvidenceError as error:
            raise GateError(str(error)) from error
        if evidence["path"] != str(expected_path.resolve()):
            raise GateError(f"system-test launch receipt {role} path is not the final {role}")
        launch_inputs[role] = evidence
    return {
        "producer": "system_test_qemu_runner",
        "receipt": file_evidence(receipt_path),
        "inputs": launch_inputs,
    }


def _default_artifacts(root: Path) -> tuple[Path, ...]:
    """The exact q35 system images produced by the build/system-test steps."""

    raw_state = os.environ.get("THEKERNEL_STATE_DIR", "").strip()
    state = Path(raw_state).expanduser() if raw_state else root / ".state"
    if not state.is_absolute():
        state = root / state
    output = state.resolve() / "out" / "x86_64" / "q35-uefi" / "system" / "smp4-mem1g"
    return (output / "kernel-x86_64", output / "kernel-x86_64.esp", state.resolve() / "out" / "rootfs" / "x86" / "rootfs-x86.img")


def run_gate(
    output: Path,
    *,
    root: Path | None = None,
    runner: CommandRunner = _default_runner,
    commands: dict[str, Sequence[str]] | None = None,
    artifacts: Sequence[Path] = (),
) -> int:
    """Run the ordered gate and atomically publish one stable-schema manifest."""

    root = (root or Path(__file__).resolve().parents[2]).resolve()
    output = output.resolve()
    run_dir = output.parent
    logs = run_dir / "logs"
    system_workdir = run_dir / "system-test"
    receipt_path = system_workdir / "qemu-receipt.json"
    defaults: dict[str, Sequence[str]] = {
        "build": ("./tools/thekernel.py", "build", "--machine", "q35", "--firmware", "uefi", "--smp", "4"),
        "lint": ("./tools/thekernel.py", "lint", "--machine", "q35", "--firmware", "uefi", "--smp", "4"),
        "portable_differential": ("./scripts/host-differential.sh",),
        "system_test": ("./tools/thekernel.py", "system-test", "--machine", "q35", "--firmware", "uefi", "--smp", "4", "--accel", "tcg", "--workdir", str(system_workdir), "--receipt", str(receipt_path)),
    }
    if commands:
        defaults.update(commands)
    artifact_paths = tuple(artifacts) if artifacts else _default_artifacts(root)
    manifest: dict[str, Any] = {
        "schema_version": SCHEMA_VERSION,
        "gate": GATE_NAME,
        "started_at": _timestamp(),
        "state": "failed",
        "preflight": None,
        "postflight": None,
        "commands": [],
        "artifacts": {
            "capture_phase": "after_system_test",
            "producer": "system_test",
            "launch_receipt": None,
            "launch_inputs": {},
            "post_system_test": {},
            "unchanged_since_launch": False,
            "complete": False,
        },
        "guest": None,
        "failure": None,
    }
    try:
        manifest["preflight"] = preflight(root)
        if not manifest["preflight"]["valid"]:
            raise GateError("source preflight rejected: " + ", ".join(manifest["preflight"]["failures"]))
        # Do this only after cleanliness is checked: a manifest under the
        # checkout is itself an untracked file and must not poison preflight.
        logs.mkdir(parents=True, exist_ok=True)
        for name in ("build", "lint", "portable_differential", "system_test"):
            record = _command_record(name, defaults[name], root, logs, runner)
            manifest["commands"].append(record)
            if record["returncode"] != 0:
                if name == "system_test":
                    guest_log = system_workdir / "console.log"
                    if guest_log.is_file():
                        manifest["guest"] = validate_guest_log(
                            guest_log, system_test_returncode=record["returncode"]
                        )
                raise GateError(f"{name} exited {record['returncode']}")
        manifest["postflight"] = preflight(root)
        if (
            not manifest["postflight"]["valid"]
            or manifest["postflight"] != manifest["preflight"]
        ):
            raise GateError("source identity changed during gate")
        guest_log = system_workdir / "console.log"
        if not guest_log.is_file():
            raise GateError(f"system test did not create guest log: {guest_log}")
        guest = validate_guest_log(
            guest_log, system_test_returncode=manifest["commands"][-1]["returncode"]
        )
        manifest["guest"] = guest
        if not guest["valid"]:
            raise GateError("guest validation rejected: " + "; ".join(guest["failures"]))
        # ``system-test`` rebuilds and launches this image triplet.  ESP image
        # construction contains timestamped filesystem metadata, so comparing
        # it to the earlier build output would reject a valid launch.  Bind the
        # final launched inputs once instead; file_evidence rejects mutation
        # while the SHA-256 snapshot itself is being captured.
        launch = _launch_receipt_inputs(
            receipt_path,
            workdir=system_workdir,
            log_path=guest_log,
            artifacts=artifact_paths,
            source_identity=manifest["preflight"],
        )
        final = _final_artifact_evidence(artifact_paths)
        manifest["artifacts"]["launch_receipt"] = launch
        manifest["artifacts"]["launch_inputs"] = launch["inputs"]
        manifest["artifacts"]["post_system_test"] = final
        manifest["artifacts"]["unchanged_since_launch"] = all(
            launch["inputs"][role] == final[role] for role in launch["inputs"]
        )
        if not manifest["artifacts"]["unchanged_since_launch"]:
            raise GateError("system-test launch inputs changed or were replaced after launch")
        manifest["artifacts"]["complete"] = True
        manifest["state"] = "passed"
    except (GateError, EvidenceError, OSError) as error:
        manifest["failure"] = str(error)
    finally:
        manifest["ended_at"] = _timestamp()
        atomic_write_receipt(output, manifest)
    return 0 if manifest["state"] == "passed" else 1


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True, help="manifest JSON path")
    parser.add_argument("--root", type=Path, default=Path.cwd())
    parser.add_argument(
        "--artifact",
        action="append",
        type=Path,
        default=[],
        help="override final kernel, ESP, and rootfs artifacts, in that order",
    )
    args = parser.parse_args(argv)
    return run_gate(args.output, root=args.root, artifacts=args.artifact)


if __name__ == "__main__":
    raise SystemExit(main())
