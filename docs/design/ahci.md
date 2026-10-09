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
DMA EXT through slot zero, accepts bounded pinned physical SG requests as
multiple PRDs without payload copies, and routes bounce-buffer requests through
one split-phase typed completion handle. Direct pinned physical-SG NCQ batches
reserve slot zero for recovery and can publish multiple tag-matched slots in
one CI write; each completion retains the original handle/cookie and block
owner. Callers retain pinned SG pages until physical completion. A port-wide
error/reset fails all still-in-flight batch members (the READ LOG EXT tag is
retained when available); it does not yet reproduce CAM's fine-grained
requeue of non-victim requests. It checks task-file errors and implements
`BlockDriverOps` sync/async read-write, flush, and DSM/TRIM discard. FPDMA is
used with tag zero for synchronous and bounce-buffer operations when both HBA
and IDENTIFY advertise NCQ; direct physical batch operations use concurrent
per-slot FPDMA tags up to the IDENTIFY queue depth.
A command timeout poisons the disk; the persistent workspace is retained until
port shutdown proves DMA stopped, and is leaked if shutdown cannot prove it.

The initial PCI binding lives in `tk-axdriver/src/ahci.rs`: it matches PCI
class/subclass/prog-if (and known RAID-class IDs), enables memory and bus
mastering, maps ABAR (BAR5 or the ABAR0 quirk), resets the HBA, and publishes
the first identified ATA disk. AHCI is selected
by the `tk-axdriver` default feature. The complete 317-row FreeBSD PCI
ID/revision/name/quirk table is translated in `tk-axdriver/src/ahci/pci_ids.rs`;
`ahci_pci_attach` selects BAR0 for the ABAR0 quirk and BAR5 otherwise. Remaining
The PCI frontend now attempts MSI-X, MSI, then firmware-routed shared INTx, and
AHCI completions acknowledge status before waking waiters. Concurrent direct
physical-SG NCQ submission is implemented across the advertised slot depth, but
an error/reset completes all outstanding batch members rather than requeueing
only the READ LOG EXT victim. When CAP.SPM and the port signature identify a
PMP, the frontend scans targets 0 through 14 with PMP-targeted IDENTIFY and
publishes each disk as a distinct node; wrappers share one port-owned engine
and serialize commands while selecting target-specific FIS/geometry state.
Individual PMP-target removal detection and hot-swap behavior remain
unimplemented. Remaining work also includes
enclosure management, Intel remapped NVMe, CAM CCB/SCSI translation,
PCI-function removal events, and power-management/newbus lifecycle. A controller
worker retries ports that were empty or not ready
at boot and publishes successfully identified media through the runtime block
registry. An existing port returns I/O errors while absent and only resumes
after IDENTIFY geometry and serial/model/capacity fingerprint match.

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
`AHCI_PARTITION_MKFS_RW_OK`. Controller hotplug insertion/removal was added, but
physical media swap has not yet been exercised in QEMU; PCI-function removal
and the untranslated FreeBSD functions listed above remain outstanding.

`AHCI_Q_IOMMU_BUSWIDE` is retained in the translated PCI quirk table, but cannot
be applied by this driver until the platform IOMMU provides a bus-wide DMA
identity/domain API. PHY-change events on an existing disk force link reset and
IDENTIFY fingerprint validation. A periodic controller worker also attaches and
publishes media discovered on ports empty at boot through the runtime registry.

The block error path now uses an explicit `ahci_reset()` adapter matching the
upstream reset order: prove FIS/command-engine DMA stop, attempt CLO without
making CLO timeout fatal, reset PHY, then restart FIS and command processing.
The block caller completes its request rather than CAM-freezing/requeueing CCBs.

The IRQ top half now splits controller-wide interrupt routing from the exact
`ahci_ch_intr()` per-port latch/ack step. It does not run CAM's locked task
queue; status is atomically retained and request-context completion checks CI/
SACT and TFD before publishing completion.

The controller-error path is named `ahci_issue_recovery()`: NCQ failures attempt
READ LOG EXT and capture the failing tag before reset. The current block layer
reports typed errors for the affected submitted batch; it does not reproduce
CAM's held CCB sense/retry queue.

`ahci_process_read_log()` now names the NQ/tag decode of READ LOG EXT status;
NCQ recovery consumes the failing tag before port reset, but all pending block
members in the errored hardware batch are failed together rather than CAM's
single-victim requeue protocol.

`ahci_timeout()` is the bounded-wait expiry adapter and delegates to the shared
reset/quiescence path; no FreeBSD callout is armed, so timeout detection occurs
while a block request is waiting or reaped.

Physical requests now share source-named `ahci_done()` typed completion
publication, and reset retirement maps to `ahci_end_transaction()`. These
adapters preserve request handle/cookie and report zero completed bytes on an
error; they do not provide CAM sense or request-requeue policy.

`ahci_process_timeout()` drains the active physical batch after a request reaches
the bounded wait deadline; it performs one shared reset and reports device
error only if DMA stop is proven, otherwise keeping each caller's physical
buffer quarantined.
