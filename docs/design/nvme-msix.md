# NVMe MSI-X completion ownership

2026-10-04. Original Rust, PCI MSI-X/NVMe register facts; Linux 7.2.3
`drivers/nvme/host/pci.c` was consulted for CQ IEN/vector semantics.
**未在硬件上验证**; QEMU KVM delivers real guest MSI-X messages in this test.

The x86 platform reserves a handler-table vector strictly above every
implemented IOAPIC pin's vector and below the LAPIC/IPI reserved range.
BSP destination must be representable without interrupt remapping. No IOAPIC
entry is enabled or repurposed. Ownership lasts until reboot: delayed device
messages cannot target a replacement handler, including failed initialization.

Native PCI capability access is bounded and halfword-correct. Probe first
masks/disables inherited MSI-X state. It checks the entire capability table
against its memory BAR, masks all entries/function delivery, installs one
permanent vector and its entry zero message, then creates the controller/CQs.
Admin and both I/O CQs use vector zero; I/O CQ IEN is set only with an admitted
route. After successful initialization, entry zero/function delivery unmask.
Failure leaves the function masked and DMA follows the driver's reset/retain
contract. INTx remains disabled. `nvme.poll=1` explicitly tests fallback.

IRQ context only increments an atomic generation; the existing IRQ-safe task
waker registry performs wake delivery. The single task-context CQ owner keeps
phase/CID/SQID verification and CQ-head acknowledgment. Check-register-check
prevents a missed wake. A bounded timer, repeated CQ reads and an absolute
five-second command deadline preserve progress if an IRQ is missing, IRQs are
disabled during early boot, or a task cannot sleep. No separate IRQ ring drain
races the synchronous owner. RO/write gating is unchanged.

Host tests validate table bounds, exclusion of IOAPIC/LAPIC vectors, IEN/vector
encoding, delayed IRQ completion and lost-interrupt polling with identical
payloads/CQ ownership. QEMU GPT RO/RW tests observed a completion wake on vector
0xee, verified content and write protection; forced polling also passed.
Actual N305 MSI-X delivery and any IOMMU/interrupt-remapping environment remain
unverified. Coherent identity DMA is still the platform's explicit contract.
