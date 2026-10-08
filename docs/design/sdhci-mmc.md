# SDHCI and MMC/SD port

The register/quirk definitions in `tk-axdriver-block::sdhci` are translated
from FreeBSD `sys/dev/sdhci/sdhci.h` at
`c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-2-Clause). The Rust path now
includes bounded host reset/clock/command/PIO, SD and MMC OCR initialization,
CSD and EXT_CSD capacity/partition metadata parsing, CID identity and mmcsd
card-ID formatting, CMD55 APP_CMD validation,
SCR and SD Status decoding and best-effort reads,
idempotent-command retries with CMD/DAT reset,
function-separated MMC CMD8/OCR/CID/RCA/CSD/select/status initialization helpers,
single/multi-block CMD17/18/24/25 I/O,
CMD12 multi-block stop, MMC bus-width/HS_TIMING selection, and erase-group-
aligned discard via CMD32/33/38 (using SD Status allocation-unit geometry when
available); the
PCI binding class-matches SD host controllers, decodes PCI slot-info and maps
each advertised slot BAR. FreeBSD newbus, task/callout, CAM, and bus-DMA
frameworks are not copied.

Product builds include SDHCI by default. The N305 Intel eMMC (`8086:54c4`) is
read-only by default; `mmc.allow_write=1` is required to permit writes, and the
block driver itself enforces the write restriction. The PCI binding maps the
FreeBSD `sdhci_devices[]` IDs and their quirk bits, but does not yet implement
all behavior attached to those quirks, interrupt-driven completion, full card-removal
lifecycle, automatic SD four-bit/high-speed selection, 64-bit ADMA2, automatic 1.8V
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
Capability-gated 32-bit ADMA2 uses descriptors in the prefix of a 512 KiB DMA
bounce region and falls back to SDMA or PIO; capability-gated SDMA uses the
same region. If a timeout leaves DMA quiescence uncertain, the region is
quarantined rather than freed. User-area and any advertised boot0/boot1 areas are
published as separate views;
boot area writes follow the same default-read-only policy for Intel eMMC. RPMB
metadata is decoded but its authenticated key/frame protocol is not exposed as a
generic block device, preventing unauthenticated writes.

Each data command programs the FreeBSD-derived 1-second timeout exponent from
the capability timeout clock (or the SDCLK/1 MHz quirks); missing/broken timeout
clocks select the maximum exponent, and the increment-timeout quirk is retained.

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
`tk-axdriver::sdhci`; remaining DMA-specific quirk actions that lack a safe
equivalent in the bounded polling path are explicitly not mapped.

Removable-card write-protect is sampled through the generic host callback, and
MMC R1 status errors are returned as controller errors instead of being treated
as an endless busy state. The same read-only status is carried into the user
area and boot partition block views.

The MMC layer chooses a four- or eight-bit bus from host capability, applies
EXT_CSD BUS_WIDTH/HS_TIMING through the MMC SWITCH command, and switches eMMC
boot/user views under a per-controller lock. R1B operations wait for DAT busy to
clear. RPMB remains intentionally unavailable as a raw block device because its
write protocol requires authenticated frames and key policy.
Legacy high-speed switching is rejected before CMD6 unless the SDHCI host
advertises `CAN_DO_HISPD`; the card and host cannot be left in mismatched modes.

For EXT_CSD revision 6+ devices with a nonzero cache size, attach enables the
eMMC cache and tracks successful writes; `flush()` issues EXT_CSD FLUSH_CACHE
and clears the dirty state only after command completion. The FreeBSD power-
class selection fields are decoded and applied for the implemented legacy
high-speed path. HS200/HS400 remain gated off until 1.2/1.8 V, retuning, and the
complete timing transition paths are connected.

On 2026-10-09, after the MMC timing, cache, CMD23, and data-timeout updates, the
single disposable KVM SDHCI/ext4 smoke was rerun successfully: mount, write/read,
unmount, `SDHCI_EXT4_RW_OK`, runner exit 0. This still uses a host-prepared GPT/ext4
image and does not verify guest-side partition creation/formatting.
