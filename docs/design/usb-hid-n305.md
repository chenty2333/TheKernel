# N305 USB HID preparation

**CH9329 未在硬件上验证.** The actual report descriptors and selected CH9329
relative/absolute mode are unknown until the hardware capture.

USB configuration is now selected once per physical device. Every supported
alternate-setting-zero interface is claimed separately, retaining the shared
Device owner. A composite keyboard+pointer is no longer reduced to its first
interface. Boot keyboards select boot protocol; boot-subclass pointers select
report protocol, and non-boot pointers use their report descriptor.

The bounded HID 1.11 short-item parser handles pointer application/physical
collections, button variables, X/Y relative or absolute fields, wheel fields,
report IDs and global push/pop. Logical ranges feed EV_ABS information; input
property POINTER is exposed for absolute pointers. Truncated/unknown reports
are ignored. Long items, excessive fields/report bits and unsupported layouts
are refused rather than guessed. This is not a universal HID interpreter or
an NKRO/non-boot keyboard implementation.

Host tests cover signed relative motion, absolute coordinates, independent
report IDs, bounds and truncated descriptors/reports. The QEMU USB topology
contains `usb-kbd`, `usb-mouse` and `usb-tablet`; positive enumeration and
actual input checks are listed in `n305-tonight.md`. None confirms CH9329's
firmware, USB scheduling or report layout. The existing xHCI path still has a
fatal halt-confirmation assertion if the controller cannot stop DMA; that
exceptional recovery behavior has not been redesigned in this preparation.
