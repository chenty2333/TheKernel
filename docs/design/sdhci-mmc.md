# SDHCI and MMC/SD port

The register/quirk definitions in `tk-axdriver-block::sdhci` are translated
from FreeBSD `sys/dev/sdhci/sdhci.h` at
`c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-2-Clause). The Rust path now
includes bounded host reset/clock/command/PIO, SD and MMC OCR initialization,
CSD and EXT_CSD capacity/partition metadata parsing, CID identity and mmcsd
card-ID formatting, CMD55 APP_CMD validation,
idempotent-command retries with CMD/DAT reset,
function-separated MMC CMD8/OCR/CID/RCA/CSD/select/status initialization helpers,
single/multi-block CMD17/18/24/25 I/O,
CMD12 multi-block stop, MMC bus-width/HS_TIMING selection, and erase-group-
aligned discard via CMD32/33/38; the
PCI binding class-matches SD host controllers, decodes PCI slot-info and maps
each advertised slot BAR. FreeBSD newbus, task/callout, CAM, and bus-DMA
frameworks are not copied.

Product builds include SDHCI by default. The N305 Intel eMMC (`8086:54c4`) is
read-only by default; `mmc.allow_write=1` is required to permit writes, and the
block driver itself enforces the write restriction. The PCI binding maps the
FreeBSD `sdhci_devices[]` IDs and their quirk bits, but does not yet implement
all behavior attached to those quirks, interrupt handling, full card-removal
lifecycle, automatic SD four-bit/high-speed selection, ADMA2, automatic 1.8V
negotiation/tuning, UHS/HS200/HS400, and the full
upstream function set. Removable SD defaults to the safe 1-bit/25 MHz mode;
the SD CMD6/ACMD6 helpers are present, but automatic SD bus-width/high-speed
switching remains off because QEMU's emulated card times out those requests.
Generic signal-voltage and tuning entry points are translated, but are not
entered automatically until end-to-end voltage-switch/tuning support is wired
into card capability negotiation.
The QEMU PCI SDHCI model (`1b36:0007`) additionally uses a local
single-block-only mode after observed CMD18 timeouts; this local behavior is
separate from FreeBSD's PCI quirk table. The generic write-protect callback
follows FreeBSD's active-low PRESENT_STATE interpretation.
Capability-gated SDMA uses a 512 KiB, 32-bit DMA bounce
region; broken/unsupported DMA falls back to PIO. If a timeout leaves DMA
quiescence uncertain, the region is quarantined rather than freed. User-area and any advertised boot0/boot1 areas are
published as separate views;
boot area writes follow the same default-read-only policy for Intel eMMC. RPMB
metadata is decoded but its authenticated key/frame protocol is not exposed as a
generic block device, preventing unauthenticated writes.

QEMU `1b36:0007` advertises DMA but fails SD CMD17 with the SDMA transfer setup
used here, so that virtual model is assigned a local broken-DMA quirk and
single-block PIO fallback. This is separate from the FreeBSD upstream PCI
quirk table. With that mapping, the requested KVM `sdhci-pci` + `sd-card` test
mounted GPT/ext4, verified read/write, unmounted, and emitted
`SDHCI_EXT4_RW_OK`; the disposable image retains the written file after exit.

QEMU KVM attached `sdhci-pci` plus a `sd-card`; TheKernel enumerated
`/dev/mmcblk0` and its GPT partition, mounted ext4, read/wrote a file, unmounted,
and emitted `SDHCI_EXT4_RW_OK`. The backing image contained the guest-written
marker after clean shutdown. The repeatable guest operation is
`tests/guest/sdhci-ext4-smoke.sh`. GPT and ext4 were prepared on the host for
this run; the guest does not yet rescan partitions created after boot and the
current BusyBox image has no `mkfs.ext4`, so guest-side partitioning/formatting
remains unverified. The QEMU SD-card path does not exercise N305 eMMC.

The generic host also applies the translated response-shift, card-presence,
reset-order, timeout-control, and per-controller quirk behavior where its PIO
path has a direct equivalent. The exact PCI ID/quirk table is in
`tk-axdriver::sdhci`; unsupported DMA-specific quirk actions remain inert because
this path does not use SDMA/ADMA.

Removable-card write-protect is sampled through the generic host callback, and
MMC R1 status errors are returned as controller errors instead of being treated
as an endless busy state. The same read-only status is carried into the user
area and boot partition block views.

The MMC layer chooses a four- or eight-bit bus from host capability, applies
EXT_CSD BUS_WIDTH/HS_TIMING through the MMC SWITCH command, and switches eMMC
boot/user views under a per-controller lock. R1B operations wait for DAT busy to
clear. RPMB remains intentionally unavailable as a raw block device because its
write protocol requires authenticated frames and key policy.
