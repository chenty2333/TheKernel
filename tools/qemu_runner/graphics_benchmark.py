"""Shared fixed QMP benchmark protocol for TheKernel and Linux oracle runs."""

from __future__ import annotations

import argparse
import json

from .model import QmpCheckpoint, QmpPciHotplug
from .profiles import BENCHMARK_FAULTS, GRAPHICS_PROFILES, INPUT_SAMPLES

BENCHMARK_READY_MARKER = "THEKERNEL_GRAPHICS_BENCHMARK_READY"
BENCHMARK_INPUT_HOTPLUG_REMOVED_MARKER = "THEKERNEL_GRAPHICS_INPUT_HOTPLUG_REMOVED"
BENCHMARK_INPUT_HOTPLUG_READY_MARKER = "THEKERNEL_GRAPHICS_INPUT_HOTPLUG_READY"
BENCHMARK_COMPLETE_MARKER = "THEKERNEL_GRAPHICS_BENCHMARK_COMPLETE"
BENCHMARK_RENDERER_PREFIX = "THEKERNEL_GRAPHICS_RENDERER "
# Benchmark-scoped alias; the value itself lives only in profiles.py.
BENCHMARK_INPUT_SAMPLES = INPUT_SAMPLES


def renderer_for_profile(profile: str) -> str:
    """Return the only guest renderer accepted for a fixed QEMU topology."""

    topology = GRAPHICS_PROFILES.get(profile)
    if topology is None or topology.renderer is None:
        raise ValueError(f"unsupported graphics benchmark profile: {profile}")
    return topology.renderer


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
        for index in range(INPUT_SAMPLES):
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


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--list-faults",
        action="store_true",
        help="print the deterministic benchmark fault matrix as a JSON array",
    )
    args = parser.parse_args(argv)
    if args.list_faults:
        print(json.dumps(sorted(BENCHMARK_FAULTS)))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
