# Runtime block topology and GPT rescan

`tk-axfs-ng` owns the registered queue topology after filesystem initialization.
Its additive `add_block_device` / `remove_block_device` APIs publish disks and
validated GPT child views transactionally; `rescan_gpt_partitions` builds a
replacement before taking the registry lock and refuses to replace mounted
children. The block-node `BLKRRPART` ioctl is CAP_SYS_ADMIN-gated and maps bad
GPT, I/O, allocation, and busy outcomes to distinct errors.

`/dev` resolves block nodes from the live inventory, and `/sys/block`,
`/sys/class/block`, and `/sys/dev/block` expose live names/links. Stable major
and minor assignments are derived from the Linux disk spelling for AHCI,
SDHCI/MMC, and NVMe names. Old open nodes compare a shared queue identity on
every operation and fail after remove or same-name replacement.

Registry add/remove/rescan changes bridge into the kernel `DeviceRegistry` and
its add/remove uevent publisher. A kernel worker now polls only block devices
whose driver returns an authoritative media-presence fact; absent SATA links
and SD card detect withdraw the disk and children through that shared path,
while mounted claims defer removal. Automatic publication for media inserted
into an empty HBA/SDHCI slot and safe re-identification of replacement cards
remain outstanding. PCI MSI/MSI-X/INTx completion routes are wired to AHCI and
SDHCI; AHCI MSI is exercised in QEMU, while SDHCI signal generation remains
disabled by default because QEMU's INTx route lost command status when enabled.
Guest-side GPT creation, `BLKRRPART`, ext4 formatting, mount, and read/write
remain exercised by AHCI and SDHCI QEMU smoke runs.
# PCI block completion interrupts

The AHCI and SDHCI PCI frontends now admit one completion endpoint per PCI
function/slot, preferring MSI-X, then one-message MSI, then firmware-routed
shared INTx. Each endpoint acknowledges its device status before publishing a
monotonic generation and invoking registered nonblocking completion notifiers.
The block driver's `enable_irq`/`disable_irq` operations control the device
source; status polling remains bounded fallback. SDHCI keeps signal generation
disabled during card enumeration and synchronous command polling because the
QEMU SDHCI INTx path loses command status when enabled during initialization;
clients may opt in after initialization. QEMU verification observed AHCI MSI
and SDHCI firmware-routed INTx admission, and guest partition/mkfs/read/write
passed with polling fallback. Physical-device interrupt delivery is not
verified.
