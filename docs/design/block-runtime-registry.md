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

Remaining work: AHCI/SDHCI hardware workers do not yet call the registry APIs
for slot/card insertion or removal; registry changes are not yet bridged into
the kernel `DeviceRegistry` add/remove uevent publisher; PCI MSI/MSI-X and INTx
completion paths are not yet wired into these controllers. The guest formatter
and partitioner are staged in the optional inspect payload, but QEMU must still
exercise partition creation and ext4 formatting from inside the guest.
