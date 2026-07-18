"""Composition layer for explicit images, QEMU topology, and process control."""

from __future__ import annotations

import json
import os
import shutil
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from .command import VALID_DRIVE_MODES, build_qemu_command
from .evidence import file_evidence
from .images import materialize_writable_image, prepare_image
from .model import Arch, Drive, DriveMode, Interaction, RunLimits, RunResult
from .process import run_process


class RunnerError(ValueError):
    """Raised for an invalid product-run configuration."""


@dataclass(frozen=True)
class RunConfig:
    arch: Arch
    kernel: Path
    rootfs: Path
    workdir: Path
    log_path: Path
    cache_dir: Path
    rootfs_mode: DriveMode = "snapshot"
    extra_block: Path | None = None
    extra_block_mode: DriveMode = "rw"
    limits: RunLimits = RunLimits()
    interaction: Interaction = Interaction()
    memory: str = "1G"
    cpus: int = 1
    qemu_binary: str | None = None
    receipt_path: Path | None = None


def normalize_arch(value: str) -> Arch:
    if value in {"rv", "riscv64"}:
        return "rv"
    if value in {"la", "loongarch64"}:
        return "la"
    raise RunnerError(f"unsupported architecture: {value}")


def _validate_mode(name: str, mode: str) -> DriveMode:
    if mode not in VALID_DRIVE_MODES:
        raise RunnerError(f"unsupported {name} drive mode: {mode}")
    return mode  # type: ignore[return-value]


def _prepare_drive(
    source: Path,
    *,
    mode: DriveMode,
    label: str,
    cache_dir: Path,
    workdir: Path,
) -> Drive:
    prepared = prepare_image(source, cache_dir=cache_dir)
    runtime = prepared.runtime
    if mode == "rw":
        runtime = materialize_writable_image(
            prepared,
            destination_dir=workdir / "writable-images",
            label=label,
        )
    return Drive(path=runtime, mode=mode)


def _qemu_evidence(command: tuple[str, ...]) -> dict[str, str | int | None]:
    requested = command[0]
    resolved = shutil.which(requested)
    if resolved is None and "/" in requested:
        candidate = Path(requested).expanduser().resolve()
        if candidate.is_file():
            resolved = str(candidate)
    if resolved is None:
        return {"requested": requested, "path": None, "size_bytes": None, "sha256": None}
    evidence = file_evidence(Path(resolved).resolve())
    return {"requested": requested, **evidence}


def _write_receipt(path: Path, payload: dict[str, Any]) -> None:
    path = path.expanduser().resolve()
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary_name = tempfile.mkstemp(
        dir=path.parent,
        prefix=f".{path.name}.",
        suffix=".tmp",
    )
    temporary = Path(temporary_name)
    try:
        output = os.fdopen(descriptor, "w", encoding="utf-8")
        descriptor = -1
        with output:
            json.dump(payload, output, indent=2, sort_keys=True)
            output.write("\n")
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, path)
    finally:
        if descriptor >= 0:
            os.close(descriptor)
        temporary.unlink(missing_ok=True)


def _validate_output_destination(
    destination: Path,
    *,
    label: str,
    protected_paths: tuple[Path, ...],
) -> Path:
    destination = destination.expanduser().resolve()
    for protected in protected_paths:
        protected = protected.expanduser().resolve()
        aliases = destination == protected
        if not aliases and destination.exists() and protected.exists():
            aliases = destination.samefile(protected)
        if aliases:
            raise RunnerError(f"{label} aliases a run input or output: {destination}")
    return destination


def run(
    config: RunConfig,
    *,
    input_stream=None,
    console_stream=None,
) -> RunResult:
    """Prepare explicit artifacts and run one architecture without discovery."""

    kernel = config.kernel.expanduser().resolve()
    if not kernel.is_file():
        raise RunnerError(f"kernel does not exist: {kernel}")
    workdir = config.workdir.expanduser().resolve()
    workdir.mkdir(parents=True, exist_ok=True)
    cache_dir = config.cache_dir.expanduser().resolve()

    rootfs_mode = _validate_mode("rootfs", config.rootfs_mode)
    extra_mode = _validate_mode("extra-block", config.extra_block_mode)
    rootfs = _prepare_drive(
        config.rootfs,
        mode=rootfs_mode,
        label="rootfs",
        cache_dir=cache_dir,
        workdir=workdir,
    )
    extra_block = (
        _prepare_drive(
            config.extra_block,
            mode=extra_mode,
            label="extra",
            cache_dir=cache_dir,
            workdir=workdir,
        )
        if config.extra_block is not None
        else None
    )

    command = build_qemu_command(
        arch=config.arch,
        kernel=kernel,
        rootfs=rootfs,
        extra_block=extra_block,
        memory=config.memory,
        cpus=config.cpus,
        qemu_binary=config.qemu_binary,
    )
    qemu = _qemu_evidence(command)
    if qemu["path"] is not None:
        command = (str(qemu["path"]), *command[1:])
    rootfs_source_path = config.rootfs.expanduser().resolve()
    rootfs_runtime_path = rootfs.path.resolve()
    run_input_paths = [kernel, rootfs_source_path, rootfs_runtime_path]
    if qemu["path"] is not None:
        run_input_paths.append(Path(str(qemu["path"])))
    if config.extra_block is not None and extra_block is not None:
        run_input_paths.extend(
            [config.extra_block.expanduser().resolve(), extra_block.path.resolve()]
        )
    log_path = _validate_output_destination(
        config.log_path,
        label="log",
        protected_paths=tuple(run_input_paths),
    )
    rootfs_source = file_evidence(rootfs_source_path)
    rootfs_runtime = (
        rootfs_source.copy()
        if rootfs_runtime_path == rootfs_source_path
        else file_evidence(rootfs_runtime_path)
    )
    receipt = {
        "schema_version": 1,
        "state": "prepared",
        "arch": config.arch,
        "cpus": config.cpus,
        "memory": config.memory,
        "kernel": file_evidence(kernel),
        "rootfs_source": rootfs_source,
        "rootfs_runtime_before": rootfs_runtime,
        "qemu": qemu,
        "command": list(command),
        "workdir": str(workdir),
        "log_path": str(log_path),
        "rootfs_mode": rootfs_mode,
        "extra_block_mode": extra_mode,
    }
    if config.extra_block is not None and extra_block is not None:
        receipt["extra_block_source"] = file_evidence(config.extra_block.expanduser().resolve())
        receipt["extra_block_runtime_before"] = file_evidence(extra_block.path.resolve())
    receipt_path = None
    if config.receipt_path is not None:
        receipt_path = _validate_output_destination(
            config.receipt_path,
            label="receipt",
            protected_paths=tuple([*run_input_paths, log_path]),
        )
        _write_receipt(receipt_path, receipt)

    result = run_process(
        arch=config.arch,
        command=command,
        workdir=workdir,
        log_path=log_path,
        limits=config.limits,
        interaction=config.interaction,
        input_stream=input_stream,
        console_stream=console_stream,
    )
    if receipt_path is not None:
        receipt["rootfs_runtime_after"] = file_evidence(rootfs_runtime_path)
        receipt["log"] = file_evidence(result.log_path)
        if config.extra_block is not None and extra_block is not None:
            receipt["extra_block_runtime_after"] = file_evidence(extra_block.path.resolve())
        receipt.update(
            {
                "state": "complete",
                "returncode": result.returncode,
                "duration_ms": result.duration_ms,
                "error_message": result.error_message,
                "timed_out": result.timed_out,
                "interrupted": result.interrupted,
                "intentionally_stopped": result.intentionally_stopped,
            }
        )
        _write_receipt(receipt_path, receipt)
    return result
