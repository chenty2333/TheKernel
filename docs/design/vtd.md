# Intel VT-d / DMAR

`tk-vtd` parses ACPI DMAR DRHD/RMRR entries and requester scopes, selects the matching remapping unit, provides a bounded first-fit IOVA allocator, and implements four-level Intel second-level page tables. The x86 ACPICA boot hook runs before PCI probing. For safety, a DMAR system now remains in identity mode by default; translation is opt-in with the exact boot argument `intel_iommu=on`. If enabled and no DMAR table exists, DMA remains identity-mapped; if DMAR is present, every unit must expose supported 4-level paging, 2 MiB pages, and queued invalidation or PCI DMA is refused. It installs root/context tables, a shared second-level tree with 2 MiB identity mappings for legacy drivers/RMRRs, enables QI and translation, and polls QI head/fault status with bounds.

The new DMA facade maps page-rounded physical ranges into a disjoint IOVA window and invalidates each unit before returning the device address. VirtIO's DMA allocation/share/map/unmap seam and NVMe's coherent allocation seam use this facade; neither places a CPU physical address in a new descriptor when DMAR is active. Legacy drivers continue to address the identity portion.

Current limitation: all PCI requester contexts share one second-level domain and one IOVA allocator; this supplies translated addresses but is not per-device isolation. Interrupt-remapping tables/MSI remapping and fault-event interrupt delivery are not enabled yet; faults are polled and cause fail-closed map/probe behavior. Translation-mode QEMU acceptance is **not achieved**: with split-irqchip `intel-iommu,intremap=on` and a VirtIO-block rootfs, the guest reached ACPI/VT-d initialization completion (including QI and translation enable) but then stalled before the shell acceptance marker and timed out. A diagnostic identity-DMA run with second-level contexts retained passed the block/network command sequence, narrowing the unresolved issue to translated DMA use, but not identifying the exact defect. FreeBSD's QI enable path preserves its software `hw_gcmd` bits and waits for GSTS.QIES (`intel_qi.c`); its context path flushes modified context entries before enabling translation (`intel_ctx.c`). The corresponding ordering is not yet proven equivalent here. Do not enable translation by default or treat the identity-mode run as translated-DMA acceptance. See the upstream reference implementations: [intel_qi.c](https://github.com/freebsd/freebsd-src/blob/main/sys/x86/iommu/intel_qi.c), [intel_ctx.c](https://github.com/freebsd/freebsd-src/blob/main/sys/x86/iommu/intel_ctx.c), [intel_drv.c](https://github.com/freebsd/freebsd-src/blob/main/sys/x86/iommu/intel_drv.c).


## Source-file translation progress (2026-10-09)

The initial constants/model work is now supplemented by Rust translations of
FreeBSD `intel_reg.h` (240/240 non-guard definitions, 3/3 entry structures),
`intel_dmar.h` (3/3 persistent DMAR structures with FreeBSD resource/lock/task
handles mapped to TheKernel ownership), and `intel_utils.c` (25/26 functions;
only the sysctl callback is omitted). `tk-vtd/src/utils.rs` models GCMD/GSTS
status waits and global register invalidation separately from QI; unit tests
exercise register ordering using fake MMIO. These helpers are not yet substituted
into `kernel/src/acpi/vtd.rs`, and no claim is made that the DMA hang is fixed.
Translation remains opt-in pending end-to-end translated QEMU acceptance.

FreeBSD `intel_qi.c` is now represented in `tk-vtd/src/qi.rs` at 19/19
function entry points, including queue capacity/refill and tail ordering,
wait-descriptor generation wrap, global/page/IEC invalidations, completion
sequence waits, interrupt/task drain hooks, and queue lifecycle. `QiIo` is the
platform seam for coherent DMA queue memory, taskqueues and lock/wakeup rules.
The kernel's current ad-hoc queue code has not yet been replaced by this port.

FreeBSD `intel_idpgtbl.c` is now translated at 14/14 function entries in
`tk-vtd/src/idpgtbl.rs` and `pgtbl.rs`, and the kernel Manager map/unmap seam
calls these functions. The port covers identity-table reuse/refcounts, PTE
permissions, map rollback, page/domain invalidation choice and the hardware
actual-granularity fallback. The current hardware page-table adapter supports
four-level paging and 2 MiB identity leaves; subtable pages are retained until
quiesced domain destruction instead of being reclaimed during unmap. Unit tests
pass, but no translated-QEMU claim follows from this source-level port.

FreeBSD `intel_ctx.c` is now represented at 24/24 function entry points by
`tk-vtd/src/context.rs` plus its hardware entry models. The kernel's root table
now has one root/context page per bus and each context page has the translated
second-level domain entry layout. The current design still uses one shared
second-level domain across all requesters; PCI discovery, RMRR/GAS allocation,
per-device isolation and delayed unload taskqueue ownership are not integrated.

FreeBSD `intel_fault.c` is translated at 9/9 function entries in
`tk-vtd/src/fault.rs`, including bounded two-word circular logging, FSTS/FECTL
W1C behavior, record draining and deferred reporting. Locks, taskqueues and
requester/device formatting are native `FaultIo` callbacks. The kernel's
current runtime path still uses its existing polled fault checks rather than
registering this new interrupt/task adapter.
