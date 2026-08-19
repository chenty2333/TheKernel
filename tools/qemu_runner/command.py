"""Architecture-specific QEMU command construction."""

from __future__ import annotations

from pathlib import Path

from .model import Arch, Drive, DriveMode


class CommandError(ValueError):
    """Raised for an invalid QEMU command configuration."""


VALID_DRIVE_MODES = frozenset({"snapshot", "readonly", "rw"})
VALID_NETWORK_MODES = frozenset({"user", "passt", "tap-vhost"})

# Keep the performance block policy in one place.  The baseline lanes use a
# single queue deliberately: the host-side iothread and the guest-side queue
# then describe one measurable serialization point instead of an accidental
# QEMU default that can vary by version.  These are virtio-blk-pci property
# names, not shell arguments, and are appended only to the optional data
# disk; the boot/root filesystem remains compatible with the correctness
# lanes.
PERFORMANCE_BLOCK_PROPERTIES = (
    "num-queues=1",
    "queue-size=128",
    "request-merging=off",
    # Keep guest-side optional discard/write-zeroes offloads out of the
    # latency path.  ioeventfd/event_idx are explicit virtio notification
    # offloads and are supported by the QEMU virtio-blk-pci device.
    "discard=off",
    "write-zeroes=off",
    "ioeventfd=on",
    "event_idx=on",
)


def _escaped_path(path: Path) -> str:
    # QEMU keyval syntax escapes a literal comma as a doubled comma.
    return str(path).replace(",", ",,")


def drive_options(
    path: Path,
    drive_id: str,
    *,
    mode: DriveMode,
    cache_none: bool = False,
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
    if cache_none:
        options += ",cache=none"
    return options


def _append_pci_drive(
    command: list[str],
    drive: Drive,
    drive_id: str,
    *,
    iothread_id: str | None = None,
    performance: bool = False,
) -> None:
    device = f"virtio-blk-pci,drive={drive_id}"
    if iothread_id is not None:
        device += f",iothread={iothread_id}"
    if performance:
        device += "," + ",".join(PERFORMANCE_BLOCK_PROPERTIES)
    command.extend(
        [
            "-drive",
            drive_options(
                drive.path,
                drive_id,
                mode=drive.mode,
                cache_none=performance,
            ),
            "-device",
            device,
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
    qemu_launcher: tuple[str, ...] | None = None,
    accel: str | None = None,
    cpu: str | None = None,
    iothread_id: str | None = None,
    network: str = "user",
    network_mode: str | None = None,
    network_topology: str | None = None,
    tap_name: str | None = None,
    extra_args: tuple[str, ...] = (),
) -> tuple[str, ...]:
    """Build the deterministic architecture-specific QEMU topology."""

    if cpus <= 0:
        raise CommandError("CPU count must be positive")
    if not memory:
        raise CommandError("memory size must not be empty")
    if network_mode is not None:
        if network != "user" and network != network_mode:
            raise CommandError("network and network_mode disagree")
        network = network_mode
    if network_topology is not None:
        if network != "user" and network != network_topology:
            raise CommandError("network and network_topology disagree")
        network = network_topology
    if network not in VALID_NETWORK_MODES:
        raise CommandError(f"unsupported network topology: {network}")
    if tap_name is not None and (not tap_name or any(char in tap_name for char in ",=\n\r")):
        raise CommandError("tap name must be a non-empty QEMU-safe value")
    if qemu_launcher is not None and (
        not qemu_launcher or any(not item for item in qemu_launcher)
    ):
        raise CommandError("qemu launcher must contain non-empty argv entries")
    qemu_argv = list(qemu_launcher or (qemu_binary or "qemu-system-x86_64",))
    if arch == "x86_64":
        if direct_kernel:
            command = [
                *qemu_argv,
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
                *qemu_argv,
                "-machine",
                "q35",
                "-drive",
                f"if=pflash,format=raw,readonly=on,aio=threads,file={_escaped_path(ovmf_code)}",
                "-drive",
                f"if=pflash,format=raw,aio=threads,file={_escaped_path(ovmf_vars)}",
                "-drive",
                f"file={_escaped_path(esp.path)},if=ide,format=raw,snapshot=on,aio=threads",
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
        if accel is not None:
            command.extend(["-accel", accel])
        if cpu is not None:
            command.extend(["-cpu", cpu])
        if iothread_id is not None:
            command.extend(["-object", f"iothread,id={iothread_id}"])
        _append_pci_drive(command, rootfs, "rootfs", iothread_id=iothread_id)
        if network == "user":
            netdev = "user,id=net0"
        elif network == "passt":
            # QEMU's passt backend starts a rootless passt helper.  No tap
            # device, uid 0, or host-side setup is implied by this topology.
            netdev = "passt,id=net0"
        else:
            netdev = "tap,id=net0,vhost=on"
            if tap_name is not None:
                netdev += f",ifname={tap_name}"
        command.extend(
            [
                "-no-reboot",
                "-device",
                "virtio-net-pci,netdev=net0",
                "-netdev",
                netdev,
                "-rtc",
                "base=utc",
            ]
        )
        if extra_block is not None:
            _append_pci_drive(
                command,
                extra_block,
                "extra",
                iothread_id=iothread_id,
                performance=True,
            )
        if extra_args:
            command.extend(extra_args)
        return tuple(command)

    raise CommandError(f"unsupported architecture: {arch}")
