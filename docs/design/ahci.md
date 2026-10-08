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
DMA EXT through slot zero, polls CI synchronously, checks task-file errors, and
implements `BlockDriverOps` flush. ATA FPDMA and DSM/TRIM register-FIS encoding
is present, but NCQ queue admission and TRIM submission are not yet enabled.
A command timeout poisons the disk; the persistent workspace is retained until
port shutdown proves DMA stopped, and is leaked if shutdown cannot prove it.

The initial PCI binding lives in `tk-axdriver/src/ahci.rs`: it matches PCI
class/subclass/prog-if, enables memory and bus mastering, maps ABAR (BAR5),
resets the HBA, and publishes the first identified ATA disk. AHCI is selected
by the `tk-axdriver` default feature. Remaining porting work includes the full
FreeBSD PCI ID/quirk table, every port/device publication, MSI/MSI-X routing,
CAM CCB/SCSI translation, hotplug, multi-slot scheduling/recovery, and NCQ/TRIM
submission. The PCI path has only been compiled so far; QEMU disk read/write
acceptance still remains.
