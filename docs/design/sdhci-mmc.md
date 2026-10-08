# SDHCI and MMC/SD port

The register/quirk definitions in `tk-axdriver-block::sdhci` are translated
from FreeBSD `sys/dev/sdhci/sdhci.h` at
`c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-2-Clause). The Rust path now
includes bounded host reset/clock/command/PIO, SD and MMC OCR initialization,
CSD and EXT_CSD capacity parsing, idempotent-command retries with CMD/DAT reset,
single/multi-block CMD17/18/24/25 I/O,
CMD12 multi-block stop, SD four-bit bus selection and legacy high-speed switch,
MMC HS_TIMING selection, and erase-group-aligned discard via CMD32/33/38; the
PCI binding class-matches SD host controllers and
maps BAR0. FreeBSD newbus, task/callout, CAM, and bus-DMA frameworks are not
copied.

Product builds include SDHCI by default. The N305 Intel eMMC (`8086:54c4`) is
read-only by default; `mmc.allow_write=1` is required to permit writes, and the
block driver itself enforces the write restriction. The PCI binding does not
yet translate controller-specific quirk tables, interrupt handling, full
slot/card removal lifecycle, SDMA/ADMA2, 1.8V switching, tuning, UHS/HS200/HS400,
boot/RPMB eMMC child devices, full quirk coverage, interrupt-driven completion,
and the full upstream function set. Only the user-area eMMC is currently
published; EXT_CSD partition metadata is read but not exposed as devices.

QEMU KVM attached `sdhci-pci` plus a `sd-card`; TheKernel enumerated
`/dev/mmcblk0` and its GPT partition, mounted ext4, read/wrote a file, unmounted,
and emitted `SDHCI_EXT4_RW_OK`. The backing image contained the guest-written
marker after clean shutdown. The repeatable guest operation is
`tests/guest/sdhci-ext4-smoke.sh`. GPT and ext4 were prepared on the host for
this run; the guest does not yet rescan partitions created after boot and the
current BusyBox image has no `mkfs.ext4`, so guest-side partitioning/formatting
remains unverified. The QEMU SD-card path does not exercise N305 eMMC.
