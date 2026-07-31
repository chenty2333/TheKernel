# axplat-bcm2837-zero2w

ArceOS platform implementation for the **Raspberry Pi Zero 2W** (RP3A0
system-in-package, BCM2710A1 die — the same BCM2837 die as the Raspberry Pi 3:
quad Cortex-A53, 512 MB LPDDR2, VideoCore IV GPU).

## Status

This is a bring-up platform package. It provides:

- PL011 UART0 console (115200 baud)
- ARM generic timer (CNTPCT / CNTPNSIRQ)
- BCM2836 CPU-local interrupt controller + BCM2835-style banked ARM control
  interrupt controller (peripheral IRQ 0..=95, timer on the CPU-local domain)
- Flat early page table (identity + `0xffff_0000_0000_0000` linear mapping)
- Watchdog reboot (`system_off` falls back to reboot; the SoC has no true
  power-down)
- Single-CPU boot (SMP is not implemented yet)

The boot flow requires the closed-source GPU firmware chain
(`bootcode.bin` -> `start.elf` -> `kernel8.img` at `0x0008_0000`), which is
the standard Zero 2W boot path; use the `mingo` UART chainloader from
`os/arceos/tools/raspi4/chainloader` for fast iteration.

## Interfaces

One crate, two platform contracts (mutually exclusive features):

| Feature | Contract | Consumers |
|---|---|---|
| *(default)* | `ax-plat` (tgoskits) | `ax-hal` / `axruntime` via `AX_PLATFORM_CRATE=axplat_bcm2837_zero2w` |
| `legacy` | crates.io `axplat` 0.3.x | TheKernel (add the package + a platform feature) |

## Hardware notes

- Peripheral base (ARM view): `0x3f00_0000`; GPU view is `0x7e00_0000`.
- Kernel image load address: `0x0008_0000` (firmware), linked at
  `0xffff_0000_0008_0000` with `phys-virt-offset = 0xffff_0000_0000_0000`.
- The interrupt controller is **not** a GIC: peripherals use the banked
  armctrl controller (with the bank-0 shortcut aliases) chained under the
  BCM2836 CPU-local controller.
- A page-aligned region at `0x0100_0000` is reserved for the per-CPU runtime
  areas (ax-percpu); it is excluded from RAM allocation.
- V3D (`0x3fc0_0000`, IRQ 42) and HVS (`0x3f40_0000`, IRQ 65) addresses are
  published in `config.rs` for the companion driver crates
  `bcm283x-v3d` / `bcm283x-hvs`.

## Build

```sh
# tgoskits interface
cargo check -p axplat-bcm2837-zero2w --target aarch64-unknown-none-softfloat --features irq

# TheKernel legacy interface
cargo check -p axplat-bcm2837-zero2w --target aarch64-unknown-none-softfloat --features legacy
```
