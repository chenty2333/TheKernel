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


def _append_pci_drive(command: list[str], drive: Drive, drive_id: str) -> None:
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
    esp: Drive | None = None,
    ovmf_code: Path | None = None,
    ovmf_vars: Path | None = None,
    direct_kernel: bool = False,
    memory: str = "1G",
    cpus: int = 1,
    qemu_binary: str | None = None,
) -> tuple[str, ...]:
    """Build the deterministic architecture-specific QEMU topology."""

    if cpus <= 0:
        raise CommandError("CPU count must be positive")
    if not memory:
        raise CommandError("memory size must not be empty")
    if arch == "x86_64":
        if direct_kernel:
            command = [
                qemu_binary or "qemu-system-x86_64",
                "-machine",
                "q35",
                "-kernel",
                str(kernel),
                "-m",
                memory,
                "-nographic",
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
                qemu_binary or "qemu-system-x86_64",
                "-machine",
                "q35",
                "-drive",
                f"if=pflash,format=raw,readonly=on,file={_escaped_path(ovmf_code)}",
                "-drive",
                f"if=pflash,format=raw,file={_escaped_path(ovmf_vars)}",
                "-drive",
                f"file={_escaped_path(esp.path)},if=ide,format=raw,snapshot=on",
                "-m",
                memory,
                "-nographic",
                "-smp",
                str(cpus),
            ]
        command.extend(
            [
                "-object",
                "rng-random,filename=/dev/urandom,id=rng0",
                "-device",
                "virtio-rng-pci,rng=rng0",
            ]
        )
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
        return tuple(command)

    raise CommandError(f"unsupported architecture: {arch}")
