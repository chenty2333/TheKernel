// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. Original x86_64 adapter; Linux7.2.3
// arch/x86/mm/pat/memtype.c PAT/cache_cpu_init behavior is a reference only.
//! Required CPU WC mapping support for the explicitly opted-in Intel GT.
//! Startup only, before user VMAs exist. No default PAT change. Existing
//! mappings use PAT0(WB) or PAT3(UC); only unused PAT1 is assigned WC.
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
static READY: AtomicUsize = AtomicUsize::new(0);
static FAILED: AtomicBool = AtomicBool::new(false);
fn palette(old: u64) -> Option<u64> {
    if old & 0xff != 6 || (old >> 24) & 0xff != 0 || !matches!((old >> 8) & 0xff, 1 | 4) {
        return None;
    }
    Some((old & !(0xff << 8)) | (1 << 8))
}
pub(crate) fn init(cpu: usize) {
    #[cfg(target_os = "none")]
    {
        let requested = crate::boot_command_line().and_then(|s| {
            s.split_ascii_whitespace()
                .filter_map(|s| s.strip_prefix("intel.gt="))
                .next_back()
        }) == Some("1");
        if !requested {
            return;
        }
        if cpu >= usize::BITS as usize {
            FAILED.store(true, Ordering::Release);
            return;
        }
        // INVPCID includes global translations and every PCID without changing
        // the existing PCID policy. N305 supports it; no guessed fallback.
        if core::arch::x86_64::__cpuid(0).eax < 7 {
            FAILED.store(true, Ordering::Release);
            return;
        }
        let caps = core::arch::x86_64::__cpuid(1);
        let leaf7 = core::arch::x86_64::__cpuid_count(7, 0);
        if caps.edx & (1 << 16) == 0 || leaf7.ebx & (1 << 10) == 0 {
            FAILED.store(true, Ordering::Release);
            return;
        }
        let success = x86_64::instructions::interrupts::without_interrupts(|| {
            // SAFETY: per-CPU ring0 startup, capability gated above, no user
            // mapping exists. Preserve every other PAT entry and CR0 bit.
            unsafe {
                let old = x86::msr::rdmsr(0x277);
                let Some(new) = palette(old) else {
                    return false;
                };
                if old != new {
                    let cr0: u64;
                    core::arch::asm!("mov {}, cr0",out(reg)cr0,options(nomem,nostack,preserves_flags));
                    let disabled = (cr0 | (1 << 30)) & !(1 << 29);
                    core::arch::asm!("mov cr0, {}",in(reg)disabled,options(nostack,preserves_flags));
                    core::arch::asm!("wbinvd", options(nostack, preserves_flags));
                    x86_64::instructions::tlb::flush_pcid(
                        x86_64::instructions::tlb::InvPcidCommand::All,
                    );
                    x86::msr::wrmsr(0x277, new);
                    core::arch::asm!("wbinvd", options(nostack, preserves_flags));
                    x86_64::instructions::tlb::flush_pcid(
                        x86_64::instructions::tlb::InvPcidCommand::All,
                    );
                    core::arch::asm!("mov cr0, {}",in(reg)cr0,options(nostack,preserves_flags));
                }
                x86::msr::rdmsr(0x277) == new
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
    fn palette_only_replaces_unused_index_one_and_refuses_unknown_primary_types() {
        let old = 0x0007040600070406;
        assert_eq!(palette(old), Some(0x0007040600070106));
        assert_eq!(palette(0x0007040600070106), Some(0x0007040600070106));
        for old in [old ^ 1, old | (1 << 24), old & !(0xff << 8)] {
            assert!(palette(old).is_none());
        }
    }
}
