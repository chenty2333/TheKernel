//! Wrapper functions for assembly instructions.

use core::arch::asm;
#[cfg(feature = "asid-fast-switch")]
use core::sync::atomic::{AtomicUsize, Ordering};

#[cfg(target_os = "none")]
use memory_addr::MemoryAddr;
use memory_addr::{PhysAddr, VirtAddr};
#[cfg(target_os = "none")]
use x86::controlregs;
use x86::msr;
#[cfg(target_os = "none")]
use x86::tlb;
use x86_64::instructions::interrupts;
#[cfg(all(feature = "asid-fast-switch", target_os = "none"))]
use x86_64::instructions::tlb as x86_64_tlb;
#[cfg(all(target_os = "none", any(feature = "asid-fast-switch", feature = "pkeys")))]
use x86_64::registers::control::{Cr4, Cr4Flags};
#[cfg(target_os = "none")]
use x86_64::{
    registers::control::{Cr3, Cr3Flags},
    structures::paging::PhysFrame,
};

#[cfg(feature = "asid-fast-switch")]
static PCID_CPUS_ENABLED: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "asid-fast-switch")]
static PCID_CPUS_FAILED: AtomicUsize = AtomicUsize::new(0);

/// Architectural user-CET state owned by one schedulable task.  CET is
/// switched explicitly, independently from PKRU and XSAVE state.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UserCetState {
    /// IA32_U_CET value.
    pub u_cet: u64,
    /// IA32_PL3_SSP value.
    pub pl3_ssp: u64,
    /// Software ABI lock state.
    pub locked: bool,
}

#[cfg(target_os = "none")]
const IA32_U_CET: u32 = 0x6a0;
#[cfg(target_os = "none")]
const IA32_PL3_SSP: u32 = 0x6a7;

/// Whether this CPU has enabled user shadow-stack support. Hosted builds must
/// always return false: touching privileged CET state there would be invalid.
#[inline]
pub fn user_shadow_stack_enabled() -> bool {
    #[cfg(target_os = "none")]
    {
        let cpuid = core::arch::x86_64::__cpuid_count(7, 0);
        return cpuid.ecx & (1 << 7) != 0
            && x86::controlregs::cr4() & (1 << 23) != 0;
    }
    #[cfg(not(target_os = "none"))]
    false
}

/// Enables CR4.CET on capable CPUs only. It does not enable CET for a task.
pub fn init_user_shadow_stack() {
    #[cfg(target_os = "none")]
    {
        let cpuid = core::arch::x86_64::__cpuid_count(7, 0);
        if cpuid.ecx & (1 << 7) != 0 {
            unsafe { x86::controlregs::cr4_write(x86::controlregs::cr4() | (1 << 23)) };
        }
    }
}

/// Reads the user CET MSRs when CET is active on this CPU.
#[inline]
pub fn read_user_cet_state() -> UserCetState {
    #[cfg(target_os = "none")]
    if user_shadow_stack_enabled() {
        return UserCetState {
            u_cet: unsafe { msr::rdmsr(IA32_U_CET) },
            pl3_ssp: unsafe { msr::rdmsr(IA32_PL3_SSP) },
            locked: false,
        };
    }
    UserCetState::default()
}

/// Writes the user CET MSRs when CET is active on this CPU.
#[inline]
pub fn write_user_cet_state(state: UserCetState) {
    #[cfg(target_os = "none")]
    if user_shadow_stack_enabled() {
        unsafe {
            msr::wrmsr(IA32_PL3_SSP, state.pl3_ssp);
            msr::wrmsr(IA32_U_CET, state.u_cet);
        }
    }
    #[cfg(not(target_os = "none"))]
    let _ = state;
}

/// The architectural PKRU value that permits access through every user key.
#[cfg(feature = "pkeys")]
pub const PKRU_DEFAULT: u32 = 0;

/// Per-CPU observations used to decide whether protection keys are usable.
#[cfg(feature = "pkeys")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PkeyCapabilityMatrix {
    /// CPUID.7.0:ECX.PKU was advertised.
    pub cpuid_pku: bool,
    /// CR4.PKE is set and the PKRU instructions are therefore enabled.
    pub pke_enabled: bool,
}

#[cfg(feature = "pkeys")]
impl PkeyCapabilityMatrix {
    /// Returns whether this CPU can use PKRU and protection-key PTE bits.
    pub const fn usable(self) -> bool {
        self.cpuid_pku && self.pke_enabled
    }
}

/// Returns the local CPU's protection-key capability observations.
///
/// Host tests cannot inspect CR4 and therefore always report PKE disabled.
#[cfg(feature = "pkeys")]
pub fn probe_pkey_capabilities() -> PkeyCapabilityMatrix {
    let cpuid_pku = x86::cpuid::CpuId::new()
        .get_extended_feature_info()
        .is_some_and(|features| features.has_pku());

    #[cfg(target_os = "none")]
    {
        PkeyCapabilityMatrix {
            cpuid_pku,
            pke_enabled: Cr4::read().contains(Cr4Flags::PROTECTION_KEY_USER),
        }
    }
    #[cfg(not(target_os = "none"))]
    {
        PkeyCapabilityMatrix {
            cpuid_pku,
            pke_enabled: false,
        }
    }
}

/// Enables CR4.PKE on this CPU when the processor advertises protection keys.
///
/// PKRU is switched explicitly with RDPKRU/WRPKRU; this deliberately does not
/// enable XSAVE, XCR0 PKRU state, AVX, or any wider vector state.
#[cfg(feature = "pkeys")]
pub fn init_pkeys() {
    #[cfg(target_os = "none")]
    {
        let capabilities = probe_pkey_capabilities();
        if capabilities.cpuid_pku && !capabilities.pke_enabled {
            let mut cr4 = Cr4::read();
            cr4.insert(Cr4Flags::PROTECTION_KEY_USER);
            // SAFETY: CPUID has advertised PKU and only CR4.PKE is changed.
            unsafe { Cr4::write(cr4) };
        }
        // A bootloader-provided PKRU must not become the initial task state.
        if probe_pkey_capabilities().usable() {
            let _ = write_pkru(PKRU_DEFAULT);
        }
    }
}

/// Returns whether protection keys are enabled on this CPU.
#[cfg(feature = "pkeys")]
#[inline]
pub fn pkeys_enabled() -> bool {
    #[cfg(target_os = "none")]
    {
        return probe_pkey_capabilities().usable();
    }
    #[cfg(not(target_os = "none"))]
    false
}

/// Reads PKRU if protection keys are enabled on this CPU.
#[cfg(feature = "pkeys")]
#[inline]
pub fn read_pkru() -> Option<u32> {
    if !pkeys_enabled() {
        return None;
    }
    let pkru: u32;
    // SAFETY: CR4.PKE was checked above; ECX must be zero for RDPKRU.
    unsafe {
        asm!(
            "rdpkru",
            in("ecx") 0_u32,
            lateout("eax") pkru,
            lateout("edx") _,
            options(nomem, nostack, preserves_flags),
        );
    }
    Some(pkru)
}

/// Writes PKRU if protection keys are enabled on this CPU.
///
/// The trailing LFENCE prevents later loads from being speculated with the
/// permissions that preceded this update.
#[cfg(feature = "pkeys")]
#[inline]
pub fn write_pkru(pkru: u32) -> bool {
    if !pkeys_enabled() {
        return false;
    }
    // SAFETY: CR4.PKE was checked above; WRPKRU requires ECX and EDX zero.
    unsafe {
        asm!(
            "wrpkru",
            "lfence",
            in("eax") pkru,
            in("ecx") 0_u32,
            in("edx") 0_u32,
            options(nostack, preserves_flags),
        );
    }
    true
}

/// Per-CPU capability observations used to decide whether PCID is safe for
/// the whole boot.
#[cfg(feature = "asid-fast-switch")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcidCapabilityMatrix {
    /// CPUID.1:ECX.PCID was advertised.
    pub cpuid_pcid: bool,
    /// CPUID.7.0:EBX.INVPCID was advertised.
    pub cpuid_invpcid: bool,
    /// The current CR3 had no low twelve bits before enabling PCIDE.
    pub cr3_low_bits_zero: bool,
    /// CR4.PCIDE was set and readable after the enable attempt.
    pub pcide_enabled: bool,
}

#[cfg(feature = "asid-fast-switch")]
impl PcidCapabilityMatrix {
    /// Returns whether this CPU can participate in the PCID/INVPCID path.
    pub const fn usable(self) -> bool {
        // `cr3_low_bits_zero` is an enable-time precondition.  A CPU that
        // entered the path with PCIDE already set may legitimately have a
        // nonzero current PCID, so that observation must not reject an
        // otherwise usable pre-enabled CPU.
        self.cpuid_pcid && self.cpuid_invpcid && self.pcide_enabled
    }
}

#[cfg(feature = "asid-fast-switch")]
#[inline]
fn invpcid_supported() -> bool {
    x86::cpuid::CpuId::new()
        .get_extended_feature_info()
        .is_some_and(|features| features.has_invpcid())
}

#[cfg(feature = "asid-fast-switch")]
#[inline]
fn root_pcid_encoding(root: usize, pcid: usize, no_flush: bool) -> Option<u64> {
    if root & 0xfff != 0 || root >= (1usize << 52) || pcid >= 4096 || (pcid == 0 && no_flush) {
        return None;
    }
    Some(root as u64 | pcid as u64 | ((no_flush as u64) << 63))
}

/// Result of classifying one current-to-target user address-space switch.
#[cfg(feature = "asid-fast-switch")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserTlbSwitchDecision {
    /// The complete identity is valid and may retain the target PCID's TLB.
    Retain,
    /// The target must be entered through a flushing path.
    Flush(crate::AsidSwitchFallbackReason),
}

/// Classifies a user address-space transition without touching privileged
/// registers.
#[cfg(feature = "asid-fast-switch")]
#[inline]
#[allow(clippy::too_many_arguments)]
pub fn classify_user_tlb_switch(
    current_root: usize,
    current_asid: usize,
    current_generation: u64,
    current_fallback: crate::AddressSpaceFallbackReason,
    next_root: usize,
    next_asid: usize,
    next_generation: u64,
    next_fallback: crate::AddressSpaceFallbackReason,
) -> UserTlbSwitchDecision {
    // The shared classifier treats ASID 0 as the conservative legacy path.
    // Keep that path conservative only when its metadata is itself a valid
    // legacy identity; malformed current state must not be allowed to retain
    // a target PCID merely because the current numeric ASID is zero.
    if current_asid == 0
        && (current_root & 0xfff != 0
            || current_root >= (1usize << 52)
            || current_generation != 0
            || matches!(current_fallback, crate::AddressSpaceFallbackReason::None))
    {
        return UserTlbSwitchDecision::Flush(crate::AsidSwitchFallbackReason::InvalidWidth);
    }
    match crate::classify_user_tlb_switch(
        current_root,
        current_asid,
        current_generation,
        current_fallback,
        next_root,
        next_asid,
        next_generation,
        next_fallback,
    ) {
        crate::TlbSwitchDecision::Retain => UserTlbSwitchDecision::Retain,
        crate::TlbSwitchDecision::Flush(reason) => UserTlbSwitchDecision::Flush(reason),
    }
}

/// Returns the local CPU's PCID/INVPCID capability observations.
#[cfg(feature = "asid-fast-switch")]
pub fn probe_pcid_capabilities() -> PcidCapabilityMatrix {
    let cpuid = x86::cpuid::CpuId::new();
    let cpuid_pcid = cpuid
        .get_feature_info()
        .is_some_and(|features| features.has_pcid());
    let cpuid_invpcid = cpuid
        .get_extended_feature_info()
        .is_some_and(|features| features.has_invpcid());

    #[cfg(target_os = "none")]
    {
        let cr3 = unsafe { controlregs::cr3() };
        let cr4 = Cr4::read();
        PcidCapabilityMatrix {
            cpuid_pcid,
            cpuid_invpcid,
            cr3_low_bits_zero: cr3 & 0xfff == 0,
            pcide_enabled: cr4.contains(Cr4Flags::PCID),
        }
    }
    #[cfg(not(target_os = "none"))]
    {
        // Host tests cannot read privileged control registers. They must not
        // accidentally make the kernel allocator believe that PCID is live.
        PcidCapabilityMatrix {
            cpuid_pcid,
            cpuid_invpcid,
            cr3_low_bits_zero: false,
            pcide_enabled: false,
        }
    }
}

/// Disables PCIDE after moving through PCID 0, which performs the required
/// non-global TLB invalidation before ordinary CR3 reloads are allowed again.
///
/// This is only needed for a boot environment that entered the kernel with
/// PCIDE already enabled but without INVPCID.  The CR3 write deliberately
/// clears the low bits while PCIDE is still set; clearing CR4.PCIDE only after
/// that write avoids leaving a mixed PCID/non-PCID state behind.
#[cfg(all(feature = "asid-fast-switch", target_os = "none"))]
fn disable_pcide_safely() -> bool {
    let current_cr3 = unsafe { controlregs::cr3() } as usize;
    let root = current_cr3 & !0xfff;
    if root >= (1usize << 52) {
        return false;
    }

    let frame = PhysFrame::containing_address(x86_64::PhysAddr::new_truncate(root as u64));
    // SAFETY: the current root is retained, PCID 0 is explicitly selected,
    // and NOFLUSH is clear.  With PCIDE set this is the architectural full
    // non-global invalidation needed before disabling PCIDE.
    unsafe { Cr3::write(frame, Cr3Flags::empty()) };
    if (unsafe { controlregs::cr3() } as usize) & 0xfff != 0 {
        return false;
    }

    let mut cr4 = Cr4::read();
    cr4.remove(Cr4Flags::PCID);
    // SAFETY: only the PCIDE bit is changed and paging remains enabled.
    unsafe { Cr4::write(cr4) };
    !Cr4::read().contains(Cr4Flags::PCID)
}

/// Enables PCIDE on the current CPU after validating the boot CR3 and CPUID.
#[cfg(feature = "asid-fast-switch")]
pub fn init_pcid() {
    #[cfg(target_os = "none")]
    let mut capabilities = probe_pcid_capabilities();
    #[cfg(not(target_os = "none"))]
    let capabilities = probe_pcid_capabilities();

    #[cfg(target_os = "none")]
    {
        // A pre-enabled PCIDE without INVPCID cannot use the ordinary CR3
        // full-flush fallback.  First return to PCID 0 and then disable
        // PCIDE, so the remainder of this boot has a valid non-PCID mode.
        if capabilities.pcide_enabled && (!capabilities.cpuid_pcid || !capabilities.cpuid_invpcid) {
            if !disable_pcide_safely() {
                panic!("cannot disable pre-enabled PCIDE without INVPCID");
            }
            capabilities = probe_pcid_capabilities();
        }

        if !capabilities.pcide_enabled
            && capabilities.cpuid_pcid
            && capabilities.cpuid_invpcid
            && capabilities.cr3_low_bits_zero
        {
            let mut cr4 = Cr4::read();
            cr4.insert(Cr4Flags::PCID);
            // SAFETY: CPUID advertised PCID and CR3 was checked to have zero
            // low bits, as required when enabling CR4.PCIDE.
            unsafe { Cr4::write(cr4) };
            capabilities = probe_pcid_capabilities();
        }
    }

    if capabilities.usable() {
        PCID_CPUS_ENABLED.fetch_add(1, Ordering::Release);
    } else {
        PCID_CPUS_FAILED.fetch_add(1, Ordering::Release);
    }
}

/// Returns whether every boot CPU reported usable PCID/INVPCID support.
#[cfg(feature = "asid-fast-switch")]
pub fn pcid_bootstrap_complete(expected_cpus: usize) -> bool {
    expected_cpus != 0
        && PCID_CPUS_ENABLED.load(Ordering::Acquire) == expected_cpus
        && PCID_CPUS_FAILED.load(Ordering::Acquire) == 0
}

/// Returns whether PCID is enabled on the current CPU.
#[cfg(feature = "asid-fast-switch")]
#[inline]
pub fn pcid_enabled() -> bool {
    #[cfg(target_os = "none")]
    {
        return Cr4::read().contains(Cr4Flags::PCID);
    }
    #[cfg(not(target_os = "none"))]
    false
}

/// Allows the current CPU to respond to interrupts.
#[inline]
pub fn enable_irqs() {
    #[cfg(not(target_os = "none"))]
    {
        warn!("enable_irqs: not implemented");
    }
    #[cfg(target_os = "none")]
    interrupts::enable()
}

/// Makes the current CPU to ignore interrupts.
#[inline]
pub fn disable_irqs() {
    #[cfg(not(target_os = "none"))]
    {
        warn!("disable_irqs: not implemented");
    }
    #[cfg(target_os = "none")]
    interrupts::disable()
}

/// Returns whether the current CPU is allowed to respond to interrupts.
#[inline]
pub fn irqs_enabled() -> bool {
    interrupts::are_enabled()
}

/// Relaxes the current CPU and waits for interrupts.
///
/// It must be called with interrupts enabled, otherwise it will never return.
#[inline]
pub fn wait_for_irqs() {
    if cfg!(target_os = "none") {
        unsafe { asm!("hlt") }
    } else {
        core::hint::spin_loop()
    }
}

/// Halt the current CPU.
#[inline]
pub fn halt() {
    disable_irqs();
    wait_for_irqs(); // should never return
}

/// Reads the current page table root register for user space (`CR3`).
///
/// x86_64 does not have a separate page table root register for user and
/// kernel space, so this operation is the same as [`read_kernel_page_table`].
///
/// Returns the physical address of the page table root.
#[inline]
pub fn read_user_page_table() -> PhysAddr {
    #[cfg(target_os = "none")]
    {
        pa!(unsafe { controlregs::cr3() } as usize).align_down_4k()
    }
    #[cfg(not(target_os = "none"))]
    {
        // Host tests cannot read privileged CR3.  Keep this fallback a
        // harmless pure value even when the caller did not opt into the
        // dummy-context feature.
        pa!(0)
    }
}

/// Reads the current page table root register for kernel space (`CR3`).
///
/// x86_64 does not have a separate page table root register for user and
/// kernel space, so this operation is the same as [`read_user_page_table`].
///
/// Returns the physical address of the page table root.
#[inline]
pub fn read_kernel_page_table() -> PhysAddr {
    read_user_page_table()
}

/// Writes the register to update the current page table root for user space
/// (`CR3`).
///
/// x86_64 does not have a separate page table root register for user
/// and kernel space, so this operation is the same as [`write_kernel_page_table`].
///
/// Note that the TLB will be **flushed** after this operation.
///
/// # Safety
///
/// This function is unsafe as it changes the virtual memory address space.
#[inline]
pub unsafe fn write_user_page_table(root_paddr: PhysAddr) {
    #[cfg(target_os = "none")]
    {
        let frame = PhysFrame::containing_address(x86_64::PhysAddr::new_truncate(
            root_paddr.as_usize() as u64,
        ));
        // SAFETY: the caller owns the address-space transition. Using the
        // normal CR3 write deliberately selects PCID 0 and never sets
        // CR3.NOFLUSH.
        unsafe { Cr3::write(frame, Cr3Flags::empty()) }
    }
    #[cfg(not(target_os = "none"))]
    {
        // A hosted build must remain executable as an ordinary ring-3 test
        // process.  This API is deliberately a no-op there rather than a
        // best-effort CR3 write.
        let _ = root_paddr;
    }
}

/// Writes a user root and a nonzero PCID without flushing that PCID's TLB.
///
/// # Safety
///
/// The caller must own the address-space transition and provide a root/PCID
/// pair that remains valid for the entire boot. The PCID must not be recycled
/// while any CPU can still refill translations for its previous root.
#[cfg(feature = "asid-fast-switch")]
#[inline]
pub unsafe fn write_user_page_table_with_asid(root_paddr: PhysAddr, pcid: usize) {
    #[cfg(target_os = "none")]
    {
        let encoding = root_pcid_encoding(root_paddr.as_usize(), pcid, true);
        if pcid != 0 && encoding.is_some() && pcid_enabled() && invpcid_supported() {
            let frame = PhysFrame::containing_address(x86_64::PhysAddr::new_truncate(
                root_paddr.as_usize() as u64,
            ));
            let pcid = x86_64_tlb::Pcid::new(pcid as u16)
                .expect("root_pcid_encoding accepted a PCID outside the architectural range");
            // SAFETY: PCIDE is read back as enabled and the root/PCID pair
            // was validated above; the caller owns the address-space
            // transition.
            unsafe { Cr3::write_pcid_no_flush(frame, pcid) };
        } else {
            // Invalid or unavailable PCID state must use the conservative
            // CR3=0 path rather than encoding an unvalidated value into CR3.
            unsafe { write_user_page_table(root_paddr) };
        }
    }
    #[cfg(not(target_os = "none"))]
    {
        // Hosted tests may exercise classification and call sequencing, but
        // they must never execute a privileged CR3 instruction.
        let _ = (root_paddr, pcid);
    }
}

/// Writes a user root and a nonzero PCID with the architectural flush write.
///
/// # Safety
///
/// The caller must own the address-space transition and provide a root/PCID
/// pair that remains valid for the entire boot. The PCID must not be recycled
/// while any CPU can still refill translations for its previous root.
#[cfg(feature = "asid-fast-switch")]
#[inline]
pub unsafe fn write_user_page_table_with_asid_flush(root_paddr: PhysAddr, pcid: usize) {
    #[cfg(target_os = "none")]
    {
        let encoding = root_pcid_encoding(root_paddr.as_usize(), pcid, false);
        if pcid != 0 && encoding.is_some() && pcid_enabled() && invpcid_supported() {
            let frame = PhysFrame::containing_address(x86_64::PhysAddr::new_truncate(
                root_paddr.as_usize() as u64,
            ));
            let pcid = x86_64_tlb::Pcid::new(pcid as u16)
                .expect("root_pcid_encoding accepted a PCID outside the architectural range");
            // SAFETY: see [`write_user_page_table_with_asid`]. The NOFLUSH
            // bit is intentionally clear so the target PCID is flushed.
            unsafe { Cr3::write_pcid(frame, pcid) };
        } else {
            unsafe { write_user_page_table(root_paddr) };
        }
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = (root_paddr, pcid);
    }
}

/// Writes the register to update the current page table root for kernel space
/// (`CR3`).
///
/// x86_64 does not have a separate page table root register for user
/// and kernel space, so this operation is the same as [`write_user_page_table`].
///
/// Note that the TLB will be **flushed** after this operation.
///
/// # Safety
///
/// This function is unsafe as it changes the virtual memory address space.
#[inline]
pub unsafe fn write_kernel_page_table(root_paddr: PhysAddr) {
    unsafe { write_user_page_table(root_paddr) }
}

/// Flushes the TLB.
///
/// If `vaddr` is [`None`], flushes the entire TLB. Otherwise, flushes the TLB
/// entry that maps the given virtual address.
#[inline]
pub fn flush_tlb(vaddr: Option<VirtAddr>) {
    #[cfg(target_os = "none")]
    {
        if let Some(vaddr) = vaddr {
            // SAFETY: this target-specific operation is only compiled for the
            // kernel's ring-0 execution environment.
            unsafe { tlb::flush(vaddr.into()) }
        } else {
            #[cfg(feature = "asid-fast-switch")]
            if pcid_enabled() {
                if invpcid_supported() {
                    // SAFETY: the capability check above guarantees that
                    // INVPCID is implemented and CR4.PCIDE is enabled on this
                    // CPU.
                    unsafe { x86_64_tlb::flush_pcid(x86_64_tlb::InvPcidCommand::AllExceptGlobal) };
                    return;
                }

                if !disable_pcide_safely() {
                    // A normal CR3 reload while PCIDE remains set is only a
                    // current-PCID operation, not the full flush promised
                    // here. Stop rather than silently continuing with stale
                    // entries.
                    panic!("cannot disable PCIDE before a full TLB flush");
                }
            }
            // SAFETY: this target-specific operation is only compiled for the
            // kernel's ring-0 execution environment, and PCIDE has been
            // disabled above whenever the INVPCID path was unavailable.
            unsafe { tlb::flush_all() }
        }
    }
    #[cfg(not(target_os = "none"))]
    {
        // Hosted callers may test the policy and classifier paths directly;
        // no INVLPG, CR3 reload, or full-flush instruction is legal there.
        let _ = vaddr;
    }
}

/// Synchronizes instruction fetches with earlier writes to executable memory.
///
/// x86 keeps its instruction and data caches coherent, so no instruction is
/// required at this publication boundary.
#[inline]
pub fn flush_icache_all() {}

/// Reads the thread pointer of the current CPU (`FS_BASE`).
///
/// It is used to implement TLS (Thread Local Storage).
#[inline]
pub fn read_thread_pointer() -> usize {
    unsafe { msr::rdmsr(msr::IA32_FS_BASE) as usize }
}

/// Writes the thread pointer of the current CPU (`FS_BASE`).
///
/// It is used to implement TLS (Thread Local Storage).
///
/// # Safety
///
/// This function is unsafe as it changes the CPU states.
#[inline]
pub unsafe fn write_thread_pointer(fs_base: usize) {
    unsafe { msr::wrmsr(msr::IA32_FS_BASE, fs_base as u64) }
}

/// Loads this CPU's kernel-owned LDT system descriptor and LDTR.
///
/// Callers must disable IRQs/preemption and keep `base..base + bytes` alive
/// until every CPU that may have loaded it has crossed its maintenance grace.
#[inline]
pub unsafe fn load_user_ldt(base: *const u8, bytes: usize) {
    unsafe { super::gdt::load_ldt(base, bytes) }
}

#[cfg(feature = "uspace")]
core::arch::global_asm!(include_str!("user_copy.S"));

#[cfg(feature = "uspace")]
unsafe extern "C" {
    /// Copies data from source to destination, where addresses may be in user
    /// space. Equivalent to memcpy.
    ///
    /// # Safety
    /// This function is unsafe because it performs raw memory operations.
    ///
    /// # Returns
    /// Returns the number of bytes not copied. This means 0 indicates success,
    /// while a value > 0 indicates failure.
    pub fn user_copy(dst: *mut u8, src: *const u8, size: usize) -> usize;
}

#[cfg(all(test, feature = "asid-fast-switch"))]
mod tests {
    use memory_addr::{PhysAddr, VirtAddr};

    use super::{
        PcidCapabilityMatrix, UserTlbSwitchDecision, classify_user_tlb_switch, flush_tlb,
        read_kernel_page_table, read_user_page_table, root_pcid_encoding, write_kernel_page_table,
        write_user_page_table, write_user_page_table_with_asid,
        write_user_page_table_with_asid_flush,
    };

    #[test]
    fn capability_matrix_requires_every_architectural_gate() {
        let usable = PcidCapabilityMatrix {
            cpuid_pcid: true,
            cpuid_invpcid: true,
            cr3_low_bits_zero: false,
            pcide_enabled: true,
        };
        assert!(usable.usable());
        for (cpuid_pcid, cpuid_invpcid, cr3_low_bits_zero, pcide_enabled) in [
            (false, true, true, true),
            (true, false, true, true),
            (true, true, true, false),
        ] {
            assert!(
                !PcidCapabilityMatrix {
                    cpuid_pcid,
                    cpuid_invpcid,
                    cr3_low_bits_zero,
                    pcide_enabled,
                }
                .usable()
            );
        }
    }

    #[test]
    fn preenabled_pcid_can_have_a_nonzero_current_pcid() {
        assert!(
            PcidCapabilityMatrix {
                cpuid_pcid: true,
                cpuid_invpcid: true,
                cr3_low_bits_zero: false,
                pcide_enabled: true,
            }
            .usable()
        );
    }

    #[test]
    fn cr3_pcid_encoding_rejects_bad_roots_and_never_sets_noflush_for_zero() {
        assert_eq!(
            root_pcid_encoding(0x12_3000, 1, true),
            Some(0x8000_0000_0012_3001)
        );
        assert_eq!(root_pcid_encoding(0x12_3001, 1, true), None);
        assert_eq!(root_pcid_encoding(0x12_3000, 4096, true), None);
        assert_eq!(root_pcid_encoding(0x12_3000, 0, false), Some(0x12_3000));
        assert_eq!(root_pcid_encoding(0x12_3000, 0, true), None);
    }

    #[test]
    fn switch_classifier_is_pure_and_rejects_invalid_identity_metadata() {
        assert_eq!(
            classify_user_tlb_switch(
                0x12_3000,
                7,
                1,
                crate::AddressSpaceFallbackReason::None,
                0x12_3000,
                7,
                2,
                crate::AddressSpaceFallbackReason::None,
            ),
            UserTlbSwitchDecision::Flush(crate::AsidSwitchFallbackReason::GenerationMismatch)
        );
        assert!(matches!(
            classify_user_tlb_switch(
                0x12_3000,
                7,
                1,
                crate::AddressSpaceFallbackReason::None,
                0x12_3000,
                7,
                1,
                crate::AddressSpaceFallbackReason::InvalidWidth,
            ),
            UserTlbSwitchDecision::Flush(crate::AsidSwitchFallbackReason::InvalidWidth)
        ));
        assert!(matches!(
            classify_user_tlb_switch(
                0x12_3001,
                0,
                0,
                crate::AddressSpaceFallbackReason::AsidZero,
                0x12_3000,
                7,
                1,
                crate::AddressSpaceFallbackReason::None,
            ),
            UserTlbSwitchDecision::Flush(crate::AsidSwitchFallbackReason::InvalidWidth)
        ));
    }

    #[cfg(not(target_os = "none"))]
    #[test]
    fn hosted_page_table_operations_never_execute_privileged_instructions() {
        let root = PhysAddr::from_usize(0x12_3000);
        assert_eq!(read_user_page_table(), PhysAddr::from_usize(0));
        assert_eq!(read_kernel_page_table(), PhysAddr::from_usize(0));
        // These calls are intentionally direct: a hosted test must remain
        // safe even when a caller reaches the low-level API without the
        // higher-level host-test-context feature.
        unsafe {
            write_user_page_table(root);
            write_kernel_page_table(root);
            write_user_page_table_with_asid(root, 7);
            write_user_page_table_with_asid_flush(root, 7);
        }
        flush_tlb(Some(VirtAddr::from_usize(0x4000)));
        flush_tlb(None);
    }
}
