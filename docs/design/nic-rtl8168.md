# RTL8168H / shared r8169-family core

**未在硬件上验证.** N305 capture measured PCI `10ec:8168` rev 0x15, BAR2
`0x80504000`, 4 KiB. Revision 0x15 is not treated as the MAC identity. Probe
prints raw TxConfig and XID, admits `(XID & 0x7cf) == 0x541` only for PCI
8168, and rejects other XIDs before reset. RTL8125B still requires PCI 8125
and masked XID 0x641. The table does not admit other 8168 generations or M.

## Organization and behavior

The existing RTL8125 DMA/descriptor/loan implementation is extracted into
`tk-axdriver-net/src/r8169`; it is not duplicated. Separate H MAC and PHY
configuration uses bounded ERI/EPHY/OCP transactions. The 8168 mapping is
4 KiB, its mask/status are words at 0x3c/0x3e, and TX uses a byte at 0x38
(bit 6). No 8125-only queue/RSS/coalescing register is written on H.
High DMA address halves precede low halves and enable; RX strips FCS, and
TX zero-pads short frames. Failed register transactions stop initialization,
restore the config lock and do not enable DMA. Reset failure retains DMA
allocations rather than freeing potentially live memory.

N305 builds select the family driver automatically. `--net-rtl8168` enables
it on QEMU and uses a separate artifact variant. `net-rtl8168` propagates
through root/kernel/axfeat/axdriver features. The platform seam remains in
`tk-axdriver/src/rtl8125.rs` to keep shared registration files additive.

With a usable single-message MSI capability and eight-bit APIC destination,
the platform installs a status-only IRQ handler before unmasking RX/TX/link
interrupts. No locking, logging or descriptor access occurs in the handler.
The network IRQ hook wakes the ordinary network worker. MSI allocation or
configuration failure leaves polling enabled; **10 ms polling is retained
also with MSI** to guard against lost native interrupts. MSI-X is not needed
on this single-queue path (the measured device supplies MSI as well). MSI
and all real transfer behavior remain hardware-unverified.

## Rootfs-only firmware

After rootfs mount and before network publication, the runtime reads
`/lib/firmware/rtl_nic/rtl8168h-2.fw` (64 KiB cap). Header/checksum, opcode
and branch bounds are checked before MAC reset. Execution has instruction
and cumulative-delay budgets; timeouts propagate instead of spinning.
Firmware is **not embedded in Rust/ELF**. Successful upload is followed by
H PHY calibration, normal duplex advertisement and auto-negotiation restart.
A missing file prints `degraded warm-PXE PHY only`; the already running PXE
PHY state is preserved. This is a fallback, not a cold-boot guarantee.
Malformed firmware is rejected before reset; a runtime transaction failure
can leave the MAC stopped and prints a clear failure. No interface success
or DHCP claim is inferred from such a message.

Optional image staging (no root required):

```sh
export THEKERNEL_STATE_DIR=/home/ava/.cache/thekernel-targets/wt-dev
export THEKERNEL_RTL8168_FIRMWARE_DIR=/home/ava/.cache/thekernel-targets/refs/firmware/rtl_nic
python3 tools/thekernel.py build --platform n305 --profile shell
```

The input directory must contain `rtl8168h-2.fw` **and** `LICENSE.r8169`;
rootfs reuse accounts for both files' contents. Missing notice fails staging.
The supplied linux-firmware notice is © 2011–2013 Realtek Semiconductor
Corporation; distribution is allowed in hexadecimal/equivalent format with
that notice accompanying the data. This is firmware-specific permission,
not the Rust license and not the WiFi firmware license. Retain the notice
in the rootfs when distributing the image. Linux behavior was consulted,
not translated or quoted into the original Rust implementation.

## Validation boundaries

Host fake coverage: identities, BAR-sized register separation, word IRQ and
byte doorbell, bounded indirect/reset faults, lock restoration, IRQ unmask
ordering, real byte payload/padding through the shared rings, RX FCS, ring
wrap and loans, firmware pages/opcodes/malformed jumps/loop and delay limits.
QEMU has no RTL8168 model: a boot can test the feature wiring and absent-device
rejection, **not** native DHCP or MSI or PHY silicon behavior.

Next real PXE boot must record XID, firmware path/result, link and DHCP lease;
then start `thekernel-netconsole HOST PORT` and verify host reception of actual
log bytes. The relay is userspace best-effort; it cannot send pre-init/panic
logs. A down link is a failure to diagnose, not proof that firmware is absent.

## Reference facts

Linux 7.2.3 `drivers/net/ethernet/realtek/r8169_main.c`: `rtl_chip_infos`,
`rtl_hw_start_8168h_1`, ERI/EPHY/MAC/PHY OCP accessors, `rtl_init_rxcfg`,
`rtl_hw_start_8168`; `r8169_phy_config.c`: `rtl8168h_2_hw_phy_config`;
`r8169_firmware.c`: runtime bytecode format and opcodes. Firmware permission
is supplied in host linux-firmware `LICENSE.r8169`, copied alongside the
reference bytecode and staged unchanged with it.

Measured on dev for this change (2026-10-04): focused family tests **14/14**,
including parsing/executing the supplied 976-byte firmware against FakeBus;
full host suite succeeds (Python 623, 3 existing skips; kernel host 2538);
KVM guest **52/52**, clean exit; default and N305 lint succeed with existing
warnings and no new warnings on changed lines. The new family scope's Linux
excerpt scan finds zero matching lines. N305-profile QEMU shell boot rejected
the absent Realtek device, read the staged 976-byte firmware and notice, and
exited cleanly with `poweroff -f`. No result here establishes native DHCP,
PHY calibration or MSI on N305 silicon.
