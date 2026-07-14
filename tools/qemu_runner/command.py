"""Architecture-specific QEMU command construction."""

from __future__ import annotations

from pathlib import Path

from .model import Arch, Drive, DriveMode


class CommandError(ValueError):
    """Raised for an invalid QEMU command configuration."""


VALID_DRIVE_MODES = frozenset({"snapshot", "readonly", "rw"})


def _escaped_path(path: Path) -> str:
    # QEMU keyval syntax escapes a literal comma as a doubled comma.
    return str(path).replace(",", ",,")


def drive_options(path: Path, drive_id: str, *, mode: DriveMode) -> str:
    if mode not in VALID_DRIVE_MODES:
        raise CommandError(f"unsupported drive mode: {mode}")
    options = f"file={_escaped_path(path)},if=none,format=raw,id={drive_id}"
    if mode == "snapshot":
        return f"{options},snapshot=on"
    if mode == "readonly":
        return f"{options},readonly=on"
    return options


def _append_rv_drive(command: list[str], drive: Drive, drive_id: str, bus_index: int) -> None:
    command.extend(
        [
            "-drive",
            drive_options(drive.path, drive_id, mode=drive.mode),
            "-device",
            f"virtio-blk-device,drive={drive_id},bus=virtio-mmio-bus.{bus_index}",
        ]
    )


def _append_la_drive(command: list[str], drive: Drive, drive_id: str) -> None:
    command.extend(
        [
            "-drive",
            drive_options(drive.path, drive_id, mode=drive.mode),
            "-device",
            f"virtio-blk-pci,drive={drive_id}",
        ]
    )


def build_qemu_command(
    *,
    arch: Arch,
    kernel: Path,
    rootfs: Drive,
    extra_block: Drive | None = None,
    memory: str = "1G",
    cpus: int = 1,
    qemu_binary: str | None = None,
) -> tuple[str, ...]:
    """Build the deterministic RV/LA QEMU topology used by product tests."""

    if cpus <= 0:
        raise CommandError("CPU count must be positive")
    if not memory:
        raise CommandError("memory size must not be empty")

    if arch == "rv":
        command = [
            qemu_binary or "qemu-system-riscv64",
            "-machine",
            "virt",
            "-kernel",
            str(kernel),
            "-m",
            memory,
            "-nographic",
            "-smp",
            str(cpus),
            "-bios",
            "default",
            "-object",
            "rng-random,filename=/dev/urandom,id=rng0",
            "-device",
            "virtio-rng-device,rng=rng0,bus=virtio-mmio-bus.7",
        ]
        _append_rv_drive(command, rootfs, "rootfs", 0)
        if extra_block is not None:
            _append_rv_drive(command, extra_block, "extra", 1)
        command.extend(
            [
                "-no-reboot",
                "-device",
                "virtio-net-device,netdev=net0",
                "-netdev",
                "user,id=net0",
                "-rtc",
                "base=utc",
            ]
        )
        return tuple(command)

    if arch != "la":
        raise CommandError(f"unsupported architecture: {arch}")

    command = [
        qemu_binary or "qemu-system-loongarch64",
        "-machine",
        "virt",
        "-kernel",
        str(kernel),
        "-m",
        memory,
        "-nographic",
        "-smp",
        str(cpus),
        "-object",
        "rng-random,filename=/dev/urandom,id=rng0",
        "-device",
        "virtio-rng-pci,rng=rng0",
    ]
    _append_la_drive(command, rootfs, "rootfs")
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
        _append_la_drive(command, extra_block, "extra")
    return tuple(command)
