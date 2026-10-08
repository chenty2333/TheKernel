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

The FreeBSD bus/resource manager, task queues/interrupt fanout, CAM CCB/SCSI
translation, hotplug event handling, multi-slot transaction recovery, and
PCI binding are still outstanding. The current controller logic is tested with
synthetic MMIO only; QEMU device acceptance awaits the PCI/frontend integration.
