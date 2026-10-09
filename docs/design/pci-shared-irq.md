# Shared PCI interrupt dispatch

The x86 platform now admits up to sixteen immutable shared-source dispatchers.
Each is invoked before direct vector handlers on every interrupt, and all
callbacks run even after one claims the vector. This preserves the existing
VirtIO ISR-latch ordering while allowing independent block-device and other
PCI INTx owners to acknowledge their own level-triggered sources. Registration
is bounded, lock-free, and permanent for the boot; callbacks must remain
allocation-free and non-blocking. PCI INTx routing/polarity remains a separate
firmware/IOAPIC operation. MSI/MSI-X vectors are direct handlers and do not
need the shared-source registry.

The callback table is a platform dispatch facility, not yet a hotplug lifecycle:
there is no removal API. Device removal must first mask and acknowledge the
device, then synchronize in-flight callbacks before its endpoint context can be
reused.
