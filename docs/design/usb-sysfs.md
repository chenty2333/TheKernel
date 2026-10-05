# Cached USB sysfs enumeration

The existing CrabUSB 0.11.0 backend already addresses devices, reads descriptors
and records topology before handing probes to TheKernel. Its public probe ID
is a core allocation ID, **not a USB address or an xHCI slot number**. A narrow
local patch exposes an immutable observation: actual USB address from the
owned coherent output context, already discovered parent ports/speed and the
existing selected-configuration cache. No command, configuration selection or
control transfer is added or changed by this read-only observation surface.
The Cargo patch replaces the existing dependency at the same version; registry
sources are not modified and no parallel USB implementation is introduced.

Native controller bus numbers are assigned logical kernel identities. Physical
child names use actual discovered port paths; addresses are not guessed from
IDs. Each real boot observation is retained even if no class driver is selected.
The current registry publishes those real devices under `/sys/bus/usb/devices`
with busnum/devnum, IDs, speed, selected config and cached descriptors. Raw
configuration bytes are already retained by usb-if and are copied verbatim.
The standard 18-byte device descriptor is encoded from its already decoded
fields; malformed larger bLength values would be normalized, not presented as
an exact original malformed packet. Unknown observation data is not synthesized.

The registry's genuine canonical paths/subsystem links and usb_device uevents
allow the signed libusb/eudev/usbutils stack to enumerate without usbfs control
access. No root-hub VID/product/descriptor is invented: this native host does
not provide a USB-device representation for its logical root port controller.
Basic lsusb is the closeout goal; virtual root-hub trees, usbfs operations,
interface/driver details and live USB hotplug remain known differences.

Initial actual QEMU USB run `shell-vg_i5pml`: signed lsusb listed all three
keyboard/mouse/tablet devices with real addresses 1/2/3, physical port nodes
1-5/1-6/1-7, VID/PID 0627:0001, speed480 and selected config1, without diagnostics.
This is QEMU enumeration, not N305 hardware acceptance. A non-interactive guest
regression verifies every identity/address/configuration blob and the actual
lsusb rows. Final validation: complete host suite passed (657 Python cases, 3 environment
skips; 6058 Rust cases, 1 existing ignored case; kernel2621), including the new
standard-device-field encoding/path/speed test. q35/n305 lint passed with 784
existing kernel warnings. KVM guest70/70 passed with no skips and normal shutdown
(`system-sm8jr8fn`). The USB-specific standalone KTAP and real signed lsusb
regression passed on actual QEMU xHCI+keyboard/mouse/tablet (`shell-3fzpnccq`,
result0); each cached descriptor contained 18 device bytes and 34 real config
bytes. No physical acceptance or USB root-hub/usbfs functionality is claimed.
