"""Composition layer for explicit images, QEMU topology, and process control."""

from __future__ import annotations

import gzip
import lzma
import os
import shutil
import sys
from dataclasses import dataclass
from pathlib import Path

from .command import VALID_DRIVE_MODES, VALID_NETWORK_MODES, build_qemu_command
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
    input_path: Path | None = None


@dataclass(frozen=True)
class _DrivePlan:
    source: Path
    runtime: Path
    mode: DriveMode
    label: str
    compression_suffix: str | None


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


def _copy_ovmf_vars(source: Path, destination: Path) -> Path:
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


def _plan_drive(
    source: Path,
    *,
    mode: DriveMode,
    label: str,
    workdir: Path,
) -> _DrivePlan:
    source = source.expanduser().resolve()
    if not source.is_file():
        raise RunnerError(f"{label} image does not exist: {source}")
    if source.stat().st_size == 0:
        raise RunnerError(f"{label} image is empty: {source}")
    runtime = source
    compression_suffix = None
    if source.name.endswith((".xz", ".gz")):
        compression_suffix = ".xz" if source.name.endswith(".xz") else ".gz"
        runtime = (
            workdir
            / "images"
            / f"{label}-{source.name.removesuffix(compression_suffix)}"
        ).resolve()
    return _DrivePlan(
        source=source,
        runtime=runtime,
        mode=mode,
        label=label,
        compression_suffix=compression_suffix,
    )


def _prepare_drive(plan: _DrivePlan) -> Drive:
    source = plan.source
    runtime = plan.runtime
    if plan.compression_suffix is not None:
        runtime.parent.mkdir(parents=True, exist_ok=True)
        temporary = runtime.with_name(f".{runtime.name}.tmp")
        temporary.unlink(missing_ok=True)
        opener = lzma.open if plan.compression_suffix == ".xz" else gzip.open
        try:
            with opener(source, "rb") as input_file, temporary.open("wb") as output_file:
                shutil.copyfileobj(input_file, output_file, length=1024 * 1024)
            if temporary.stat().st_size == 0:
                raise RunnerError(f"decompressed {plan.label} image is empty: {source}")
            temporary.replace(runtime)
        except RunnerError:
            temporary.unlink(missing_ok=True)
            raise
        except (OSError, EOFError, lzma.LZMAError) as error:
            temporary.unlink(missing_ok=True)
            raise RunnerError(
                f"could not decompress {plan.label} image {source}: {error}"
            ) from error
    return Drive(path=runtime, mode=plan.mode)


def _resolve_executable(requested: str) -> Path | None:
    resolved = shutil.which(requested)
    if resolved is None and "/" in requested:
        candidate = Path(requested).expanduser().resolve()
        if candidate.is_file():
            resolved = str(candidate)
    return Path(resolved).resolve() if resolved is not None else None


def _qemu_evidence(requested: str) -> dict[str, str | int | None]:
    from .evidence import file_evidence

    resolved = _resolve_executable(requested)
    if resolved is None:
        return {"requested": requested, "path": None, "size_bytes": None, "sha256": None}
    evidence = file_evidence(resolved)
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


def _temporary_destination(destination: Path) -> Path:
    return destination.with_name(f".{destination.name}.tmp")


def _validate_output_destinations(
    outputs: tuple[tuple[str, Path], ...],
    *,
    protected_paths: tuple[Path, ...],
) -> dict[str, Path]:
    resolved: dict[str, Path] = {}
    earlier_outputs: list[Path] = []
    for label, destination in outputs:
        checked = _validate_output_destination(
            destination,
            label=label,
            protected_paths=tuple([*protected_paths, *earlier_outputs]),
        )
        resolved[label] = checked
        earlier_outputs.append(checked)
    return resolved


def run(
    config: RunConfig,
    *,
    console_stream=None,
) -> RunResult:
    """Prepare explicit artifacts and run one architecture without discovery."""

    kernel = config.kernel.expanduser().resolve()
    if not kernel.is_file():
        raise RunnerError(f"kernel does not exist: {kernel}")
    workdir = config.workdir.expanduser().resolve()
    rootfs_mode = _validate_mode("rootfs", config.rootfs_mode)
    extra_mode = _validate_mode("extra-block", config.extra_block_mode)
    network = _validate_network(config)
    input_path = None
    if config.input_path is not None:
        input_path = config.input_path.expanduser().resolve()
        if not input_path.is_file():
            raise RunnerError(f"input file does not exist: {input_path}")
        if not config.interaction.interactive:
            raise RunnerError("input file requires interactive mode")
    if config.receipt_path is not None and input_path is None:
        raise RunnerError("receipt requires an input file")
    effective_qemu_launcher = config.qemu_launcher
    if (
        effective_qemu_launcher is None
        and config.qemu_binary is not None
        and config.qemu_binary.endswith(".py")
    ):
        # The scheduler pinner is a source-controlled Python entry point and
        # intentionally need not carry an executable bit in a checkout.
        effective_qemu_launcher = (sys.executable, config.qemu_binary)

    qemu_requested = config.qemu_binary or "qemu-system-x86_64"
    qemu_executable = _resolve_executable(qemu_requested)
    launcher_executable = (
        _resolve_executable(effective_qemu_launcher[0])
        if effective_qemu_launcher is not None
        else None
    )

    rootfs_plan = _plan_drive(
        config.rootfs,
        mode=rootfs_mode,
        label="rootfs",
        workdir=workdir,
    )
    extra_plan = (
        _plan_drive(
            config.extra_block,
            mode=extra_mode,
            label="extra",
            workdir=workdir,
        )
        if config.extra_block is not None
        else None
    )

    esp_plan = None
    ovmf_code = None
    ovmf_vars_source = None
    ovmf_vars_runtime = None
    if not config.direct_kernel:
        if config.esp is None:
            raise RunnerError(
                "x86_64 UEFI boot requires a GPT ESP; pass --esp or use --direct-kernel"
            )
        esp_plan = _plan_drive(
            config.esp,
            mode="snapshot",
            label="esp",
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
        ovmf_vars_runtime = (workdir / "firmware" / "OVMF_VARS.fd").resolve()

    run_input_paths = [kernel, rootfs_plan.source]
    if input_path is not None:
        run_input_paths.append(input_path)
    if esp_plan is not None:
        run_input_paths.append(esp_plan.source)
    if ovmf_code is not None:
        run_input_paths.append(ovmf_code)
    if ovmf_vars_source is not None:
        run_input_paths.append(ovmf_vars_source)
    if extra_plan is not None:
        run_input_paths.append(extra_plan.source)
    if qemu_executable is not None:
        run_input_paths.append(qemu_executable)
    if launcher_executable is not None:
        run_input_paths.append(launcher_executable)

    planned_outputs: list[tuple[str, Path]] = []
    for plan in (rootfs_plan, extra_plan, esp_plan):
        if plan is not None and plan.compression_suffix is not None:
            planned_outputs.extend(
                [
                    (f"{plan.label} runtime", plan.runtime),
                    (f"{plan.label} runtime temporary", _temporary_destination(plan.runtime)),
                ]
            )
    if ovmf_vars_runtime is not None:
        planned_outputs.extend(
            [
                ("OVMF vars runtime", ovmf_vars_runtime),
                (
                    "OVMF vars runtime temporary",
                    _temporary_destination(ovmf_vars_runtime),
                ),
            ]
        )
    planned_outputs.append(("log", config.log_path))
    if config.receipt_path is not None:
        planned_outputs.append(("receipt", config.receipt_path))
    resolved_outputs = _validate_output_destinations(
        tuple(planned_outputs), protected_paths=tuple(run_input_paths)
    )
    log_path = resolved_outputs["log"]
    receipt_path = (
        resolved_outputs["receipt"] if config.receipt_path is not None else None
    )

    workdir.mkdir(parents=True, exist_ok=True)
    rootfs = _prepare_drive(rootfs_plan)
    extra_block = _prepare_drive(extra_plan) if extra_plan is not None else None
    esp = _prepare_drive(esp_plan) if esp_plan is not None else None
    if ovmf_vars_source is not None and ovmf_vars_runtime is not None:
        ovmf_vars_runtime = _copy_ovmf_vars(ovmf_vars_source, ovmf_vars_runtime)

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
    if qemu_executable is not None and effective_qemu_launcher is None:
        command = (str(qemu_executable), *command[1:])

    rootfs_source_path = rootfs_plan.source
    rootfs_runtime_path = rootfs.path.resolve()
    if config.receipt_path is not None:
        # Receipts are a performance-comparator input.  Keep their evidence
        # capture out of the ordinary correctness path entirely.
        from .evidence import file_evidence
        from .receipt import (
            RECEIPT_SCHEMA_VERSION,
            atomic_write_receipt,
            command_stream_evidence,
            source_identity,
        )

        qemu = _qemu_evidence(qemu_requested)
        rootfs_source = file_evidence(rootfs_source_path)
        rootfs_runtime = (
            rootfs_source.copy()
            if rootfs_runtime_path == rootfs_source_path
            else file_evidence(rootfs_runtime_path)
        )
        receipt = {
            "schema_version": RECEIPT_SCHEMA_VERSION,
            "source_identity": source_identity(),
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
            },
        }
        assert input_path is not None
        input_evidence = command_stream_evidence(input_path)
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
        assert receipt_path is not None

    if input_path is None:
        result = run_process(
            arch=config.arch,
            command=command,
            workdir=workdir,
            log_path=log_path,
            limits=config.limits,
            interaction=config.interaction,
            console_stream=console_stream,
        )
    else:
        with input_path.open("rb") as input_stream:
            result = run_process(
                arch=config.arch,
                command=command,
                workdir=workdir,
                log_path=log_path,
                limits=config.limits,
                interaction=config.interaction,
                input_stream=input_stream,
                console_stream=console_stream,
                capture_input_evidence=receipt_path is not None,
            )
    if receipt_path is not None:
        receipt["rootfs_runtime_after"] = file_evidence(rootfs_runtime_path)
        receipt["log"] = file_evidence(result.log_path)
        if config.extra_block is not None and extra_block is not None:
            receipt["extra_block_runtime_after"] = file_evidence(extra_block.path.resolve())
        from .receipt import command_stream_evidence, input_forwarding_payload

        assert input_path is not None
        assert result.input_forwarding is not None
        receipt["stdin"] = input_forwarding_payload(
            result.input_forwarding,
            source=input_evidence,
            source_unchanged=command_stream_evidence(input_path) == input_evidence,
        )
        receipt.update(
            {
                "state": "recorded",
                "returncode": result.returncode,
                "duration_ms": result.duration_ms,
                "error_message": result.error_message,
                "timed_out": result.timed_out,
                "interrupted": result.interrupted,
                "intentionally_stopped": result.intentionally_stopped,
                "marker_success": result.marker_success,
                "guest_clean_shutdown": result.guest_clean_shutdown,
                "runner_terminated": result.runner_terminated,
                "runner_termination_reason": result.runner_termination_reason,
                # QEMU process exit observes neither device quiescence nor
                # kernel-internal physical retirement.
                "physical_retirement_proven": False,
            }
        )
        atomic_write_receipt(receipt_path, receipt)
    return result
