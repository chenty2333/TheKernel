"""Architecture-specific QEMU command construction."""

from __future__ import annotations

from pathlib import Path

from .model import Arch, Drive, DriveMode, GraphicsProfile
from .profiles import GRAPHICS_PROFILES, graphics_device


class CommandError(ValueError):
    """Raised for an invalid QEMU command configuration."""


VALID_DRIVE_MODES = frozenset({"snapshot", "readonly", "rw"})
Q35_MACHINE = "q35,max-ram-below-4g=2G"
_ACCEL_CPU_MODELS = {"kvm": "host", "tcg": "max"}
_RUNNER_OWNED_OPTIONS = frozenset({"-accel", "-cpu"})


def _escaped_path(path: Path) -> str:
    # QEMU keyval syntax escapes a literal comma as a doubled comma.
    return str(path).replace(",", ",,")


def _validate_extra_args(extra_args: tuple[str, ...]) -> None:
    """Keep runner-owned CPU and accelerator selections non-overridable."""

    for argument in extra_args:
        option = argument.split("=", 1)[0]
        if option in _RUNNER_OWNED_OPTIONS:
            raise CommandError(f"extra_args must not override runner-owned {option}")


def drive_options(
    path: Path,
    drive_id: str,
    *,
    mode: DriveMode,
) -> str:
    if mode not in VALID_DRIVE_MODES:
        raise CommandError(f"unsupported drive mode: {mode}")
    # Keep the host-side storage policy reproducible.  QEMU's automatic AIO
    # selection can choose io_uring and then fail during setup under otherwise
    # valid locked-memory/resource limits, before the guest even starts.
    options = f"file={_escaped_path(path)},if=none,format=raw,id={drive_id}"
    if mode == "snapshot":
        options += ",snapshot=on"
    elif mode == "readonly":
        options += ",readonly=on"
    options += ",aio=threads"
    return options


def _append_pci_drive(
    command: list[str],
    drive: Drive,
    drive_id: str,
) -> None:
    device = f"virtio-blk-pci,drive={drive_id}"
    command.extend(
        [
            "-drive",
            drive_options(drive.path, drive_id, mode=drive.mode),
            "-device",
            device,
        ]
    )


def build_qemu_command(
    *,
    arch: Arch,
    kernel: Path,
    rootfs: Drive | None,
    extra_block: Drive | None = None,
    esp: Drive | None = None,
    ovmf_code: Path | None = None,
    ovmf_vars: Path | None = None,
    direct_kernel: bool = False,
    memory: str = "1G",
    cpus: int = 1,
    qemu_binary: str | None = None,
    accel: str | None = None,
    graphics_profile: GraphicsProfile = "headless",
    graphics_width: int = 800,
    graphics_height: int = 600,
    qmp_socket: Path | None = None,
    extra_args: tuple[str, ...] = (),
) -> tuple[str, ...]:
    """Build the deterministic architecture-specific QEMU topology."""

    if cpus <= 0:
        raise CommandError("CPU count must be positive")
    if not memory:
        raise CommandError("memory size must not be empty")
    if graphics_profile not in GRAPHICS_PROFILES:
        raise CommandError(f"unsupported graphics profile: {graphics_profile}")
    if graphics_width <= 0 or graphics_height <= 0:
        raise CommandError("graphics dimensions must be positive")
    if qmp_socket is not None and (
        not str(qmp_socket) or any(char in str(qmp_socket) for char in ",\n\r")
    ):
        raise CommandError("QMP socket path must be QEMU-safe")
    _validate_extra_args(extra_args)
    qemu_argv = [qemu_binary or "qemu-system-x86_64"]
    if arch == "x86_64":
        if direct_kernel:
            command = [
                *qemu_argv,
                "-machine",
                Q35_MACHINE,
                "-kernel",
                str(kernel),
                "-m",
                memory,
                "-smp",
                str(cpus),
            ]
        else:
            if esp is None:
                raise CommandError(
                    "x86_64 UEFI boot requires a GPT ESP; pass --esp or use --direct-kernel"
                )
            if ovmf_code is None or ovmf_vars is None:
                raise CommandError(
                    "x86_64 UEFI boot requires OVMF code and writable vars images"
                )
            if esp.mode != "snapshot":
                raise CommandError("x86_64 ESP must use snapshot mode")
            command = [
                *qemu_argv,
                "-machine",
                Q35_MACHINE,
                "-drive",
                f"if=pflash,format=raw,readonly=on,aio=threads,file={_escaped_path(ovmf_code)}",
                "-drive",
                f"if=pflash,format=raw,aio=threads,file={_escaped_path(ovmf_vars)}",
                "-drive",
                f"file={_escaped_path(esp.path)},if=ide,format=raw,snapshot=on,aio=threads",
                "-m",
                memory,
                "-smp",
                str(cpus),
            ]
        command.extend(
            [
                # q35's defaults include VGA and PS/2 input.  Define the
                # entire guest-visible graphics/input topology instead.
                "-nodefaults",
                "-serial",
                "stdio",
                "-display",
                GRAPHICS_PROFILES[graphics_profile].display,
                "-device",
                graphics_device(graphics_profile, graphics_width, graphics_height),
                "-device",
                "pcie-root-port,id=rp-input-kbd,slot=2,chassis=2",
                "-device",
                "virtio-keyboard-pci,id=input-kbd,bus=rp-input-kbd",
                "-device",
                "pcie-root-port,id=rp-input-mouse,slot=3,chassis=3",
                "-device",
                "virtio-mouse-pci,id=input-mouse,bus=rp-input-mouse",
                "-device",
                "pcie-root-port,id=rp-input-tablet,slot=4,chassis=4",
                "-device",
                "virtio-tablet-pci,id=input-tablet,bus=rp-input-tablet",
                "-object",
                "rng-random,filename=/dev/urandom,id=rng0",
                "-device",
                "virtio-rng-pci,rng=rng0",
            ]
        )
        if accel is not None:
            command.extend(["-accel", accel])
            cpu_model = _ACCEL_CPU_MODELS.get(accel)
            if cpu_model is not None:
                command.extend(["-cpu", cpu_model])
        if qmp_socket is not None:
            command.extend(["-qmp", f"unix:{qmp_socket},server=on,wait=off"])
        if rootfs is not None:
            _append_pci_drive(command, rootfs, "rootfs")
        command.extend(
            [
                "-no-reboot",
                "-device",
                "virtio-net-pci,netdev=net0",
                "-netdev",
                "user,id=net0",
                "-rtc",
                "base=utc",
            ]
        )
        if extra_block is not None:
            _append_pci_drive(command, extra_block, "extra")
        if extra_args:
            command.extend(extra_args)
        return tuple(command)

    raise CommandError(f"unsupported architecture: {arch}")
