# NVMe MSI-X completion ownership

2026-10-04. Original Rust, PCI MSI-X/NVMe register facts; Linux 7.2.3
`drivers/nvme/host/pci.c` was consulted for CQ IEN/vector semantics.
**未在硬件上验证**; QEMU KVM delivers real guest MSI-X messages in this test.

The x86 platform reserves a handler-table vector strictly above every
implemented IOAPIC pin's vector and below the LAPIC/IPI reserved range.
No IOAPIC entry is enabled or repurposed. The requester identity crosses the
existing VT-d interrupt-remapping boundary when that platform mode is active.
The dynamic IRQ handle owns the callback, native vector and remapping entry.
Teardown masks/readbacks the device's MSI-X entry, synchronizes in-flight
callbacks and trap boundaries, and only then releases the route. A failed
remapping release retains ownership rather than allowing vector reuse.

Native PCI capability access is bounded and halfword-correct. Probe first
masks/disables inherited MSI-X state. It checks the entire capability table
against its memory BAR, masks all entries/function delivery, installs one
owned dynamic vector and its entry zero message before creating controller/CQs.
Admin and both I/O CQs use vector zero; I/O CQ IEN is set only with an admitted
route. The callback's per-device state is installed before entry zero/function
delivery unmask, including initialization-time events. Failure drops the route
only after masking its entry; DMA follows the driver's reset/retain contract.
INTx remains disabled. `nvme.poll=1` explicitly selects polling.

IRQ context increments a per-device atomic generation and invokes the bounded
shared-runtime notifier; it never drains CQ or accesses DMA owners. Task context
keeps phase/CID/SQID verification and CQ-head acknowledgment. Check-register-check
prevents missed wake registration. In serviceable IRQ mode CQ is inspected only after an
event generation changes; bootstrap owners with interrupts masked explicitly
inspect CQ, before sharing the controller; an absolute five-second command deadline fails closed
on lost delivery instead of polling the IRQ-owned queue. Explicit polling mode
and route-less devices use bounded CQ reads. RO/write gating is unchanged.

The original 2026-10-04 validation below predates dynamic route ownership and
owned batches; it is historical transport coverage, not acceptance of new code.

Host tests validate table bounds, exclusion of IOAPIC/LAPIC vectors, IEN/vector
encoding, delayed IRQ completion and lost-interrupt polling with identical
payloads/CQ ownership. QEMU GPT RO/RW tests observed a completion wake on vector
0xee, verified content and write protection; forced polling also passed.
Actual N305 MSI-X delivery and any IOMMU/interrupt-remapping environment remain
unverified. Coherent identity DMA is still the platform's explicit contract.
