# Intel VT-d / DMAR

`tk-vtd` parses ACPI DMAR DRHD/RMRR entries and requester scopes, selects the matching remapping unit, provides a bounded first-fit IOVA allocator, and implements four-level Intel second-level page tables. The x86 ACPICA boot hook runs before PCI probing. For safety, a DMAR system remains in identity mode by default; translation is opt-in with the exact boot argument `intel_iommu=on`. If enabled and no DMAR table exists, DMA remains identity-mapped; if DMAR is present, every unit must expose supported 4-level paging, 2 MiB pages, and queued invalidation or PCI DMA is refused. It installs root/context tables, a shared second-level tree with 2 MiB identity mappings for legacy drivers/RMRRs, enables QI and translation.

The new DMA facade maps page-rounded physical ranges into a disjoint IOVA window and invalidates each unit before returning the device address. VirtIO's DMA allocation/share/map/unmap seam and NVMe's coherent allocation seam use this facade; neither places a CPU physical address in a new descriptor when DMAR is active. Legacy drivers continue to address the identity portion.

Current limitation: all PCI requester contexts share one second-level domain and one IOVA allocator; this supplies translated addresses but is not per-device isolation. Interrupt-remapping tables/MSI remapping and fault-event interrupt delivery are not enabled yet; faults are polled and cause fail-closed map/probe behavior. Kernel initialization now calls the translated `intel_utils.c` register/status helpers and `intel_qi.c` queue/sequence operations. The latest `intel_iommu=on` split-irqchip acceptance timed out before its block/network marker. A separate GDB diagnostic (same kernel and topology) established that VT-d initialization did complete (`MODE_ENABLED`, manager present) and CPU 0 was waiting in VirtIO GPU `GET_DISPLAY_INFO` at `VirtQueue::can_pop`; the manager held seven dynamic mappings. At the captured point, the used ring index was 1, the response buffer remained zeroed, the used element length was zero, `GSTS=0xc4000000`, `FSTS=0`, and `IQH=IQT=0x200`. The second-level leaf for the dynamic response page was present/read-write and pointed at its expected physical page. This narrows the failure to translated VirtIO-GPU queue/request behavior, but does not establish whether the cause is a queue descriptor, device response, or another translation interaction. Do not enable translation by default or treat the identity-mode run as translated-DMA acceptance. See the upstream reference implementations: [intel_qi.c](https://github.com/freebsd/freebsd-src/blob/main/sys/x86/iommu/intel_qi.c), [intel_ctx.c](https://github.com/freebsd/freebsd-src/blob/main/sys/x86/iommu/intel_ctx.c), [intel_drv.c](https://github.com/freebsd/freebsd-src/blob/main/sys/x86/iommu/intel_drv.c).


## Source-file translation progress (2026-10-09)

The initial constants/model work is now supplemented by Rust translations of
FreeBSD `intel_reg.h` (240/240 non-guard definitions, 3/3 entry structures),
`intel_dmar.h` (3/3 persistent DMAR structures with FreeBSD resource/lock/task
handles mapped to TheKernel ownership), and `intel_utils.c` (25/26 functions;
only the sysctl callback is omitted). `tk-vtd/src/utils.rs` models GCMD/GSTS
status waits and global register invalidation separately from QI; unit tests
exercise register ordering using fake MMIO. The kernel hardware adapter uses
these helpers for firmware-state shutdown, root-table load, pre-QI global
invalidation and TE activation.

FreeBSD `intel_qi.c` is now represented in `tk-vtd/src/qi.rs` at 19/19
function entry points, including queue capacity/refill and tail ordering,
wait-descriptor generation wrap, global/page/IEC invalidations, completion
sequence waits, interrupt/task drain hooks, and queue lifecycle. `QiIo` is the
platform seam for coherent DMA queue memory, taskqueues and lock/wakeup rules.
The kernel uses this queue for global context/IOTLB invalidation and hardware
completion writeback; without an APIC QI-vector handler, wait interrupts stay
masked and completions are synchronously polled.

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

FreeBSD `intel_quirks.c` is mapped at 7/7 functions in `tk-vtd/src/quirks.rs`,
including the 5400/5500 northbridge revisions and E5 MAMV cap. The Rust matcher
accepts native PCI northbridge and CPUID leaf-1 facts; the driver initialization
order still needs to call pre-use/post-ident hooks.

FreeBSD `intel_drv.c` table/device orchestration is now represented at 35/40
function entries. The five omitted functions are DDB-only diagnostic commands.
The parser consumes ACPI DRHD/RMRR/RHSA structures through the iterator, retains
RHSA proximity affinity and RMRR scope paths, and offers endpoint/bridge,
include-all, HPET/IOAPIC, PCI path, and RMRR lookup adapters. Kernel startup now
builds one root entry/context page per bus before unit initialization. The Rust
`DriverState` layer is an adapter rather than a full dynamic PCI device driver.
'''
PY
rustfmt --edition 2024 crates/ax/tk-vtd/src/driver.rs crates/ax/tk-vtd/src/lib.rs kernel/src/acpi/vtd.rs
git diff --check && export THEKERNEL_STATE_DIR=/home/ava/.cache/thekernel-targets/wt-platform CARGO_BUILD_JOBS=3; cargo test -p tk-vtd --lib >/home/ava/.cache/thekernel-targets/wt-platform-vtd-driver-test.log && cargo check -p tk-kernel --features 'input nvme intel-hda watchdog-itco pmu perf-sampling bpf hwp-uclamp' --target x86_64-unknown-none >/home/ava/.cache/thekernel-targets/wt-platform-vtd-driver-check.log && git add crates/ax/tk-vtd/src/driver.rs crates/ax/tk-vtd/src/lib.rs kernel/src/acpi/vtd.rs docs/upstream-provenance.md docs/licensing.md docs/design/vtd.md && git commit -m 'vtd: translate FreeBSD Intel DMAR driver routing' && git status --short
The generic FreeBSD `iommu_gas.c` allocator is ported to `tk-vtd/src/gas.rs`
(31/33 functions; only two DDB GAS views omitted). Its address-ordered first-fit
now preserves page guard gaps, boundary/alignment and low/high bounds, fixed
RMRR overlap handling, partial unmap clipping and deferred entry lifetime.
`IovaAllocator` delegates to this GAS facade and returns guarded IOVAs. The
source's augmented intrusive RB tree is represented by a sorted vector scan;
this is behaviorally bounded by domain entries but has O(n) scans rather than
RB-tree logarithmic lookup. A product lint attempt after the previous five-commit
batch stopped on seven existing undocumented-unsafe errors under
`crates/ax/tk-axallocator/{slab,tlsf}.rs`; no unrelated allocator edits were made.

The scoped `iommu_gas.c` address-space core is now ported in `tk-vtd/src/gas.rs`
(31/33 functions); the two DDB-only inspection commands are omitted. It retains
first-fit bounds/guard-page/alignment/boundary behavior, fixed/RMRR reservations,
partial removal and delayed entry release, and the IOVA facade now delegates to
it. For platform fit, the source's augmented intrusive RB tree is represented
as an ordered vector with linear gap scans; this preserves layout behavior but
has O(n) lookup cost under many active mappings.

FreeBSD `busdma_iommu.c` core map load/unload is represented in
`tk-vtd/src/busdma.rs` (2/34 source functions): the translated load path maps
bounded physical ranges into device-visible segments and rolls back partial
work on failure; unload retires every entry. FreeBSD busdma tags, VM-page
lookup, KMSAN, callback locks, and delayed taskqueue wrappers are framework
adapters. TheKernel callers provide physical ranges to the DMA interface. The
adapter enforces segment count/size, alignment, boundary, and low-address
constraints, but is not yet wired as a tag layer into every kernel DMA client.

`intel_intrmap.c` is represented at 10/10 function entry points by
`tk-vtd/src/intrmap.rs`: contiguous first-fit IRTE allocation, direct DMAR MSI
routing, requester-tagged MSI entries, IOAPIC delivery-mode/polarity/trigger
encoding, entry update/free with IEC invalidation, and IRTA initialization/final
sequence. VMEM, `device_t` source lookup, `intr_reprogram()` and physical IRTE
allocation are adapters. TheKernel's current ACPI/VT-d startup does not install
this IR table adapter into the APIC/IOAPIC vector path; therefore these routes
are unit-tested code, not enabled interrupt remapping.

The generic FreeBSD `iommu_utils.c` shared routines now have an explicit
`tk-vtd/src/iommu_utils.rs` mapping (8/44 functions): four radix page-table
geometry helpers, the domain-derived bus-DMA constraints, and three queued-
invalidation generation/wait routines (the latter reuse the port in `qi.rs`).
The remaining routines are FreeBSD VM/sf_buf and bus topology wrappers, x86
IOMMU vtable dispatch, IRQ/MSI resource management, sysctl registration, and
DDB output; page storage, DMA clients, QI, and interrupt routes map to existing
TheKernel-owned seams instead of importing those frameworks.

Final `intel_iommu=on` QEMU split-irqchip acceptance was rerun after the
FreeBSD source adapters were added. The runner confirmed the Multiboot command
line carried `intel_iommu=on` and attached `intel-iommu,intremap=on`, but QEMU
timed out at 180 seconds before the guest acceptance commands ran. The serial
log stopped immediately after BSP CPU feature/enable messages; no VT-d-stage
log or guest marker was observed, so the exact stall point is unknown. The
identity-by-default fallback remains in place; translated DMA and interrupt
remapping are not accepted and must not be enabled by default.

Follow-up `busdma_iommu.c` coverage adds the page-array loader and contiguous
physical loader seams (`iommu_bus_dmamap_load_ma` and `_load_phys`), raising
mapped source functions to 4/34. VM page-object discovery and virtual-buffer
`pmap_extract` stay caller-supplied framework inputs.

The kernel ACPI adapter now routes firmware-state shutdown, RTADDR programming,
register context/IOTLB invalidation, queued invalidation enable, wait-descriptor
completion and TE activation through the translated `utils.rs` and `qi.rs`
operations. Its `QiIo` implementation reserves a dedicated coherent writeback
page; since this platform seam does not yet allocate/register the VT-d QI MSI
vector, it leaves QI interrupts masked and polls the memory completion sequence.
Global context/IOTLB invalidations now use the translated QI ring rather than the
previous two-descriptor MMIO polling path.

The busdma load sequence is now split into the upstream `load_something()`
all-or-nothing commit wrapper and `load_something1()` segment mapper. It also
accepts page-array, physical-extent, and virtual-buffer loads; the latter uses a
caller-provided page-table extractor to replace FreeBSD `pmap_extract`. This
raises direct busdma source coverage to 6/34 functions; tags, memory alloc/free,
wait/callback and KMSAN routines remain TheKernel framework seams.

Follow-up acceptance on 2026-10-09 still failed with `intel_iommu=on`. The
firmware-framebuffer profile without VirtIO-GPU advanced through filesystem,
VirtIO-net and secondary-CPU startup, then stopped after `Initialize alarm...`
before the shell marker; the matching no-VT-d control completed disk/network/
ping and powered off. This narrows the stopping point versus earlier boot logs,
but does not prove whether the cause is DMA translation, a device interrupt, or
an unrelated startup interaction. The required translated acceptance did not
pass; identity-by-default stays mandatory. No additional acceptance retry is
claimed.

Coverage accounting for the final source-file pass: `busdma_iommu.c` is 6/34;
the 28 omitted entry points are native busdma tag/identity/requester/context
wrappers (`iommu_bus_dma_is_dev_disabled`, `iommu_get_requester`,
`iommu_instantiate_ctx`, `iommu_get_dev_ctx`, `iommu_get_dma_tag`,
`bus_dma_iommu_set_buswide`, `iommu_is_buswide_ctx`, `iommu_set_buswide_ctx`,
`iommu_bus_dma_tag_create`, `iommu_bus_dma_tag_destroy`,
`iommu_bus_dma_tag_set_domain`, `iommu_bus_dma_id_mapped`,
`bus_dma_iommu_load_ident`), FreeBSD map-object and coherent-memory allocation
(`iommu_bus_dmamap_create/destroy`, `iommu_bus_dmamem_alloc/free`), delayed
callback/taskqueue ownership (`iommu_bus_dmamap_waitok`,
`iommu_bus_dmamap_complete`, `iommu_bus_task_dmamap`,
`iommu_bus_schedule_dmamap`, `iommu_init_busdma`, `iommu_fini_busdma`,
`iommu_domain_init/fini`, `iommu_domain_unload_task`), and KMSAN/sync hooks
(`iommu_bus_dmamap_sync`, `iommu_bus_dmamap_load_kmsan`). They depend on the
FreeBSD busdma, VM, taskqueue, or KMSAN frameworks rather than on the IOVA
load/unload algorithm. `iommu_utils.c` is 8/44: four page-table radix helpers,
the requester DMA-constraint setup, and three queued-invalidation sequence
helpers are translated. Its 36 omitted entry points are: VM-object/sf_buf page
mapping (`iommu_pgalloc`, `iommu_pgfree`, `iommu_map_pgtbl`,
`iommu_unmap_pgtbl`); x86 IOMMU vtable/no-IOMMU dispatch and device/context
lookup (`get_x86_iommu`, `set_x86_iommu`, all six `x86_no_iommu_*` routines,
`iommu_domain_free_entry`, `iommu_domain_unload_entry`,
`iommu_domain_unload`, `iommu_get_ctx`, `iommu_free_ctx_locked`,
`iommu_find`, `iommu_unit_pre_instantiate_ctx`); QI interrupt/taskqueue and
deferred-drain wrappers (`iommu_qi_common_init`, `iommu_qi_common_fini`,
`iommu_qi_drain_tlb_flush`, `iommu_qi_invalidate_locked`,
`iommu_qi_invalidate_sync`), whose descriptor/wait mechanics use `qi.rs` but
whose FreeBSD task/interrupt lifecycle is not portable; MSI/IOAPIC interrupt
resource adapters (`iommu_alloc_irq`, `iommu_alloc_msi_intr`,
`iommu_map_msi_intr`, `iommu_unmap_msi_intr`, `iommu_map_ioapic_intr`,
`iommu_unmap_ioapic_intr`, `iommu_release_intr`); and presentation/debug
callbacks (`iommu_device_set_iommu_prop`, four `iommu_db_*` DDB commands).
Page allocation, PCI discovery, APIC/IOAPIC routing, and debug registration
remain platform-owned framework responsibilities.

The QEMU VT-d topology now uses modern-only VirtIO PCI devices with
`iommu_platform=on`; transitional devices do not advertise the platform-DMA
feature and cannot test translated VirtIO DMA. The shared transport negotiates
`VERSION_1` and `ACCESS_PLATFORM` whenever offered, matching the HAL's bus-DMA
address contract. With that topology, the 2026-10-09 `intel_iommu=on` run now
reaches the shell, lists the VirtIO block device, and sends a network packet,
but still does not pass network acceptance: `ping 10.0.2.2` failed, and the
post-run interface counters were TX=1/RX=0 with zero reported TX/RX errors.
At the QEMU GDB snapshot `FSTS=0`, `GSTS=0xc4000000`, and QI head/tail both
`0x580`; these observations do not prove whether the missing RX completion is
DMA, an interrupt, or network-stack behavior. Translation remains opt-in and
must not be enabled by default until block and network acceptance both pass.
An additional bounded guest diagnostic showed the VirtIO-net route's vector 54
(ACPI GSI 22) increment from absent to 1 across the ping attempt. Thus the
route delivered at least one interrupt during the attempt, but the available
counters do not identify whether it was TX completion or RX; the missing reply
remains unresolved. A separate identity-DMA control with the same modern-only
VirtIO/`iommu_platform=on` QEMU topology also failed the ping while block
enumeration passed. This means the current failure is not isolated to dynamic
second-level mappings; modern VirtIO-net feature/receive behavior under this
topology is also implicated, but the exact fault is still unknown.
