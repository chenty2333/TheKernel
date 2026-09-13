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
    # Whether the device accepts the virtio-gpu scanout properties
    # (`max_outputs`, `xres`, `yres`).  A VGA-compatible adapter synthesises
    # its mode from its own video memory and rejects them, so the profile has
    # to state which device string it is building rather than appending the
    # scanout properties unconditionally.
    scanout_properties: bool = True


GRAPHICS_PROFILES = {
    "headless": GraphicsProfileTopology("none", "virtio-gpu-pci", "software"),
    "interactive": GraphicsProfileTopology("gtk", "virtio-gpu-pci", None),
    "virgl-headless": GraphicsProfileTopology("egl-headless,gl=on", "virtio-gpu-gl-pci", "virgl"),
    "virgl-interactive": GraphicsProfileTopology("sdl,gl=on", "virtio-gpu-gl-pci", "virgl"),
    # firmware-fb drives the boot framebuffer path: the linear surface the
    # firmware's GOP already programmed, which is the only display a machine
    # without a virtio-gpu device can have.  It must be a VGA-compatible
    # adapter, because a virtio-gpu with no display backend still reports a
    # mode through GOP but never a usable framebuffer base -- a guest booted
    # on one sees a framebuffer descriptor it is right to decline.
    "firmware-fb": GraphicsProfileTopology(
        "none", "bochs-display", None, scanout_properties=False
    ),
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
    """Return the profile's display device with the requested scanout."""

    topology = GRAPHICS_PROFILES[profile]
    if not topology.scanout_properties:
        return topology.device
    return f"{topology.device},max_outputs=1,xres={width},yres={height}"
