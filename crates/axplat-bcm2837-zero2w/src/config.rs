//! BCM2710A1 (BCM2837 die, Raspberry Pi Zero 2W) hardware constants.
//!
//! All addresses are ARM-view physical addresses. The GPU-view peripherals
//! live at `0x7e00_0000`; the ARM alias used by BCM2836/2837 is `0x3f00_0000`.
//!
//! Constants for peripherals not yet driven by this crate (V3D, HVS, eMMC,
//! mailbox) are kept for the companion driver crates and for documentation.

#![allow(dead_code)]

/// ARM-view peripheral base address of BCM2836/2837.
pub const PERIPHERAL_BASE: usize = 0x3f00_0000;
/// Size of the whole peripheral window (16 MB, up to 0x4000_0000).
pub const PERIPHERAL_SIZE: usize = 0x100_0000;

/// Linear mapping offset between virtual and physical addresses.
pub const PHYS_VIRT_OFFSET: usize = 0xffff_0000_0000_0000;

/// Physical address of the kernel image (firmware load point, kernel8.img).
pub const KERNEL_BASE_PADDR: usize = 0x0008_0000;
/// Virtual address of the kernel image.
pub const KERNEL_BASE_VADDR: usize = PHYS_VIRT_OFFSET | KERNEL_BASE_PADDR;

/// Base of the whole physical RAM.
pub const PHYS_MEMORY_BASE: usize = 0x0;
/// Size of the whole physical RAM (512 MB on Zero 2W).
pub const PHYS_MEMORY_SIZE: usize = 0x2000_0000;

/// Kernel address space base and size (covers RAM + peripherals).
pub const KERNEL_ASPACE_BASE: usize = PHYS_VIRT_OFFSET;
pub const KERNEL_ASPACE_SIZE: usize = 0x1_0000_0000;

/// Boot stack size (also used as the per-CPU stack size).
pub const BOOT_STACK_SIZE: usize = 0x40000;

/// Number of CPU cores on the platform (quad Cortex-A53).
pub const MAX_CPU_NUM: usize = 4;

/// PL011 UART0.
pub const UART0_PADDR: usize = 0x3f20_1000;
pub const UART0_IRQ: usize = 89; // armctrl bank 2, bit 25

/// BCM2836 CPU-local interrupt controller.
pub const LOCAL_INTC_PADDR: usize = 0x3f00_b000;
/// BCM2835-style banked ARM control interrupt controller.
pub const ARMCTRL_IC_PADDR: usize = 0x3f00_b200;

/// V3D 3D engine.
pub const V3D_PADDR: usize = 0x3fc0_0000;
pub const V3D_IRQ: usize = 42; // armctrl bank 1, bit 10 (shortcut bit 12)

/// HVS display controller.
pub const HVS_PADDR: usize = 0x3f40_0000;
pub const HVS_IRQ: usize = 65; // armctrl bank 2, bit 1

/// Clock manager (CPRMAN).
pub const CPRMAN_PADDR: usize = 0x3f10_1000;

/// eMMC/SDHCI.
pub const SDHCI_PADDR: usize = 0x3f30_0000;
pub const SDHCI_IRQ: usize = 94; // armctrl bank 2, bit 30 (shortcut bit 20)

/// Mailbox (firmware interface).
pub const MBOX_PADDR: usize = 0x3f00_b880;

/// ARM generic timer frequency (Hz) on BCM2837. The firmware programs the
/// CNTFRQ at boot; read it at runtime instead of trusting this value.
pub const GENERIC_TIMER_FREQ: u64 = 19_200_000;

// ---------------------------------------------------------------------------
// BCM2836 local interrupt controller registers (base LOCAL_INTC_PADDR).
// ---------------------------------------------------------------------------
pub const LOCAL_CONTROL: usize = 0x000;
pub const LOCAL_TIMER_INT_CONTROL0: usize = 0x040;
pub const LOCAL_IRQ_PENDING0: usize = 0x060;
pub const LOCAL_MAILBOX0_SET0: usize = 0x080;
pub const LOCAL_MAILBOX0_CLR0: usize = 0x0c0;

// Local IRQ numbers (hwirq of the local domain).
pub const LOCAL_IRQ_CNTPSIRQ: u32 = 0;
pub const LOCAL_IRQ_CNTPNSIRQ: u32 = 1; // physical non-secure timer
pub const LOCAL_IRQ_CNTHPIRQ: u32 = 2;
pub const LOCAL_IRQ_CNTVIRQ: u32 = 3;
pub const LOCAL_IRQ_GPU_FAST: u32 = 8; // chained armctrl-ic
pub const LOCAL_IRQ_PMU_FAST: u32 = 9;

// ---------------------------------------------------------------------------
// BCM2835-style ARM control interrupt controller registers (base
// ARMCTRL_IC_PADDR). Three banks of 32 interrupts; bank 0 has 8 valid
// "basic" bits (0-7) plus shortcut and overflow bits (8-9).
// ---------------------------------------------------------------------------
pub const ARMC_BANK0_PENDING: usize = 0x000;
pub const ARMC_BANK1_PENDING: usize = 0x004;
pub const ARMC_BANK2_PENDING: usize = 0x008;
pub const ARMC_BANK1_ENABLE: usize = 0x010;
pub const ARMC_BANK2_ENABLE: usize = 0x014;
pub const ARMC_BANK0_ENABLE: usize = 0x018;
pub const ARMC_BANK1_DISABLE: usize = 0x01c;
pub const ARMC_BANK2_DISABLE: usize = 0x020;
pub const ARMC_BANK0_DISABLE: usize = 0x024;

/// Shortcut bit list: bank 0 bits 10-20 alias bank 1 bits 7/9/10/18/19 and
/// bank 2 bits 21/22/23/24/25/30. These bits set the bank-0 alias without
/// setting the bank-overflow bits 8/9.
pub const SHORTCUT_BANK1_BITS: [u8; 5] = [7, 9, 10, 18, 19];
pub const SHORTCUT_BANK2_BITS: [u8; 6] = [21, 22, 23, 24, 25, 30];
pub const SHORTCUT_MASK1: u32 = 0x0000_7c00; // bank 0 bits 10-14
pub const SHORTCUT_MASK2: u32 = 0x001f_8000; // bank 0 bits 15-20
pub const BANK0_VALID_MASK: u32 = 0x0000_03ff; // basic bits 0-7 + overflow 8-9

/// CPRMAN clock manager registers (base CPRMAN_PADDR).
pub const CM_V3DCTL: usize = 0x038;
pub const CM_V3DDIV: usize = 0x03c;

/// Broadcom "magic" value required for writes to the power/clock registers.
pub const PM_PASSWORD: u32 = 0x5a00_0000;

/// BCM2835 power management (system reset / shutdown).
pub const PM_PADDR: usize = 0x3f10_0000;
pub const PM_WDOG: usize = 0x024;
pub const PM_RSTC: usize = 0x01c;
pub const PM_RSTC_WRCFG_FULL_RESET: u32 = 0x0000_0020;
