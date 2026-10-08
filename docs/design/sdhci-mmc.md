# SDHCI and MMC/SD port

The register and quirk definitions in `tk-axdriver-block::sdhci` are translated
from FreeBSD `sys/dev/sdhci/sdhci.h` at
`c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-2-Clause). The port will map
controller MMIO, command/data transfer, card protocol, and block registration
to TheKernel PCI, DMA, and `BlockDriverOps` interfaces. FreeBSD newbus, task,
callout, CAM, and bus-DMA frameworks are not copied. At this stage only the
hardware constant set has been translated; no SDHCI/MMC device is enabled.
