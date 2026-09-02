"""Shared fixed QMP benchmark protocol for TheKernel and Linux oracle runs."""

from __future__ import annotations

from .model import QmpCheckpoint, QmpPciHotplug

BENCHMARK_READY_MARKER = "THEKERNEL_GRAPHICS_BENCHMARK_READY"
BENCHMARK_INPUT_HOTPLUG_REMOVED_MARKER = "THEKERNEL_GRAPHICS_INPUT_HOTPLUG_REMOVED"
BENCHMARK_INPUT_HOTPLUG_READY_MARKER = "THEKERNEL_GRAPHICS_INPUT_HOTPLUG_READY"
BENCHMARK_COMPLETE_MARKER = "THEKERNEL_GRAPHICS_BENCHMARK_COMPLETE"
BENCHMARK_INPUT_SAMPLES = 10
BENCHMARK_RENDERER_PREFIX = "THEKERNEL_GRAPHICS_RENDERER "
BENCHMARK_FAULTS = frozenset(
    {"modeset", "client-crash", "vt-switch", "weston-restart", "input-hotplug"}
)


def renderer_for_profile(profile: str) -> str:
    """Return the only guest renderer accepted for a fixed QEMU topology."""

    if profile == "headless":
        return "software"
    if profile in {"virgl-headless", "virgl-interactive"}:
        return "virgl"
    if profile == "venus-interactive":
        return "venus"
    raise ValueError(f"unsupported graphics benchmark profile: {profile}")


def benchmark_checkpoints(fault: str | None = None) -> tuple[QmpCheckpoint, ...]:
    """Return the marker-gated input sequence shared by both kernels.

    The guest benchmark owns non-hotplug fault injection through its rootfs.
    QMP only performs the one topology mutation that cannot be expressed from
    userspace, keeping the Linux reference and product event ordering equal.
    """

    if fault is not None and fault not in BENCHMARK_FAULTS:
        raise ValueError(f"unsupported graphics benchmark fault: {fault}")
    keyboard_events = ((
        {"type": "key", "data": {"down": True, "key": {"type": "qcode", "data": "a"}}},
        {"type": "key", "data": {"down": False, "key": {"type": "qcode", "data": "a"}}},
    ),)

    def latency_samples(first_marker: str, *, tablet: bool = False) -> tuple[QmpCheckpoint, ...]:
        checkpoints: list[QmpCheckpoint] = []
        for index in range(BENCHMARK_INPUT_SAMPLES):
            visible = f"THEKERNEL_GRAPHICS_INPUT_VISIBLE_{index:03d}"
            input_events = keyboard_events
            if tablet:
                # Alternate absolute coordinates so every sample traverses
                # the replacement tablet's evdev -> libinput pointer path.
                input_events = ((
                    {"type": "abs", "data": {"axis": "x", "value": 320 + index % 2}},
                    {"type": "abs", "data": {"axis": "y", "value": 240 + index % 2}},
                ),)
            checkpoints.append(QmpCheckpoint(
                input_after_marker=(
                    first_marker
                    if index == 0
                    else f"THEKERNEL_GRAPHICS_INPUT_VISIBLE_{index - 1:03d}"
                ),
                input_events=input_events,
                latency_after_marker=visible,
                latency_index=index,
            ))
        return tuple(checkpoints)
    if fault == "input-hotplug":
        # Do not race input injection with PCI enumeration.  The guest starts
        # its observer before BENCHMARK_READY, confirms the old event node was
        # removed, then confirms the replacement reached both eudev and
        # Weston's libinput fd set before permitting injection.
        return (
            QmpCheckpoint(
                input_after_marker=BENCHMARK_READY_MARKER,
                pci_hotplug=(QmpPciHotplug(action="del", device_id="input-tablet"),),
            ),
            QmpCheckpoint(
                input_after_marker=BENCHMARK_INPUT_HOTPLUG_REMOVED_MARKER,
                pci_hotplug=(
                    QmpPciHotplug(
                        action="add",
                        device_id="input-tablet",
                        driver="virtio-tablet-pci",
                        bus="rp-input-tablet",
                    ),
                ),
            ),
        ) + latency_samples(BENCHMARK_INPUT_HOTPLUG_READY_MARKER, tablet=True)
    return latency_samples(BENCHMARK_READY_MARKER)
