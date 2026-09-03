"""Single-source graphics profile and benchmark fault data.

Every consumer of the graphics profile list — the QEMU command builder, the
product CLI, and the Linux oracle runner — reads this table instead of
keeping its own copy.  The benchmark fault matrix and the input-sample count
live here for the same reason.
"""

from __future__ import annotations

from dataclasses import dataclass


@dataclass(frozen=True)
class GraphicsProfileTopology:
    """The fixed QEMU display/device pair and guest renderer of a profile."""

    display: str
    device: str
    # The only guest renderer the graphics benchmark accepts on this profile,
    # or None when the profile cannot run the benchmark at all.
    renderer: str | None


GRAPHICS_PROFILES = {
    "headless": GraphicsProfileTopology("none", "virtio-gpu-pci", "software"),
    "interactive": GraphicsProfileTopology("gtk", "virtio-gpu-pci", None),
    "virgl-headless": GraphicsProfileTopology("egl-headless,gl=on", "virtio-gpu-gl-pci", "virgl"),
    "virgl-interactive": GraphicsProfileTopology("gtk,gl=on", "virtio-gpu-gl-pci", "virgl"),
    # Keep this ABI string exact.  The Venus rootfs verifies Vulkan
    # capability itself and must never fall back to the legacy Virgl device
    # configuration.
    "venus-interactive": GraphicsProfileTopology(
        "gtk,gl=on",
        "virtio-gpu-gl-pci,blob=on,venus=on,hostmem=1G,max_hostmem=1G",
        "venus",
    ),
}
BENCHMARK_PROFILES = tuple(
    name for name, topology in GRAPHICS_PROFILES.items() if topology.renderer is not None
)
BENCHMARK_FAULTS = frozenset(
    {"modeset", "client-crash", "vt-switch", "weston-restart", "input-hotplug"}
)
INPUT_SAMPLES = 10


def graphics_device(profile: str, width: int, height: int) -> str:
    """Return the profile's virtio-gpu device with the requested scanout."""

    topology = GRAPHICS_PROFILES[profile]
    return f"{topology.device},max_outputs=1,xres={width},yres={height}"
