# RTL8125B/BG: minimal polling MAC, warm PHY only

**Status: 未在硬件上验证.** The N305 NIC is still unknown. `10ec:8125`
is one possible device, not an observed device. Only TxConfig's B/BG MAC
revision (`(TxConfig >> 20) & 0x7cf == 0x641`) is admitted. An A/D/CP/BP/K
revision is identified in the inventory and refused before reset, not driven
with a guessed B sequence.

## Implemented

The mechanism crate has separate `ids`, `regs`, `desc`, `bringup`, `probe`,
`nic` and `fake` modules. The platform seam maps BAR2 and supplies coherent
x86_64 DMA allocations. `net-n305` compiles igc, RTL8125 and VirtIO together;
PCI identity chooses the driver, without making all other device classes
use the dynamic driver framework. N305 builds select this feature by default.

The MAC path reads identity/MAC first, masks 8125's **32-bit** interrupt mask,
performs a bounded reset, disables RSS/multiple queues and new descriptor
format, publishes 256 TX/RX descriptors, and enables a polling MAC. TX pads
short frames with zeroes and uses OWN/FIRST/LAST/RING_END. RX checks ownership,
errors, complete frames and length, removes FCS, and rearms only after the
stack returns the exact buffer loan. No interrupt, checksum/segmentation
or VLAN offload is advertised.

The network stack consumes the normal `NetDriverOps` interface, as it does
VirtIO and igc. A native device with no IRQ is serviced by the network worker
with an explicit 10 ms polling deadline, not admitted as an unwakeable ring.
The guest's BusyBox DHCP client and lease hook configure
`eth0`; DHCP is not implemented in the NIC. Kernel logs can be relayed with
`thekernel-netconsole HOST PORT`, using syslog's reader cursor and UDP. This
is a userspace, best-effort relay: it needs a running userspace/network and
cannot promise early-boot, panic, lossless or IRQ-time delivery.

## The important omission

This is **not** Linux r8169's complete cold-PHY setup. The PHY and its firmware
state are inherited from PXE/UEFI; the driver does not apply Linux's revision-
specific EPHY/OCP workaround tables or upload PHY microcode. It prints that
limitation before resetting the MAC. A down link is not repaired by guessing
PHY values; DHCP can time out and the screen stays available. Therefore
"native DHCP works" is not established by these changes.

No firmware is embedded in the ELF. The reference files are under
`/home/ava/.cache/thekernel-targets/refs/firmware/rtl_nic/`; Fedora supplies them
in `linux-firmware`, with `/usr/share/licenses/linux-firmware/LICENCE.rtlwifi_firmware.txt`.
A future cold-PHY path must load the appropriate file from `/lib/firmware/rtl_nic/`
after rootfs is mounted and validate its format before executing it. This
path is **not implemented**; a missing file must not become fabricated success.
The Alpine capture boot uses Alpine's signed firmware packages at runtime.

## Safety and tests

All four allocations must succeed before publication. Failed allocation frees
its unpublished prefix. Reset failure is bounded; unconfirmed DMA stop or live
CPU packet loans retain memory instead of returning it to the allocator.
Fallible publication of a native NIC reports an allocation failure and drops
it through that same stop path. No display register is touched by this driver.

Fake tests cover ID/revision rejection, descriptor bits/FCS/errors, 64-bit DMA
addresses and high-before-low programming, mask/enable ordering, bounded reset,
ring-full and wrap/recycle, duplicate loans, allocation-prefix cleanup and
DMA retention on failed reset. These tests prove decisions against a model,
not the controller's silicon behavior. QEMU has no RTL8125 or igc model;
its negative probes prove only that absent devices are not touched.

## Sources (facts only, no translated Linux implementation)

Linux 7.2.3 `drivers/net/ethernet/realtek/r8169_main.c`: identification table
at 126; register layout 257–315 and 433–455; descriptor fields 583–660;
reset 2673–2679; RX/TX configuration 2594–2617 and 2790–2815; old descriptor
selection and single queue 3866–3886; coalescing 4043–4061; enabling 4131–4136;
TX doorbell 4554; RX validation/FCS 4790–4823. The omitted PHY initialization
is in `r8169_phy_config.c`; firmware format/loading is in `r8169_firmware.c`.
