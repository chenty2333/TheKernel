# AHCI driver port

The initial header port translates FreeBSD `sys/dev/ahci/ahci.h` from
the FreeBSD source snapshot `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e`
(BSD-2-Clause). Hardware offsets, bit encodings, DMA wire layouts, slot/error
states, and controller/channel bookkeeping map to Rust definitions in
`tk-axdriver-block::ahci::regs`. The FreeBSD bus/resource, mutex, callout and
CAM/CCB framework types are not carried over; subsequent AHCI code will map
storage requests to TheKernel's block-device interfaces.

The header and early HBA routines are now represented by `AhciController` and
`PortState`. `AhciDisk` owns an aligned command list/FIS/command table and a
persistent DMA bounce buffer; it submits IDENTIFY DEVICE and LBA48 READ/WRITE
DMA EXT through slot zero, supports one split-phase async request with a
persistent bounce buffer, checks task-file errors, and implements
`BlockDriverOps` sync/async read-write, flush, and DSM/TRIM discard. FPDMA is
used serially with tag zero when both HBA and IDENTIFY advertise NCQ; concurrent
NCQ queue admission and multi-victim slot recovery remain untranslated.
A command timeout poisons the disk; the persistent workspace is retained until
port shutdown proves DMA stopped, and is leaked if shutdown cannot prove it.

The initial PCI binding lives in `tk-axdriver/src/ahci.rs`: it matches PCI
class/subclass/prog-if (and known RAID-class IDs), enables memory and bus
mastering, maps ABAR (BAR5 or the ABAR0 quirk), resets the HBA, and publishes
the first identified ATA disk. AHCI is selected
by the `tk-axdriver` default feature. The complete 317-row FreeBSD PCI
ID/revision/name/quirk table is translated in `tk-axdriver/src/ahci/pci_ids.rs`;
`ahci_pci_attach` selects BAR0 for the ABAR0 quirk and BAR5 otherwise. Remaining
porting work includes MSI/MSI-X routing, enclosure management, Intel remapped
NVMe, CAM CCB/SCSI translation, automatic block-device publication for media
inserted into a port that was empty at boot, PCI-function removal events,
multi-slot scheduling/recovery, and concurrent NCQ submission. An existing
port returns I/O errors while absent and only resumes after IDENTIFY geometry
and serial/model/capacity fingerprint match.

The QEMU topology was extended to attach `ich9-ahci` plus an `ide-hd`, and
AHCI is explicitly included in product builds. QEMU KVM enumerated a disposable
GPT SATA disk, published `/dev/sda` and `/dev/sda1`, mounted its ext4 filesystem,
wrote/read a file, cleanly unmounted, and emitted `AHCI_EXT4_RW_OK`; the marker
was present in the backing image after shutdown. The host-prepared filesystem
smoke is `tests/guest/ahci-ext4-smoke.sh`. On 2026-10-09 the inspect payload was
extended with pinned e2fsprogs/sfdisk and the block registry gained
`BLKRRPART` plus live devfs/sysfs views. The repeatable
`tests/guest/block-partition-mkfs-smoke.sh` passed with an empty 64 MiB AHCI
image: guest-created GPT, guest `mkfs.ext4`, mount, read/write, unmount, and
`AHCI_PARTITION_MKFS_RW_OK`. Device-originated hotplug publication, PCI removal,
and the untranslated FreeBSD functions listed above remain outstanding.

`AHCI_Q_IOMMU_BUSWIDE` is retained in the translated PCI quirk table, but cannot
be applied by this driver until the platform IOMMU provides a bus-wide DMA
identity/domain API. PHY-change events on an existing disk force link reset and
IDENTIFY fingerprint validation; automatic enumeration of a newly inserted disk
on a port that was empty at boot remains blocked by the missing runtime block
registry/device-node publication API.
