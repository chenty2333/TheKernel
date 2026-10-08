# SDHCI and MMC/SD port

The register and quirk definitions in `tk-axdriver-block::sdhci` are translated
from FreeBSD `sys/dev/sdhci/sdhci.h` at
`c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-2-Clause). The port will map
controller MMIO, command/data transfer, card protocol, and block registration
to TheKernel PCI, DMA, and `BlockDriverOps` interfaces. FreeBSD newbus, task,
callout, CAM, and bus-DMA frameworks are not copied. The initial Rust path now includes bounded host reset/clock/command/PIO, SD and
MMC OCR initialization, card capacity parsing, EXT_CSD sector count, and
single-block `BlockDriverOps` I/O. The PCI binding, QEMU runner, runtime eMMC
read-only policy, CMD23/multi-block/4-bit and high-speed modes, card-removal
notification, and the full upstream function set remain to be implemented.
