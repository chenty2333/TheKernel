//! x86 specific page table structures.

use memory_addr::VirtAddr;
use page_table_entry::x86_64::X64PTE;
#[cfg(all(feature = "asid-fast-switch", target_os = "none"))]
use x86_64::{
    registers::control::{Cr3, Cr3Flags, Cr4, Cr4Flags},
    structures::paging::PhysFrame,
};

use crate::{PageTable64, PageTable64Cursor, PagingMetaData};

#[cfg(all(feature = "asid-fast-switch", target_os = "none"))]
#[inline]
fn pcide_enabled() -> bool {
    Cr4::read().contains(Cr4Flags::PCID)
}

#[cfg(all(feature = "asid-fast-switch", not(target_os = "none")))]
#[inline]
#[allow(dead_code)]
const fn pcide_enabled() -> bool {
    // Host tests cannot read privileged control registers.  In particular,
    // do not turn a host CR4 read into an accidental PCID capability claim.
    false
}

#[cfg(all(feature = "asid-fast-switch", target_os = "none"))]
#[inline]
fn pcid_invpcid_enabled() -> bool {
    let invpcid = x86::cpuid::CpuId::new()
        .get_extended_feature_info()
        .is_some_and(|features| features.has_invpcid());
    invpcid && pcide_enabled()
}

#[cfg(all(feature = "asid-fast-switch", not(target_os = "none")))]
#[inline]
const fn pcid_invpcid_enabled() -> bool {
    // Host tests cannot execute INVPCID or read privileged CR4.
    false
}

/// Returns to PCID 0 before clearing PCIDE, making an ordinary CR3 reload a
/// valid full-flush fallback for a pre-enabled CPU without INVPCID.
#[cfg(all(feature = "asid-fast-switch", target_os = "none"))]
fn disable_pcide_safely() -> bool {
    let current_cr3 = unsafe { x86::controlregs::cr3() } as usize;
    let root = current_cr3 & !0xfff;
    if root >= (1usize << 52) {
        return false;
    }
    let frame = PhysFrame::containing_address(x86_64::PhysAddr::new_truncate(root as u64));
    // SAFETY: retain the current root, select PCID 0, and leave NOFLUSH clear
    // while PCIDE is still enabled.  This performs the architectural
    // non-global invalidation before PCIDE is cleared.
    unsafe { Cr3::write(frame, Cr3Flags::empty()) };
    if (unsafe { x86::controlregs::cr3() } as usize) & 0xfff != 0 {
        return false;
    }
    let mut cr4 = Cr4::read();
    cr4.remove(Cr4Flags::PCID);
    // SAFETY: only CR4.PCIDE changes; paging remains enabled.
    unsafe { Cr4::write(cr4) };
    !pcide_enabled()
}

/// metadata of x86_64 page tables.
pub struct X64PagingMetaData;

impl PagingMetaData for X64PagingMetaData {
    const LEVELS: usize = 4;
    const PA_MAX_BITS: usize = 52;
    const VA_MAX_BITS: usize = 48;

    type VirtAddr = VirtAddr;

    #[inline]
    fn flush_tlb(vaddr: Option<VirtAddr>) {
        #[cfg(target_os = "none")]
        {
            if let Some(vaddr) = vaddr {
                // SAFETY: this target-specific operation is only compiled for
                // the kernel's ring-0 execution environment.
                unsafe { x86::tlb::flush(vaddr.into()) };
            } else {
                #[cfg(feature = "asid-fast-switch")]
                {
                    if pcid_invpcid_enabled() {
                        // SAFETY: the CPUID and CR4 checks above guarantee
                        // that INVPCID is implemented and PCIDE is enabled
                        // locally.
                        unsafe {
                            x86_64::instructions::tlb::flush_pcid(
                                x86_64::instructions::tlb::InvPcidCommand::AllExceptGlobal,
                            )
                        };
                    } else {
                        if pcide_enabled() && !disable_pcide_safely() {
                            panic!("cannot disable PCIDE before a full TLB flush");
                        }
                        // This reload is reached only after PCIDE is known to
                        // be clear.
                        unsafe { x86::tlb::flush_all() };
                    }
                }
                #[cfg(not(feature = "asid-fast-switch"))]
                // SAFETY: this target-specific operation is only compiled
                // for the kernel's ring-0 execution environment.
                unsafe {
                    x86::tlb::flush_all()
                };
            }
        }
        #[cfg(not(target_os = "none"))]
        {
            // Hosted callers may exercise the page-table metadata API, but
            // must never execute INVLPG, INVPCID, or a CR3 reload.
            let _ = vaddr;
        }
    }
}

#[cfg(all(test, feature = "asid-fast-switch", not(target_os = "none")))]
mod tests {
    use memory_addr::VirtAddr;

    use super::{X64PagingMetaData, pcid_invpcid_enabled, pcide_enabled};
    use crate::PagingMetaData;

    #[test]
    fn host_pcid_probe_never_reads_or_claims_privileged_cr4() {
        assert!(!pcide_enabled());
        assert!(!pcid_invpcid_enabled());
    }

    #[test]
    fn hosted_flush_tlb_calls_are_policy_safe_noops() {
        <X64PagingMetaData as PagingMetaData>::flush_tlb(Some(VirtAddr::from_usize(0x4000)));
        <X64PagingMetaData as PagingMetaData>::flush_tlb(None);
    }
}

/// x86_64 page table.
pub type X64PageTable<H> = PageTable64<X64PagingMetaData, X64PTE, H>;
/// x86_64 page table cursor.
pub type X64PageTableCursor<'a, H> = PageTable64Cursor<'a, X64PagingMetaData, X64PTE, H>;
