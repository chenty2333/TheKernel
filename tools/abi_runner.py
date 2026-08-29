#!/usr/bin/env python3
"""Strict q35 differential runner for declared Linux ABI cases."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[1]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

from tools import abi_cases, abi_uapi
from tools.qemu_runner import Interaction, RunConfig, RunLimits, run
from tools.qemu_runner.gate_manifest import validate_guest_log

SUITE_NAMES = {
    "eventfd.portable-differential": "eventfd",
    "creat.raw-differential": "creat",
    "native-ni.fixed-slots": "native-ni",
}
LINUX_COMPLETE = "THEKERNEL_ABI_INIT_COMPLETE"
THEKERNEL_COMPLETE = "# THEKERNEL_SYSTEM_TEST_COMPLETE"


class AbiRunnerError(RuntimeError):
    pass


def _canonical(value: Any) -> bytes:
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=True).encode()


def _sha(value: Any) -> str:
    return hashlib.sha256(_canonical(value)).hexdigest()


def _file_sha(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def _qemu_identity(command: tuple[str, ...]) -> dict[str, Any]:
    requested = command[0]
    resolved = shutil.which(requested) or (requested if Path(requested).is_file() else None)
    if resolved is None:
        return {"requested": requested, "path": None, "sha256": None}
    path = Path(resolved).resolve()
    return {"requested": requested, "path": str(path), "sha256": _file_sha(path)}


def _atomic_json(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.tmp")
    with temporary.open("w", encoding="utf-8") as stream:
        json.dump(value, stream, sort_keys=True, indent=2)
        stream.write("\n")
    os.replace(temporary, path)


def _require_clean_identity(repo_root: Path) -> dict[str, Any]:
    try:
        return abi_cases.capture_source_identity(repo_root)
    except abi_cases.AbiCaseError as error:
        raise AbiRunnerError(str(error)) from error


def _launch_file(receipt: dict[str, Any], field: str, expected: Path) -> None:
    """Require a launch receipt to name and hash the exact current artifact."""
    evidence = receipt.get(field)
    if not isinstance(evidence, dict):
        raise AbiRunnerError(f"TheKernel launch receipt lacks {field} evidence")
    expected = expected.resolve()
    if evidence.get("path") != str(expected):
        raise AbiRunnerError(f"TheKernel launch receipt {field} path differs from runner artifact")
    digest = evidence.get("sha256")
    if not isinstance(digest, str) or digest != _file_sha(expected):
        raise AbiRunnerError(f"TheKernel launch receipt {field} hash differs from runner artifact")


def _validate_thekernel_launch_receipt(
    path: Path, *, identity: dict[str, Any], kernel: Path, esp: Path, rootfs: Path
) -> str:
    """Accept only a clean, source-matched q35 system-test launch receipt."""
    try:
        receipt = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise AbiRunnerError(f"cannot read TheKernel launch receipt: {error}") from error
    if not isinstance(receipt, dict) or receipt.get("state") != "recorded":
        raise AbiRunnerError("TheKernel launch receipt is not a recorded system-test run")
    for field, expected in (("returncode", 0), ("timed_out", False), ("interrupted", False),
                            ("runner_terminated", False), ("guest_clean_shutdown", True)):
        if receipt.get(field) != expected:
            raise AbiRunnerError(f"TheKernel launch receipt is not clean: {field}")
    if receipt.get("error_message") not in (None, ""):
        raise AbiRunnerError("TheKernel launch receipt records a launch error")
    if receipt.get("direct_kernel") is not False:
        raise AbiRunnerError("TheKernel launch receipt is not a q35 UEFI system-test run")
    interaction = receipt.get("interaction")
    if interaction != {
        "interactive": True,
        "input_after_marker": THEKERNEL_COMPLETE,
        "stop_after_marker": None,
    }:
        raise AbiRunnerError("TheKernel launch receipt has an unexpected system-test interaction")

    launch_identity = receipt.get("source_identity")
    if not isinstance(launch_identity, dict) or launch_identity.get("schema") != 1 \
            or launch_identity.get("combination_id") != identity["combination_id"]:
        raise AbiRunnerError("TheKernel launch receipt source identity differs from current combination")
    sources = launch_identity.get("sources")
    if not isinstance(sources, dict) or set(sources) != set(identity["sources"]):
        raise AbiRunnerError("TheKernel launch receipt lacks the declared three-checkout identity")
    for name, expected in identity["sources"].items():
        actual = sources[name]
        if not isinstance(actual, dict) or actual.get("commit") != expected["commit"] \
                or actual.get("tree") != expected["tree"] \
                or actual.get("worktree_dirty") is not False \
                or actual.get("match_declared") is not True:
            raise AbiRunnerError(f"TheKernel launch receipt source identity mismatch for {name}")

    _launch_file(receipt, "kernel", kernel)
    _launch_file(receipt, "esp_source", esp)
    _launch_file(receipt, "rootfs_source", rootfs)
    for field in ("fatal", "failed", "skip", "skipped", "incomplete"):
        if field in receipt and receipt[field] is not False:
            raise AbiRunnerError(f"TheKernel launch receipt records {field}")
    log = receipt.get("log")
    if not isinstance(log, dict) or not isinstance(log.get("path"), str) or not isinstance(log.get("sha256"), str):
        raise AbiRunnerError("TheKernel launch receipt lacks console-log evidence")
    log_path = Path(log["path"]).resolve()
    if not log_path.is_file() or _file_sha(log_path) != log["sha256"]:
        raise AbiRunnerError("TheKernel launch receipt console-log evidence is stale")
    guest = validate_guest_log(log_path, system_test_returncode=0)
    if not guest["valid"]:
        raise AbiRunnerError(
            "TheKernel launch receipt system-test transcript is incomplete or invalid: "
            + "; ".join(guest["failures"])
        )
    return _file_sha(path)


def _verify_rootfs_case_binaries(
    rootfs: Path, cases: list[dict[str, Any]], repo_root: Path
) -> dict[str, str]:
    """Prove the receipt bytes are exactly the bytes installed in the image."""
    debugfs = shutil.which("debugfs")
    if debugfs is None:
        raise AbiRunnerError("debugfs is required to verify ABI binaries in the rootfs")
    verified: dict[str, str] = {}
    with tempfile.TemporaryDirectory(prefix="abi-rootfs-binaries-") as directory:
        output_root = Path(directory)
        for case in cases:
            basename = Path(case["binary"]).name
            guest_path = f"/opt/thekernel-tests/portable/{basename}"
            extracted = output_root / basename
            completed = subprocess.run(
                (debugfs, "-R", f"dump -p {guest_path} {extracted}", str(rootfs)),
                check=False,
                capture_output=True,
                text=True,
            )
            if completed.returncode != 0 or not extracted.is_file():
                detail = (completed.stderr or completed.stdout).strip()
                raise AbiRunnerError(
                    f"{case['id']}: cannot extract {guest_path} from rootfs: {detail}"
                )
            installed = _file_sha(extracted)
            published = _file_sha(repo_root / case["binary"])
            if installed != published:
                raise AbiRunnerError(
                    f"{case['id']}: rootfs ABI binary differs from published receipt bytes"
                )
            verified[case["id"]] = installed
    return verified


def _read_digest(path: Path, label: str) -> str:
    try:
        value = path.read_text(encoding="ascii").strip()
    except (OSError, UnicodeError) as error:
        raise AbiRunnerError(f"cannot read {label}: {error}") from error
    if not abi_uapi.HEX_64.fullmatch(value):
        raise AbiRunnerError(f"{label} is not a bound lowercase SHA-256")
    return value


def _extract_rootfs_file(rootfs: Path, guest_path: str, destination: Path) -> None:
    debugfs = shutil.which("debugfs")
    if debugfs is None:
        raise AbiRunnerError("debugfs is required to verify ABI rootfs metadata")
    completed = subprocess.run(
        (debugfs, "-R", f"dump -p {guest_path} {destination}", str(rootfs)),
        check=False, capture_output=True, text=True,
    )
    if completed.returncode != 0 or not destination.is_file():
        detail = (completed.stderr or completed.stdout).strip()
        raise AbiRunnerError(f"cannot extract {guest_path} from rootfs: {detail}")


def _verify_uapi_provenance(
    *, repo_root: Path, uapi_headers: Path, rootfs: Path, cases: list[dict[str, Any]]
) -> dict[str, str]:
    """Bind formal test binaries to the pinned, materialized Linux UAPI tree."""
    manifest_path = repo_root / "docs/linux-abi/uapi-headers.json"
    try:
        manifest = abi_uapi.load_manifest(manifest_path, repo_root)
        expected_headers = abi_uapi.repository_path(
            repo_root, manifest["headers"]["materialized_path"], "headers.materialized_path"
        )
    except abi_uapi.UapiError as error:
        raise AbiRunnerError(f"invalid pinned UAPI provenance: {error}") from error
    if uapi_headers.resolve() != expected_headers.resolve() or not uapi_headers.is_dir():
        raise AbiRunnerError("--uapi-headers must be the manifest's materialized pinned UAPI tree")
    try:
        tree_digest = abi_uapi.tree_sha256(uapi_headers)
    except abi_uapi.UapiError as error:
        raise AbiRunnerError(f"cannot verify UAPI header tree: {error}") from error
    expected_digest = manifest["headers"]["tree_sha256"]
    if tree_digest != expected_digest:
        raise AbiRunnerError("UAPI header tree hash differs from the pinned manifest")

    metadata_paths = {
        (repo_root / Path(case["binary"]).parent / ".uapi-sha256").resolve()
        for case in cases
    }
    if len(metadata_paths) != 1:
        raise AbiRunnerError("selected ABI cases do not share one published UAPI metadata file")
    published_metadata = next(iter(metadata_paths))
    published_digest = _read_digest(published_metadata, "published ABI UAPI metadata")
    if published_digest != tree_digest:
        raise AbiRunnerError("published ABI UAPI metadata is unbound or mismatched")

    with tempfile.TemporaryDirectory(prefix="abi-rootfs-uapi-") as directory:
        rootfs_metadata = Path(directory) / "abi-uapi-sha256"
        _extract_rootfs_file(rootfs, "/usr/share/thekernel/abi-uapi-sha256", rootfs_metadata)
        rootfs_digest = _read_digest(rootfs_metadata, "rootfs ABI UAPI metadata")
        rootfs_metadata_hash = _file_sha(rootfs_metadata)
    if rootfs_digest != tree_digest:
        raise AbiRunnerError("rootfs ABI UAPI metadata is unbound or mismatched")
    return {
        "headers_path": str(uapi_headers.resolve()),
        "headers_tree_sha256": tree_digest,
        "published_metadata_path": str(published_metadata),
        "published_metadata_sha256": _file_sha(published_metadata),
        "rootfs_metadata_path": "/usr/share/thekernel/abi-uapi-sha256",
        "rootfs_metadata_sha256": rootfs_metadata_hash,
    }


def _normalise_linux(transcript: str) -> str:
    return transcript


def _normalise_thekernel(transcript: str, cases: list[dict[str, Any]]) -> str:
    unknown = [case["id"] for case in cases if case["id"] not in SUITE_NAMES]
    if unknown:
        raise AbiRunnerError(f"unmapped TheKernel suite cases: {unknown}")
    selected_names = set(SUITE_NAMES[case["id"]] for case in cases)
    lines: list[str] = []
    for line in transcript.splitlines():
        if line.startswith("# ") and ": " in line:
            name, payload = line[2:].split(": ", 1)
            if name in selected_names:
                lines.append(payload)
        elif line.startswith("not ok ") or " # SKIP " in line:
            # A selected suite case must never be hidden in diagnostics.
            for name in selected_names:
                if line.endswith(f"- {name}") or f"- {name} #" in line:
                    raise AbiRunnerError(f"TheKernel suite did not pass required case {name}: {line}")
    return "\n".join(lines) + ("\n" if lines else "")


def _case_transcript(transcript: str, case_id: str) -> str:
    """Return exactly one ABI case boundary for build_receipt's strict API."""
    begin = f"THEKERNEL_ABI_CASE {case_id}"
    result_prefix = f"THEKERNEL_ABI_RESULT {case_id} "
    selected: list[str] = []
    active = False
    for line in transcript.splitlines():
        if line == begin:
            if active:
                raise AbiRunnerError(f"{case_id}: duplicate case boundary")
            active = True
        if active:
            selected.append(line)
        if active and line.startswith(result_prefix):
            active = False
            break
    if not selected or active:
        raise AbiRunnerError(f"{case_id}: cannot isolate ABI transcript")
    return "\n".join(selected) + "\n"


def _validate_result(result: Any, transcript: str, completion: str, target: str) -> None:
    if result.timed_out or result.interrupted or result.runner_terminated or not result.guest_clean_shutdown:
        raise AbiRunnerError(f"{target}: non-clean QEMU termination")
    if result.returncode != 0 or result.error_message:
        raise AbiRunnerError(f"{target}: QEMU failed exit={result.returncode} error={result.error_message}")
    fatal_markers = (
        "Kernel panic - not syncing",
        "THEKERNEL_PANIC",
        "THEKERNEL_ABI_INIT_FAIL",
        "BUG:",
        "Oops:",
    )
    if any(marker in transcript for marker in fatal_markers):
        raise AbiRunnerError(f"{target}: guest panic or ABI init failure")
    if completion not in transcript:
        raise AbiRunnerError(f"{target}: missing clean shutdown marker {completion}")


def _run_target(*, target: str, cases: list[dict[str, Any]], repo_root: Path, rootfs: Path,
                kernel: Path, esp: Path | None, output: Path, qemu: str | None,
                resources: dict[str, int], accel: str | None, thekernel_launch_receipt_sha256: str,
                guest_binary_hashes: dict[str, str], uapi_provenance: dict[str, str]) -> list[dict[str, Any]]:
    direct = target == "linux-product"
    if not direct and esp is None:
        raise AbiRunnerError("thekernel: --thekernel-esp is required")
    case_ids = ",".join(case["id"] for case in cases)
    if direct:
        expected = {case["oracle_configs"][target].get("kernel_sha256") for case in cases}
        if len(expected) != 1 or not isinstance(next(iter(expected)), str):
            raise AbiRunnerError("linux-product: cases do not agree on a pinned kernel")
        if _file_sha(kernel) != next(iter(expected)):
            raise AbiRunnerError("linux-product: kernel does not match declared oracle hash")
    commandline = (
        "root=/dev/vda",
        "rw",
        "console=ttyS0",
        "init=/etc/thekernel/abi-init.sh",
        "panic=-1",
        "reboot=t",
        f"thekernel_abi_cases={case_ids}",
    ) if direct else ()
    target_output = output / target
    target_output.mkdir(parents=True, exist_ok=True)
    command_input = target_output / ("empty.commands" if direct else "shutdown.commands")
    command_input.write_bytes(
        b"" if direct else b"/bin/busybox poweroff -f\nexit\n"
    )
    interaction = Interaction(
        interactive=True,
        input_after_marker=None if direct else THEKERNEL_COMPLETE,
    )
    qemu_receipt = target_output / "qemu-receipt.json"
    config = RunConfig(
        arch="x86_64", kernel=kernel, rootfs=rootfs, esp=esp, workdir=target_output,
        log_path=target_output / "console.log", rootfs_mode="snapshot",
        receipt_path=qemu_receipt, input_path=command_input,
        direct_kernel=direct, memory=f"{resources['memory_mib']}M", cpus=resources["cpus"], qemu_binary=qemu, accel=accel,
        limits=RunLimits(total_timeout_secs=resources["profile_timeout_seconds"]), extra_args=("-append", " ".join(commandline)) if direct else (),
        interaction=interaction,
    )
    result = run(config)
    transcript = result.log_path.read_text(encoding="utf-8", errors="replace")
    _validate_result(result, transcript, LINUX_COMPLETE if direct else THEKERNEL_COMPLETE, target)
    normalized = _normalise_linux(transcript) if direct else _normalise_thekernel(transcript, cases)
    try:
        launch_receipt = json.loads(qemu_receipt.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise AbiRunnerError(f"{target}: missing or invalid QEMU launch receipt: {error}") from error
    if launch_receipt.get("returncode") != 0 or not launch_receipt.get("guest_clean_shutdown"):
        raise AbiRunnerError(f"{target}: QEMU launch receipt is not a clean run")
    try:
        outcomes = abi_cases.validate_transcript(normalized, cases)
    except abi_cases.AbiCaseError as error:
        raise AbiRunnerError(f"{target}: invalid ABI transcript: {error}") from error
    if any(entry["outcome"] == "skip" for entry in outcomes):
        raise AbiRunnerError(f"{target}: required ABI case skipped")
    topology = {"machine": "q35", "direct_kernel": direct, "cpus": resources["cpus"],
                "memory": f"{resources['memory_mib']}M", "rootfs_mode": "snapshot"}
    config_evidence = {"topology": topology, "accel": accel, "qemu": qemu}
    records = []
    for case, outcome in zip(cases, outcomes, strict=True):
        case_transcript = _case_transcript(normalized, case["id"])
        base = abi_cases.build_receipt(case, repo_root=repo_root, command=list(result.command),
                                       target=target, exit_code=result.returncode, transcript=case_transcript)
        base["runner"] = {
            "qemu_command": list(result.command), "qemu_command_sha256": _sha(list(result.command)),
            "qemu": _qemu_identity(result.command), "qemu_exit": result.returncode,
            "duration_ms": getattr(result, "duration_ms", None), "timeout": resources["profile_timeout_seconds"],
            "timeout_sha256": _sha(resources["profile_timeout_seconds"]), "timed_out": result.timed_out,
            "clean_shutdown": result.guest_clean_shutdown, "artifact_sha256": _file_sha(kernel),
            "rootfs_sha256": _file_sha(rootfs), "config": topology,
            "guest_binary_sha256": guest_binary_hashes[case["id"]],
            "config_sha256": _sha(config_evidence), "topology_sha256": _sha(topology), "cmdline": list(commandline),
            "cmdline_sha256": _sha(list(commandline)), "log_sha256": _file_sha(result.log_path),
            "qemu_receipt_sha256": _file_sha(qemu_receipt),
            "thekernel_launch_receipt_sha256": thekernel_launch_receipt_sha256,
            "uapi": uapi_provenance,
            "outcome": outcome,
        }
        records.append(base)
    return records


def execute(args: argparse.Namespace) -> Path:
    repo_root = args.repo_root.resolve()
    before = _require_clean_identity(repo_root)
    cases = abi_cases.load_manifest(repo_root=repo_root)
    if args.case:
        wanted = set(args.case)
        cases = [case for case in cases if case["id"] in wanted]
        if len(cases) != len(wanted):
            raise AbiRunnerError("requested ABI case is not declared")
    if not cases or any(not abi_cases.is_gate_eligible(case) for case in cases):
        raise AbiRunnerError("all selected ABI cases must be required gate cases")
    try:
        resources = abi_cases.selected_resources(cases)
    except abi_cases.AbiCaseError as error:
        raise AbiRunnerError(str(error)) from error
    for path in (args.rootfs, args.linux_kernel, args.thekernel_kernel, args.thekernel_esp,
                 args.thekernel_launch_receipt):
        if path is not None and not path.is_file():
            raise AbiRunnerError(f"missing artifact: {path}")
    if not args.uapi_headers.is_dir():
        raise AbiRunnerError(f"missing UAPI header tree: {args.uapi_headers}")
    guest_binary_hashes = _verify_rootfs_case_binaries(args.rootfs, cases, repo_root)
    uapi_provenance = _verify_uapi_provenance(
        repo_root=repo_root, uapi_headers=args.uapi_headers, rootfs=args.rootfs, cases=cases
    )
    thekernel_launch_receipt_sha256 = _validate_thekernel_launch_receipt(
        args.thekernel_launch_receipt, identity=before, kernel=args.thekernel_kernel,
        esp=args.thekernel_esp, rootfs=args.rootfs,
    )
    destination = args.output.resolve()
    destination.parent.mkdir(parents=True, exist_ok=True)
    staging = Path(tempfile.mkdtemp(prefix=".abi-run-", dir=destination.parent))
    try:
        all_records: list[dict[str, Any]] = []
        all_records += _run_target(target="linux-product", cases=cases, repo_root=repo_root,
            rootfs=args.rootfs, kernel=args.linux_kernel, esp=None, output=staging, qemu=args.qemu,
            resources=resources, accel=args.accel,
            thekernel_launch_receipt_sha256=thekernel_launch_receipt_sha256,
            guest_binary_hashes=guest_binary_hashes, uapi_provenance=uapi_provenance)
        all_records += _run_target(target="thekernel", cases=cases, repo_root=repo_root,
            rootfs=args.rootfs, kernel=args.thekernel_kernel, esp=args.thekernel_esp, output=staging,
            qemu=args.qemu, resources=resources, accel=args.accel,
            thekernel_launch_receipt_sha256=thekernel_launch_receipt_sha256,
            guest_binary_hashes=guest_binary_hashes, uapi_provenance=uapi_provenance)
        after = _require_clean_identity(repo_root)
        if before != after:
            raise AbiRunnerError("source identity drifted during ABI run")
        for record in all_records:
            _atomic_json(staging / "receipts" / record["target"] / f"{record['case_id']}.json", record)
        _atomic_json(staging / "run-group.json", {"schema": "thekernel-abi-run-group-v1", "source_identity": before,
            "thekernel_launch_receipt_sha256": thekernel_launch_receipt_sha256, "resources": resources,
            "uapi": uapi_provenance,
            "receipts": [{"target": row["target"], "case_id": row["case_id"]} for row in all_records]})
        if destination.exists():
            raise AbiRunnerError(f"output already exists: {destination}")
        os.replace(staging, destination)
        return destination
    except Exception:
        shutil.rmtree(staging, ignore_errors=True)
        raise


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo-root", type=Path, default=ROOT)
    parser.add_argument("--rootfs", type=Path, required=True)
    parser.add_argument("--linux-kernel", type=Path, required=True)
    parser.add_argument("--thekernel-kernel", type=Path, required=True)
    parser.add_argument("--thekernel-esp", type=Path, required=True)
    parser.add_argument("--thekernel-launch-receipt", type=Path, required=True)
    parser.add_argument("--uapi-headers", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--case", action="append")
    parser.add_argument("--qemu")
    parser.add_argument("--accel", default="tcg")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    try:
        output = execute(parse_args(sys.argv[1:] if argv is None else argv))
    except (AbiRunnerError, abi_cases.AbiCaseError, OSError) as error:
        print(f"abi-runner: {error}", file=sys.stderr)
        return 1
    print(output)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
