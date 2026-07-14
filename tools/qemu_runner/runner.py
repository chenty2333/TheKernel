"""Composition layer for explicit images, QEMU topology, and process control."""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path

from .command import VALID_DRIVE_MODES, build_qemu_command
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
    return run_process(
        arch=config.arch,
        command=command,
        workdir=workdir,
        log_path=config.log_path,
        limits=config.limits,
        interaction=config.interaction,
        input_stream=input_stream,
        console_stream=console_stream,
    )
