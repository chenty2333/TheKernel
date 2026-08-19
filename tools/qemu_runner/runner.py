"""Composition layer for explicit images, QEMU topology, and process control."""

from __future__ import annotations

import os
import shutil
import sys
from dataclasses import dataclass
from pathlib import Path

from .command import VALID_DRIVE_MODES, VALID_NETWORK_MODES, build_qemu_command
from .evidence import file_evidence
from .images import materialize_writable_image, prepare_image
from .model import Arch, Drive, DriveMode, Interaction, RunLimits, RunResult
from .process import run_process
from .receipt import (
    RECEIPT_SCHEMA_VERSION,
    atomic_write_receipt,
    input_forwarding_payload,
)


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
    esp: Path | None = None
    rootfs_mode: DriveMode = "snapshot"
    extra_block: Path | None = None
    extra_block_mode: DriveMode = "rw"
    limits: RunLimits = RunLimits()
    interaction: Interaction = Interaction()
    memory: str = "1G"
    cpus: int = 1
    qemu_binary: str | None = None
    qemu_launcher: tuple[str, ...] | None = None
    accel: str | None = None
    cpu: str | None = None
    iothread_id: str | None = None
    # Correctness lanes retain QEMU's user-mode network by default.  The
    # performance lane opts into ``passt`` (or ``tap-vhost``) explicitly so
    # a normal qemu-runner invocation never unexpectedly requires host setup.
    network: str = "user"
    network_mode: str | None = None
    network_topology: str | None = None
    tap_name: str | None = None
    extra_args: tuple[str, ...] = ()
    ovmf_code: Path | None = None
    ovmf_vars: Path | None = None
    direct_kernel: bool = False
    receipt_path: Path | None = None
    external_input_producer: bool = False


def normalize_arch(value: str) -> Arch:
    if value in {"x86", "x86_64"}:
        return "x86_64"
    raise RunnerError(f"unsupported architecture: {value}")


def _resolve_ovmf_image(
    configured: Path | None,
    environment: str,
    candidates: tuple[str, ...],
    label: str,
) -> Path:
    raw = configured or (Path(os.environ[environment]) if os.environ.get(environment) else None)
    if raw is None:
        raw = next((Path(path) for path in candidates if Path(path).is_file()), None)
    if raw is None:
        option = environment.removeprefix("THEKERNEL_").lower().replace("_", "-")
        raise RunnerError(
            f"x86_64 UEFI requires {label}; pass --{option} "
            f"or set {environment}"
        )
    path = raw.expanduser().resolve()
    if not path.is_file():
        raise RunnerError(f"{label} does not exist: {path}")
    return path


def _copy_ovmf_vars(source: Path, workdir: Path) -> Path:
    destination = workdir / "firmware" / "OVMF_VARS.fd"
    destination.parent.mkdir(parents=True, exist_ok=True)
    temporary = destination.with_name(f".{destination.name}.tmp")
    temporary.unlink(missing_ok=True)
    try:
        shutil.copy2(source, temporary)
        temporary.replace(destination)
        destination.chmod(destination.stat().st_mode | 0o600)
    finally:
        temporary.unlink(missing_ok=True)
    return destination


def _validate_mode(name: str, mode: str) -> DriveMode:
    if mode not in VALID_DRIVE_MODES:
        raise RunnerError(f"unsupported {name} drive mode: {mode}")
    return mode  # type: ignore[return-value]


def _validate_network(config: RunConfig) -> str:
    network = config.network
    if config.network_mode is not None:
        if network != "user" and network != config.network_mode:
            raise RunnerError("network and network_mode disagree")
        network = config.network_mode
    if config.network_topology is not None:
        if network != "user" and network != config.network_topology:
            raise RunnerError("network and network_topology disagree")
        network = config.network_topology
    if network not in VALID_NETWORK_MODES:
        raise RunnerError(f"unsupported network topology: {network}")
    if config.tap_name is not None and (
        not config.tap_name
        or any(char in config.tap_name for char in ",=\n\r")
    ):
        raise RunnerError("tap name must be a non-empty QEMU-safe value")
    return network


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


def _qemu_evidence(requested: str) -> dict[str, str | int | None]:
    resolved = shutil.which(requested)
    if resolved is None and "/" in requested:
        candidate = Path(requested).expanduser().resolve()
        if candidate.is_file():
            resolved = str(candidate)
    if resolved is None:
        return {"requested": requested, "path": None, "size_bytes": None, "sha256": None}
    evidence = file_evidence(Path(resolved).resolve())
    return {"requested": requested, **evidence}


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

    if config.external_input_producer and not config.interaction.interactive:
        raise RunnerError("external input producer requires interactive mode")
    if config.external_input_producer and config.receipt_path is None:
        raise RunnerError("external input producer requires a receipt")

    kernel = config.kernel.expanduser().resolve()
    if not kernel.is_file():
        raise RunnerError(f"kernel does not exist: {kernel}")
    workdir = config.workdir.expanduser().resolve()
    workdir.mkdir(parents=True, exist_ok=True)
    cache_dir = config.cache_dir.expanduser().resolve()

    rootfs_mode = _validate_mode("rootfs", config.rootfs_mode)
    extra_mode = _validate_mode("extra-block", config.extra_block_mode)
    network = _validate_network(config)
    effective_qemu_launcher = config.qemu_launcher
    if (
        effective_qemu_launcher is None
        and config.qemu_binary is not None
        and config.qemu_binary.endswith(".py")
    ):
        # The scheduler pinner is a source-controlled Python entry point and
        # intentionally need not carry an executable bit in a checkout.
        effective_qemu_launcher = (sys.executable, config.qemu_binary)
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

    esp = None
    ovmf_code = None
    ovmf_vars_source = None
    ovmf_vars_runtime = None
    if not config.direct_kernel:
        if config.esp is None:
            raise RunnerError(
                "x86_64 UEFI boot requires a GPT ESP; pass --esp or use --direct-kernel"
            )
        esp = _prepare_drive(
            config.esp,
            mode="snapshot",
            label="esp",
            cache_dir=cache_dir,
            workdir=workdir,
        )
        ovmf_code = _resolve_ovmf_image(
            config.ovmf_code,
            "THEKERNEL_OVMF_CODE",
            (
                "/usr/share/edk2/ovmf/OVMF_CODE.fd",
                "/usr/share/edk2/ovmf/OVMF_CODE_4M.fd",
                "/usr/share/OVMF/OVMF_CODE.fd",
            ),
            "OVMF code",
        )
        ovmf_vars_source = _resolve_ovmf_image(
            config.ovmf_vars,
            "THEKERNEL_OVMF_VARS",
            (
                "/usr/share/edk2/ovmf/OVMF_VARS.fd",
                "/usr/share/edk2/ovmf/OVMF_VARS_4M.fd",
                "/usr/share/OVMF/OVMF_VARS.fd",
            ),
            "OVMF vars",
        )
        ovmf_vars_runtime = _copy_ovmf_vars(ovmf_vars_source, workdir)

    command = build_qemu_command(
        arch=config.arch,
        kernel=kernel,
        rootfs=rootfs,
        extra_block=extra_block,
        esp=esp,
        ovmf_code=ovmf_code,
        ovmf_vars=ovmf_vars_runtime,
        direct_kernel=config.direct_kernel,
        memory=config.memory,
        cpus=config.cpus,
        qemu_binary=config.qemu_binary,
        qemu_launcher=effective_qemu_launcher,
        accel=config.accel,
        cpu=config.cpu,
        iothread_id=config.iothread_id,
        network=network,
        tap_name=config.tap_name,
        extra_args=config.extra_args,
    )
    qemu_requested = config.qemu_binary or "qemu-system-x86_64"
    qemu = _qemu_evidence(qemu_requested)
    if qemu["path"] is not None and effective_qemu_launcher is None:
        command = (str(qemu["path"]), *command[1:])
    rootfs_source_path = config.rootfs.expanduser().resolve()
    rootfs_runtime_path = rootfs.path.resolve()
    run_input_paths = [kernel, rootfs_source_path, rootfs_runtime_path]
    if config.esp is not None and esp is not None:
        run_input_paths.extend([config.esp.expanduser().resolve(), esp.path.resolve()])
    if ovmf_code is not None:
        run_input_paths.append(ovmf_code)
    if ovmf_vars_source is not None and ovmf_vars_runtime is not None:
        run_input_paths.extend([ovmf_vars_source, ovmf_vars_runtime])
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
        "schema_version": RECEIPT_SCHEMA_VERSION,
        "state": "prepared",
        "arch": config.arch,
        "cpus": config.cpus,
        "memory": config.memory,
        "accel": config.accel,
        "cpu": config.cpu,
        "iothread_id": config.iothread_id,
        "network": network,
        "tap_name": config.tap_name,
        "extra_args": list(config.extra_args),
        "kernel": file_evidence(kernel),
        "rootfs_source": rootfs_source,
        "rootfs_runtime_before": rootfs_runtime,
        "qemu": qemu,
        "command": list(command),
        "qemu_launcher": list(effective_qemu_launcher) if effective_qemu_launcher is not None else None,
        "workdir": str(workdir),
        "log_path": str(log_path),
        "rootfs_mode": rootfs_mode,
        "direct_kernel": config.direct_kernel,
        "extra_block_mode": extra_mode,
        "interaction": {
            "interactive": config.interaction.interactive,
            "input_after_marker": config.interaction.input_after_marker,
            "stop_after_marker": config.interaction.stop_after_marker,
            "external_input_producer": config.external_input_producer,
        },
    }
    if config.esp is not None and esp is not None:
        receipt["esp_source"] = file_evidence(config.esp.expanduser().resolve())
        receipt["esp_runtime"] = file_evidence(esp.path.resolve())
    if ovmf_code is not None:
        receipt["ovmf_code"] = file_evidence(ovmf_code)
    if ovmf_vars_source is not None and ovmf_vars_runtime is not None:
        receipt["ovmf_vars_source"] = file_evidence(ovmf_vars_source)
        receipt["ovmf_vars_runtime"] = file_evidence(ovmf_vars_runtime)
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
        atomic_write_receipt(receipt_path, receipt)

    result = run_process(
        arch=config.arch,
        command=command,
        workdir=workdir,
        log_path=log_path,
        limits=config.limits,
        interaction=config.interaction,
        input_stream=input_stream,
        console_stream=console_stream,
        proxy_interactive_input=config.external_input_producer,
    )
    if receipt_path is not None:
        receipt["rootfs_runtime_after"] = file_evidence(rootfs_runtime_path)
        receipt["log"] = file_evidence(result.log_path)
        if config.extra_block is not None and extra_block is not None:
            receipt["extra_block_runtime_after"] = file_evidence(extra_block.path.resolve())
        final_state = "awaiting_producer" if config.external_input_producer else "complete"
        if result.input_forwarding is not None:
            stdin = input_forwarding_payload(result.input_forwarding)
            if not config.external_input_producer:
                stdin.update(
                    {
                        "state": "runner_complete",
                        "source_fully_relayed": result.input_forwarding.relay_complete,
                    }
                )
            receipt["stdin"] = stdin
        receipt.update(
            {
                "state": final_state,
                "returncode": result.returncode,
                "duration_ms": result.duration_ms,
                "error_message": result.error_message,
                "timed_out": result.timed_out,
                "interrupted": result.interrupted,
                "intentionally_stopped": result.intentionally_stopped,
            }
        )
        atomic_write_receipt(receipt_path, receipt)
    return result
