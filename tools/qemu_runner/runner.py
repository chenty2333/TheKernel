"""Composition layer for explicit images, QEMU topology, and process control."""

from __future__ import annotations

import gzip
import lzma
import os
import re
import shutil
import stat
import subprocess
from dataclasses import replace
from dataclasses import dataclass
from pathlib import Path

from .command import VALID_DRIVE_MODES, build_qemu_command
from .model import (
    Arch,
    Drive,
    DriveMode,
    GraphicsProfile,
    Interaction,
    QmpControls,
    RunLimits,
    RunResult,
    RootfsTransport,
)
from .process import run_process


class RunnerError(ValueError):
    """Raised for an invalid product-run configuration."""


_QEMU_DEVICE_HELP_NAME = re.compile(r'^\s*name "([^\"]+)"', re.MULTILINE)
_QEMU_DEVICE_PROPERTY = re.compile(r"^\s*([A-Za-z][A-Za-z0-9_-]*)=", re.MULTILINE)


def _parse_qemu_device_help(output: str) -> frozenset[str]:
    """Return device names from QEMU's stable ``-device help`` listing."""

    return frozenset(_QEMU_DEVICE_HELP_NAME.findall(output))


def _parse_qemu_display_help(output: str) -> frozenset[str]:
    """Return display backend names from QEMU's ``-display help`` listing."""

    lines = iter(output.splitlines())
    for line in lines:
        if line.strip() == "Available display backend types:":
            break
    else:
        return frozenset()
    backends: set[str] = set()
    for line in lines:
        name = line.strip()
        if not name:
            break
        backends.add(name.split(",", 1)[0])
    return frozenset(backends)


def _qemu_device_help(qemu: Path) -> frozenset[str]:
    return _parse_qemu_device_help(_qemu_help_output(qemu, "-device", "help"))


def _qemu_device_properties(qemu: Path, device: str) -> frozenset[str]:
    """Return property names from QEMU's per-device help listing."""

    return frozenset(_QEMU_DEVICE_PROPERTY.findall(_qemu_help_output(qemu, "-device", f"{device},help")))


def _qemu_help_output(qemu: Path, *arguments: str) -> str:
    try:
        completed = subprocess.run(
            [str(qemu), *arguments],
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            check=False,
            timeout=5,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise RunnerError(
            f"could not inspect QEMU graphics capabilities with {qemu}: {error}"
        ) from error
    if completed.returncode != 0:
        raise RunnerError(
            f"could not inspect QEMU graphics capabilities with {qemu}: "
            f"{' '.join(arguments)} exited {completed.returncode}"
        )
    return completed.stdout


def _probe_virgl_headless(qemu: Path) -> None:
    """Parse and initialize the headless GL topology without booting a guest."""

    command = [
        str(qemu),
        "-machine",
        "q35",
        "-nodefaults",
        "-S",
        "-display",
        "egl-headless,gl=on",
        "-device",
        "virtio-gpu-gl-pci,max_outputs=1,xres=800,yres=600",
    ]
    try:
        process = subprocess.Popen(
            command,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            text=True,
        )
    except OSError as error:
        raise RunnerError(f"could not start virgl-headless capability probe: {error}") from error
    try:
        _stdout, stderr = process.communicate(timeout=0.5)
    except subprocess.TimeoutExpired:
        process.terminate()
        try:
            process.communicate(timeout=2)
        except subprocess.TimeoutExpired:
            process.kill()
            process.communicate()
        return
    detail = (stderr or "").strip()
    lowered = detail.lower()
    if "invalid parameter" in lowered or "invalid option" in lowered:
        raise RunnerError(
            "SKIP: QEMU rejected virgl-headless argv egl-headless,gl=on: "
            f"{detail or f'exit {process.returncode}'}"
        )
    raise RunnerError(
        "SKIP: QEMU virgl-headless EGL runtime is unavailable: "
        f"{detail or f'exit {process.returncode}'}"
    )


def _validate_virgl_capabilities(profile: GraphicsProfile, qemu: Path | None) -> None:
    """Fail before launch when the explicitly requested virgl topology is absent."""

    if profile not in {"virgl-headless", "virgl-interactive"}:
        return
    if qemu is None:
        raise RunnerError("virgl profile requires a resolvable QEMU executable for capability checks")
    devices = _parse_qemu_device_help(_qemu_help_output(qemu, "-device", "help"))
    displays = _parse_qemu_display_help(_qemu_help_output(qemu, "-display", "help"))
    if "virtio-gpu-gl-pci" not in devices:
        raise RunnerError("SKIP: QEMU does not provide required virgl device virtio-gpu-gl-pci")
    backend = "egl-headless" if profile == "virgl-headless" else "gtk"
    if backend not in displays:
        raise RunnerError(f"SKIP: QEMU does not provide required virgl display backend {backend}")
    if profile == "virgl-headless":
        _probe_virgl_headless(qemu)


def _validate_venus_capabilities(profile: GraphicsProfile, qemu: Path | None) -> None:
    """Reject an unavailable Venus topology before launching the guest.

    This only establishes that the host QEMU exposes the requested device
    properties.  The guest Vulkan smoke remains the authority for the kernel
    blob/context-init lifecycle and must not be inferred from this probe.
    """

    if profile != "venus-interactive":
        return
    if qemu is None:
        raise RunnerError("venus profile requires a resolvable QEMU executable for capability checks")
    devices = _qemu_device_help(qemu)
    if "virtio-gpu-gl-pci" not in devices:
        raise RunnerError("SKIP: QEMU does not provide required Venus device virtio-gpu-gl-pci")
    properties = _qemu_device_properties(qemu, "virtio-gpu-gl-pci")
    missing = [
        property_name
        for property_name in ("blob", "venus", "hostmem", "max_hostmem", "xres", "yres")
        if property_name not in properties
    ]
    if missing:
        raise RunnerError(
            "SKIP: QEMU virtio-gpu-gl-pci lacks required Venus properties: " + ", ".join(missing)
        )


@dataclass(frozen=True)
class RunConfig:
    arch: Arch
    kernel: Path
    rootfs: Path | None
    workdir: Path
    log_path: Path
    esp: Path | None = None
    rootfs_mode: DriveMode = "snapshot"
    # The generic runner remains useful for standalone drive-backed tests.
    # `tools/thekernel.py` fixes ordinary product boots to the module path;
    # graphics benchmarks use ``module-and-drive`` to compare against Linux
    # with an identical snapshot VirtIO rootfs topology.
    rootfs_transport: RootfsTransport = "drive"
    extra_block: Path | None = None
    extra_block_mode: DriveMode = "rw"
    limits: RunLimits = RunLimits()
    interaction: Interaction = Interaction()
    memory: str = "1G"
    cpus: int = 1
    qemu_binary: str | None = None
    accel: str | None = None
    graphics_profile: GraphicsProfile = "headless"
    graphics_width: int = 800
    graphics_height: int = 600
    qmp: QmpControls = QmpControls()
    extra_args: tuple[str, ...] = ()
    ovmf_code: Path | None = None
    ovmf_vars: Path | None = None
    direct_kernel: bool = False
    input_path: Path | None = None


@dataclass(frozen=True)
class _DrivePlan:
    source: Path
    runtime: Path
    mode: DriveMode
    label: str
    compression_suffix: str | None


def _initrd_from_extra_args(extra_args: tuple[str, ...]) -> Path | None:
    """Extract the only supported file-valued initrd option fail-closed."""

    positions = [index for index, value in enumerate(extra_args) if value == "-initrd"]
    if not positions:
        return None
    if len(positions) != 1:
        raise RunnerError("extra_args contains repeated -initrd")
    index = positions[0]
    if index + 1 >= len(extra_args) or extra_args[index + 1].startswith("-"):
        raise RunnerError("-initrd requires exactly one file path")
    path = Path(extra_args[index + 1]).expanduser().resolve()
    if not path.is_file():
        raise RunnerError(f"initrd does not exist: {path}")
    return path


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
        raw = next(
            (
                path
                for candidate in candidates
                if (path := Path(candidate)).is_file()
            ),
            None,
        )
    if raw is None:
        option = environment.removeprefix("THEKERNEL_").lower().replace("_", "-")
        raise RunnerError(
            f"x86_64 q35 requires {label}; pass --{option} or set {environment}"
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


def _open_qemu_input(
    path: Path,
    *,
    label: str,
    writable: bool = False,
) -> tuple[int, Path]:
    """Open one validated input once and expose that exact object to QEMU."""

    access = os.O_RDWR if writable else os.O_RDONLY
    flags = access | os.O_CLOEXEC | getattr(os, "O_NOFOLLOW", 0)
    try:
        fd = os.open(path, flags)
    except OSError as error:
        raise RunnerError(f"could not open {label} input {path}: {error}") from error
    try:
        if not stat.S_ISREG(os.fstat(fd).st_mode):
            raise RunnerError(f"{label} input is not a regular file: {path}")
        if os.fstat(fd).st_size == 0:
            raise RunnerError(f"{label} input is empty: {path}")
    except Exception:
        os.close(fd)
        raise
    return fd, Path(f"/proc/self/fd/{fd}")


def _initrd_args_with_path(extra_args: tuple[str, ...], path: Path | None) -> tuple[str, ...]:
    if path is None:
        return extra_args
    index = extra_args.index("-initrd")
    rewritten = list(extra_args)
    rewritten[index + 1] = str(path)
    return tuple(rewritten)


def _resolve_executable(requested: str) -> Path | None:
    resolved = shutil.which(requested)
    if resolved is None and "/" in requested:
        candidate = Path(requested).expanduser().resolve()
        if candidate.is_file():
            resolved = str(candidate)
    return Path(resolved).resolve() if resolved is not None else None


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
    initrd = _initrd_from_extra_args(config.extra_args)
    input_path = None
    if config.input_path is not None:
        input_path = config.input_path.expanduser().resolve()
        if not input_path.is_file():
            raise RunnerError(f"input file does not exist: {input_path}")
        if not config.interaction.interactive:
            raise RunnerError("input file requires interactive mode")
    qmp_socket = config.qmp.socket
    if (
        config.qmp.screenshot is not None
        or config.qmp.input_events
        or config.qmp.input_after_marker is not None
        or config.qmp.screenshot_after_marker is not None
        or config.qmp.checkpoints
    ) and qmp_socket is None:
        raise RunnerError("QMP screenshot and input injection require a QMP socket")
    if qmp_socket is not None:
        qmp_socket = qmp_socket.expanduser().resolve()
        if qmp_socket.exists():
            raise RunnerError(f"QMP socket already exists: {qmp_socket}")
    screenshot = (
        config.qmp.screenshot.expanduser().resolve()
        if config.qmp.screenshot is not None
        else None
    )
    checkpoints = tuple(
        replace(
            checkpoint,
            screenshot=(
                checkpoint.screenshot.expanduser().resolve()
                if checkpoint.screenshot is not None
                else None
            ),
        )
        for checkpoint in config.qmp.checkpoints
    )

    qemu_requested = config.qemu_binary or "qemu-system-x86_64"
    qemu_executable = _resolve_executable(qemu_requested)
    _validate_virgl_capabilities(config.graphics_profile, qemu_executable)
    _validate_venus_capabilities(config.graphics_profile, qemu_executable)

    if config.rootfs_transport not in {"drive", "module", "module-and-drive"}:
        raise RunnerError(f"unsupported rootfs transport: {config.rootfs_transport}")
    rootfs_plan = (
        _plan_drive(
            config.rootfs,
            mode=rootfs_mode,
            label="rootfs",
            workdir=workdir,
        )
        if config.rootfs is not None
        and config.rootfs_transport in {"drive", "module-and-drive"}
        else None
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

    run_input_paths = [kernel]
    if rootfs_plan is not None:
        run_input_paths.append(rootfs_plan.source)
    if input_path is not None:
        run_input_paths.append(input_path)
    if initrd is not None:
        run_input_paths.append(initrd)
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
    if qmp_socket is not None:
        planned_outputs.append(("QMP socket", qmp_socket))
    if screenshot is not None:
        planned_outputs.append(("screenshot", screenshot))
    for index, checkpoint in enumerate(checkpoints):
        if checkpoint.screenshot is not None:
            planned_outputs.append((f"QMP checkpoint screenshot {index}", checkpoint.screenshot))
    resolved_outputs = _validate_output_destinations(
        tuple(planned_outputs), protected_paths=tuple(run_input_paths)
    )
    log_path = resolved_outputs["log"]

    workdir.mkdir(parents=True, exist_ok=True)
    if qmp_socket is not None:
        qmp_socket.parent.mkdir(parents=True, exist_ok=True)
    if screenshot is not None:
        screenshot.parent.mkdir(parents=True, exist_ok=True)
    for checkpoint in checkpoints:
        if checkpoint.screenshot is not None:
            checkpoint.screenshot.parent.mkdir(parents=True, exist_ok=True)
    rootfs = _prepare_drive(rootfs_plan) if rootfs_plan is not None else None
    extra_block = _prepare_drive(extra_plan) if extra_plan is not None else None
    esp = _prepare_drive(esp_plan) if esp_plan is not None else None
    if ovmf_vars_source is not None and ovmf_vars_runtime is not None:
        ovmf_vars_runtime = _copy_ovmf_vars(ovmf_vars_source, ovmf_vars_runtime)

    opened_fds: list[int] = []
    try:
        kernel_fd, qemu_kernel = _open_qemu_input(kernel, label="kernel")
        opened_fds.append(kernel_fd)
        qemu_rootfs = None
        if rootfs is not None:
            rootfs_fd, qemu_rootfs_path = _open_qemu_input(
                rootfs.path,
                label="rootfs",
                writable=rootfs.mode == "rw",
            )
            opened_fds.append(rootfs_fd)
            qemu_rootfs = Drive(path=qemu_rootfs_path, mode=rootfs.mode)
        qemu_extra_block = None
        if extra_block is not None:
            extra_fd, qemu_extra_path = _open_qemu_input(
                extra_block.path,
                label="extra",
                writable=extra_block.mode == "rw",
            )
            opened_fds.append(extra_fd)
            qemu_extra_block = Drive(path=qemu_extra_path, mode=extra_block.mode)
        qemu_esp = None
        if esp is not None:
            esp_fd, qemu_esp_path = _open_qemu_input(esp.path, label="esp")
            opened_fds.append(esp_fd)
            qemu_esp = Drive(path=qemu_esp_path, mode=esp.mode)
        qemu_ovmf_code = None
        if ovmf_code is not None:
            ovmf_code_fd, qemu_ovmf_code = _open_qemu_input(
                ovmf_code, label="OVMF code"
            )
            opened_fds.append(ovmf_code_fd)
        qemu_ovmf_vars = None
        if ovmf_vars_runtime is not None:
            ovmf_vars_fd, qemu_ovmf_vars = _open_qemu_input(
                ovmf_vars_runtime,
                label="OVMF vars",
                writable=True,
            )
            opened_fds.append(ovmf_vars_fd)
        qemu_initrd = None
        if initrd is not None:
            initrd_fd, qemu_initrd = _open_qemu_input(initrd, label="initrd")
            opened_fds.append(initrd_fd)

        command = build_qemu_command(
            arch=config.arch,
            kernel=qemu_kernel,
            rootfs=qemu_rootfs,
            extra_block=qemu_extra_block,
            esp=qemu_esp,
            ovmf_code=qemu_ovmf_code,
            ovmf_vars=qemu_ovmf_vars,
            direct_kernel=config.direct_kernel,
            memory=config.memory,
            cpus=config.cpus,
            qemu_binary=config.qemu_binary,
            accel=config.accel,
            graphics_profile=config.graphics_profile,
            graphics_width=config.graphics_width,
            graphics_height=config.graphics_height,
            qmp_socket=qmp_socket,
            extra_args=_initrd_args_with_path(config.extra_args, qemu_initrd),
        )
        if qemu_executable is not None:
            command = (str(qemu_executable), *command[1:])

        if input_path is None:
            return run_process(
                arch=config.arch,
                command=command,
                workdir=workdir,
                log_path=log_path,
                limits=config.limits,
                interaction=config.interaction,
                console_stream=console_stream,
                pass_fds=tuple(opened_fds),
                qmp_socket=qmp_socket,
                screenshot=screenshot,
                qmp_input_events=config.qmp.input_events,
                qmp_input_after_marker=config.qmp.input_after_marker,
                qmp_screenshot_after_marker=config.qmp.screenshot_after_marker,
                qmp_timeout_secs=config.qmp.timeout_secs,
                qmp_screenshot_size=config.qmp.screenshot_size,
                qmp_screenshot_color_blocks=config.qmp.screenshot_color_blocks,
                qmp_screenshot_region_crcs=config.qmp.screenshot_region_crcs,
                qmp_checkpoints=checkpoints,
            )
        with input_path.open("rb") as input_stream:
            return run_process(
                arch=config.arch,
                command=command,
                workdir=workdir,
                log_path=log_path,
                limits=config.limits,
                interaction=config.interaction,
                input_stream=input_stream,
                console_stream=console_stream,
                pass_fds=tuple(opened_fds),
                qmp_socket=qmp_socket,
                screenshot=screenshot,
                qmp_input_events=config.qmp.input_events,
                qmp_input_after_marker=config.qmp.input_after_marker,
                qmp_screenshot_after_marker=config.qmp.screenshot_after_marker,
                qmp_timeout_secs=config.qmp.timeout_secs,
                qmp_screenshot_size=config.qmp.screenshot_size,
                qmp_screenshot_color_blocks=config.qmp.screenshot_color_blocks,
                qmp_screenshot_region_crcs=config.qmp.screenshot_region_crcs,
                qmp_checkpoints=checkpoints,
            )
    finally:
        for fd in reversed(opened_fds):
            os.close(fd)
