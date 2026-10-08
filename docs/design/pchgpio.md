# Intel PCH GPIO provider

`kernel/src/acpi/pchgpio.rs` translates OpenBSD `sys/dev/acpi/pchgpio.c` rev 1.19 (ISC), including all SPT, CNL, TGL, ADL-S, ADL-N and MTL community/pad tables and the `INTC1057` HID mapping. It implements PADBAR discovery, group/pin lookup, RX/TX access, interrupt establish/enable/disable and status dispatch, and per-pin suspend-state save/restore data.

ACPI enumeration resolves controller `_CRS` assigned memory windows and IRQ resources. MMIO windows are mapped through the kernel address space; a GPIO controller is registered only when its table, BARs and directly routable GSI validate. The x86 route currently refuses legacy GSIs, MADT override ambiguity, multi-IOAPIC topologies, and unsupported vectors rather than guessing a mapping. I2C-HID requests its ACPI `GpioInt` pin; hard IRQ handling only acknowledges/masks and sets an atomic pending bit, while `GET_INPUT` remains in the existing task/input read path. Adaptive polling remains a fallback. GPIO suspend/restore methods are present but are not yet called by an ACPI S3 lifecycle (the current kernel has no enabled S3 transition path).

Tests validate the full HID table, ADL-N pin group boundaries and register offsets using an in-memory BAR model, plus ACPI memory-resource decoding. No N305 physical GPIO or I2C-HID hardware acceptance was available.
