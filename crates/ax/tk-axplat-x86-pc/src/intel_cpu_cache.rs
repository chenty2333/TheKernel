// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. Original x86_64 adapter; Linux 7.2.3
// arch/x86/mm/pat/memtype.c:pat_bp_init() / pat_cpu_init() is the reference.
//! IA32_PAT programming shared by every startup CPU.
//!
//! Linux chooses one global PAT value on the boot CPU (`pat_bp_init`) and
//! every CPU writes that same value (`pat_cpu_init`). This module does the
//! same: the BSP and each AP write [`PAT_LINUX_VALUE`], and WC/UC mappings are
//! admitted only after every startup CPU has read the value back.
//!
//! Only slots 0 (WB) and 3 (UC) are relied on by existing PTE encodings, so
//! those are the invariants checked against the firmware value before a CPU
//! is changed. Slots 1 and 4..7 are rewritten to Linux's full-PAT palette.
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Linux `PAT_VALUE(WB, WC, UC_MINUS, UC, WB, WP, UC_MINUS, WT)`.
pub const PAT_LINUX_VALUE: u64 = pat_value([6, 1, 7, 0, 6, 5, 7, 4]);

/// Pack eight PAT memory-type codes into an IA32_PAT value (slot 0 lowest).
pub const fn pat_value(types: [u8; 8]) -> u64 {
    let mut value = 0u64;
    let mut slot = 0;
    while slot < 8 {
        value |= (types[slot] as u64) << (slot * 8);
        slot += 1;
    }
    value
}

static READY: AtomicUsize = AtomicUsize::new(0);
static FAILED: AtomicBool = AtomicBool::new(false);

/// Whether a firmware PAT value keeps the slots existing PTEs depend on
/// (index 0 = WB, index 3 = UC) so the palette can be installed in place.
fn firmware_palette_is_compatible(old: u64) -> bool {
    old & 0xff == 6 && (old >> 24) & 0xff == 0
}

pub(crate) fn init(cpu: usize) {
    #[cfg(target_os = "none")]
    {
        if cpu >= usize::BITS as usize {
            FAILED.store(true, Ordering::Release);
            return;
        }
        if core::arch::x86_64::__cpuid(0).eax < 1 || core::arch::x86_64::__cpuid(1).edx & (1 << 16) == 0 {
            FAILED.store(true, Ordering::Release);
            return;
        }
        let success = x86_64::instructions::interrupts::without_interrupts(|| {
            // SAFETY: per-CPU ring0 startup with no user mapping live. Only
            // PAT slots are written; CR0/CR3/CR4 are untouched.
            unsafe {
                let old = x86::msr::rdmsr(0x277);
                if !firmware_palette_is_compatible(old) {
                    return false;
                }
                if old != PAT_LINUX_VALUE {
                    // Write back dirty lines before any slot that they may be
                    // cached under changes meaning, then drop stale TLB state.
                    core::arch::asm!("wbinvd", options(nostack, preserves_flags));
                    x86::msr::wrmsr(0x277, PAT_LINUX_VALUE);
                    core::arch::asm!("wbinvd", options(nostack, preserves_flags));
                    x86_64::instructions::tlb::flush_all();
                }
                x86::msr::rdmsr(0x277) == PAT_LINUX_VALUE
            }
        });
        if success {
            READY.fetch_or(1usize << cpu, Ordering::AcqRel);
        } else {
            FAILED.store(true, Ordering::Release);
        }
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = cpu;
    }
}

/// True once every startup CPU has the Linux palette installed.
pub(crate) fn ready() -> bool {
    #[cfg(feature = "smp")]
    let count = crate::cpu::cpu_num().min(crate::config::plat::MAX_CPU_NUM);
    #[cfg(not(feature = "smp"))]
    let count = 1usize;
    count > 0
        && count < usize::BITS as usize
        && !FAILED.load(Ordering::Acquire)
        && READY.load(Ordering::Acquire) == (1usize << count) - 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_matches_linux_full_pat_layout() {
        assert_eq!(PAT_LINUX_VALUE, 0x0407_0506_0007_0106);
        assert_eq!(PAT_LINUX_VALUE & 0xff, 6);
        assert_eq!((PAT_LINUX_VALUE >> 8) & 0xff, 1);
        assert_eq!((PAT_LINUX_VALUE >> 24) & 0xff, 0);
    }

    #[test]
    fn firmware_check_requires_wb_slot0_and_uc_slot3() {
        assert!(firmware_palette_is_compatible(0x0007_0406_0007_0406));
        assert!(!firmware_palette_is_compatible(0x0007_0406_0007_0400));
        assert!(!firmware_palette_is_compatible(0x0007_0406_0107_0406));
        assert!(!firmware_palette_is_compatible(0x0007_0406_0007_0407));
    }
}
