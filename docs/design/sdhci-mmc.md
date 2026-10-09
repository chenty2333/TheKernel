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
all behavior attached to those quirks, full card-removal lifecycle, 64-bit
ADMA2, and the full upstream function set.
Removable SD negotiates SCR-supported four-bit mode
and legacy CMD6 high-speed; when both ACMD41 S18A and host 1.8V capability are
present it switches signaling voltage with CMD11 and selects SDR104/SDR50/DDR50
only when both the card switch-status and host advertise them; SDR104 and
required SDR50 tuning use CMD19. The eMMC path
capability-gates 1.8V DDR52, HS200 (with CMD21 tuning), and HS400 on both
EXT_CSD card bits and host support. Unsupported 1.2V modes are rejected because
the generic host has no 1.2V switch operation.
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
marker after clean shutdown. The host-prepared compatibility operation is
`tests/guest/sdhci-ext4-smoke.sh`. On 2026-10-09 the guest-side partition/format
script passed on an empty 64 MiB QEMU SD card: sfdisk-created GPT, `BLKRRPART`,
guest `mkfs.ext4`, ext4 mount/read/write/unmount, and
`SDHCI_PARTITION_MKFS_RW_OK`. The QEMU SD-card path does not exercise N305 eMMC
or real card-removal interrupt handling.

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
The SD path now reads CMD6 support/selection status, negotiates 4-bit mode from
SCR, and selects SD high-speed only when the card and host both advertise it;
the QEMU acceptance card completed guest format/RW in 4-bit mode at 26 MHz.

For EXT_CSD revision 6+ devices with a nonzero cache size, attach enables the
eMMC cache and tracks successful writes; `flush()` issues EXT_CSD FLUSH_CACHE
and clears the dirty state only after command completion. The FreeBSD power-
class selection fields are decoded and applied for the selected timing and bus
width. The HS200/HS400 path uses 1.8V only and fails attach if the host/card
transition or initial tuning fails; recovery after a failed voltage/timing
transition is not available. MMC HS200/HS400 and SD SDR50/SDR104 consume Host
Control2 retune interrupt requests and implement the mode-1 interval; HS400
re-enters HS200, runs CMD21, then restores the 52MHz DDR8-to-HS400 sequence.
The QEMU 1b36:0007 scenario validates SD guest formatting and I/O but is not a
physical 1.8V signaling or removable-card swap test.

On 2026-10-09, the inspect payload gained `sfdisk` and e2fsprogs; a blank 64 MiB
QEMU SD card passed guest GPT creation, `BLKRRPART`, guest `mkfs.ext4`, mount,
write/read, unmount, and `SDHCI_PARTITION_MKFS_RW_OK`. A controller worker now
retries empty slots and re-runs card enumeration after card insertion; the
shared media worker withdraws absent cards and partitions. Physical card-swap
acceptance remains unverified. SDHCI enables routed signaling after card
enumeration when the PCI adapter considers the route usable. The QEMU 1b36:0007
model is marked signal-broken because enabling INTx loses CMD17 completions;
that device retains bounded status polling as fallback.

The eMMC timing selector is factored as `mmc_calculate_clock()` and consumes
only host/card-advertised modes and the bus width actually verified by CMD19/14;
HS200/HS400 transitions are named and sequenced by the corresponding upstream
functions. `mmc_discover_cards()` is the one-selected-card form of FreeBSD's
multi-child discovery loop. The bus-wide scan/child lifecycle remains outside
this block-driver model.

Card hotplug now follows the upstream task/poll/present split by source function
name. The slot table lock is acquired for one slot at a time; long card command
polls no longer hold the lock while unrelated slots are inspected.
