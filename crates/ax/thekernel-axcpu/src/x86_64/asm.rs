//! Wrapper functions for assembly instructions.

#[cfg(all(feature = "fp-simd", target_os = "none"))]
use core::sync::atomic::AtomicU8;
#[cfg(all(feature = "fp-simd", target_os = "none"))]
use core::sync::atomic::AtomicU64;
#[cfg(any(
    feature = "asid-fast-switch",
    all(feature = "fp-simd", target_os = "none")
))]
use core::sync::atomic::AtomicUsize;
use core::{
    arch::asm,
    sync::atomic::{AtomicBool, Ordering},
};

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
#[cfg(target_os = "none")]
use x86_64::registers::control::{Cr4, Cr4Flags};
#[cfg(target_os = "none")]
use x86_64::{
    registers::control::{Cr3, Cr3Flags},
    registers::debug::{DebugAddressRegister, Dr0, Dr1, Dr2, Dr3, Dr6, Dr7},
    structures::paging::PhysFrame,
};

/// Program the task-local DR0--DR3 set.  A `None` slot is fully disabled.
/// Callers construct this fixed array during a scheduler transition; no trap
/// path allocates or takes a scheduler lock to inspect it.
pub fn program_perf_debug_registers(slots: [Option<(u64, u64, u32)>; 4]) {
    #[cfg(target_os = "none")]
    {
        // Disable first so no partially rewritten address can fire.
        Dr7::write_raw(0);
        let mut dr7 = 0u64;
        for (slot, entry) in slots.into_iter().enumerate() {
            let Some((addr, len, ty)) = entry else {
                continue;
            };
            match slot {
                0 => Dr0::write(addr),
                1 => Dr1::write(addr),
                2 => Dr2::write(addr),
                3 => Dr3::write(addr),
                _ => unreachable!(),
            }
            // Linux HW_BREAKPOINT_{R,W,X}: execute=00, write=01, read/write=11.
            let rw = if ty & 4 != 0 {
                0
            } else if ty & 1 != 0 {
                3
            } else {
                1
            };
            let length = match len {
                1 => 0,
                2 => 1,
                8 => 2,
                4 => 3,
                _ => continue,
            };
            dr7 |= 1 << (slot * 2); // local enable
            dr7 |= (rw as u64) << (16 + slot * 4);
            dr7 |= (length as u64) << (18 + slot * 4);
        }
        // LE requests exact data-breakpoint reporting where implemented.
        Dr7::write_raw(dr7 | (1 << 8));
    }
    #[cfg(not(target_os = "none"))]
    let _ = slots;
}

/// Terminal crash-kexec shutdown for task-owned hardware breakpoints.
/// Disable DR7 before erasing addresses so no partially-cleared slot can fire.
pub fn crash_quiesce_debug_registers() {
    #[cfg(target_os = "none")]
    {
        Dr7::write_raw(0);
        Dr0::write(0);
        Dr1::write(0);
        Dr2::write(0);
        Dr3::write(0);
        unsafe { asm!("mov dr6, {}", in(reg) 0u64, options(nomem, nostack, preserves_flags)) };
    }
}

/// Read the architectural #DB status without consuming another owner's bits.
/// #DB multiplexes TF/BS, hardware watchpoints, and debugger users; callers
/// must acknowledge only the subset they actually handled.
pub fn read_perf_debug_status() -> u64 {
    #[cfg(target_os = "none")]
    {
        Dr6::read_raw()
    }
    #[cfg(not(target_os = "none"))]
    {
        0
    }
}

/// Acknowledge precisely the #DB causes in `mask`, preserving unclaimed DR6
/// status for the next owner in this trap dispatch.  DR6 cause flags clear on
/// zero writes, so the current value with just `mask` cleared is written back.
pub fn acknowledge_perf_debug_status(mask: u64) {
    #[cfg(target_os = "none")]
    {
        let current = Dr6::read_raw();
        // SAFETY: DR6 is per-CPU and this runs while dispatching #DB.
        unsafe {
            asm!("mov dr6, {}", in(reg) (current & !mask), options(nomem, nostack, preserves_flags))
        };
    }
    #[cfg(not(target_os = "none"))]
    let _ = mask;
}

#[cfg(feature = "asid-fast-switch")]
static PCID_CPUS_ENABLED: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "asid-fast-switch")]
static PCID_CPUS_FAILED: AtomicUsize = AtomicUsize::new(0);

// CR4.CET is a per-CPU bit, but user CET is a product-wide ABI contract.  Do
// not let the first capable CPU expose the ABI while another online CPU still
// has an unknown or negative capability result.
static USER_SHADOW_STACK_FLEET_ACTIVE: AtomicBool = AtomicBool::new(false);

/// CPUID.(EAX=7,ECX=0):ECX.SHSTK.  User shadow stacks have no XSAVE feature
/// dependency; fleet admission is solely this architectural capability plus
/// the platform-wide all-CPU commit.
const CPUID_7_0_ECX_SHSTK: u32 = 1 << 7;

const fn cpuid_has_user_shadow_stack(ecx: u32) -> bool {
    ecx & CPUID_7_0_ECX_SHSTK != 0
}
// BSP publishes the exact XCR0 contract before APs can start scheduling.
// Every AP compares its CPUID-derived state to this tuple; heterogeneous CPUs
// are rejected rather than silently corrupting a migrated task's registers.
#[cfg(all(feature = "fp-simd", target_os = "none"))]
static XSAVE_FLEET_FEATURES: AtomicU64 = AtomicU64::new(0);
#[cfg(all(feature = "fp-simd", target_os = "none"))]
static XSAVE_FLEET_SIZE: AtomicUsize = AtomicUsize::new(0);
// Zero-valued xfeatures are the FXSAVE fallback, so the feature value itself
// cannot also be the publication sentinel.  Publish the pair behind this
// state word: 0 = absent, 1 = writer owns it, 2 = fully visible.
#[cfg(all(feature = "fp-simd", target_os = "none"))]
static XSAVE_FLEET_STATE: AtomicU8 = AtomicU8::new(0);

/// Architectural user-CET state owned by one schedulable task.  CET is
/// switched explicitly, independently from PKRU and XSAVE state.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UserCetState {
    /// IA32_U_CET value.
    pub u_cet: u64,
    /// IA32_PL3_SSP value.
    pub pl3_ssp: u64,
    /// Linux ARCH_SHSTK_LOCK state, one bit per IA32_U_CET feature.
    pub locked: u64,
}

/// CET hardware state present when this logical CPU entered the kernel.
///
/// This is deliberately distinct from [`UserCetState`]: the latter belongs to
/// a scheduled task, while this snapshot belongs to firmware/the boot loader
/// and is needed only by a non-returning kexec handoff.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UserCetBootBaseline {
    /// The snapshot was taken on a CPU that advertises user shadow stacks.
    pub captured: bool,
    /// Boot-time CR4.CET value.
    pub cr4_cet: bool,
    /// Boot-time IA32_U_CET value.
    pub u_cet: u64,
    /// Boot-time IA32_PL3_SSP value.
    pub pl3_ssp: u64,
}

#[cfg(any(target_os = "none", test))]
const IA32_U_CET: u32 = 0x6a0;
#[cfg(any(target_os = "none", test))]
const IA32_PL3_SSP: u32 = 0x6a7;

/// Programs the CET MSRs in the architectural transition order.  CET must
/// be disabled before changing PL3_SSP, including when the target state is
/// disabled; otherwise a transient SSP check can observe the wrong task.
#[cfg(any(target_os = "none", test))]
#[inline]
fn program_user_cet_state(mut write: impl FnMut(u32, u64), state: UserCetState) {
    write(IA32_U_CET, 0);
    write(IA32_PL3_SSP, state.pl3_ssp);
    write(IA32_U_CET, state.u_cet);
}

/// Capture the firmware-owned CET state before the product fleet can lazily
/// raise CR4.CET.  This function is intentionally independent of the fleet
/// gate: it runs during the per-CPU read-only capability prepare phase.
#[inline]
pub fn user_cet_boot_baseline() -> UserCetBootBaseline {
    #[cfg(target_os = "none")]
    {
        if !user_shadow_stack_supported() {
            return UserCetBootBaseline::default();
        }
        let cr4 = Cr4::read();
        UserCetBootBaseline {
            captured: true,
            cr4_cet: cr4.contains(Cr4Flags::CONTROL_FLOW_ENFORCEMENT),
            // SAFETY: CET capability was checked above; these are local CPU
            // MSRs captured before scheduler-owned user CET is enabled.
            u_cet: unsafe { msr::rdmsr(IA32_U_CET) },
            pl3_ssp: unsafe { msr::rdmsr(IA32_PL3_SSP) },
        }
    }
    #[cfg(not(target_os = "none"))]
    UserCetBootBaseline::default()
}

/// Restore firmware's CET state at a terminal handoff.
///
/// The caller must have stopped ordinary scheduling on this CPU.  It is safe
/// to call even when prepare did not capture a baseline: in that case the
/// routine leaves CET disabled rather than carrying scheduler-owned state
/// into the next kernel.
#[inline]
pub fn restore_user_cet_boot_baseline(baseline: UserCetBootBaseline) {
    #[cfg(target_os = "none")]
    {
        if !user_shadow_stack_supported() {
            return;
        }

        // CET requires U_CET disabled while PL3_SSP is changed.  Keep CR4.CET
        // set until both MSRs are settled, then restore its boot value last.
        // This is also the safe-disabled fallback for an absent snapshot.
        unsafe { msr::wrmsr(IA32_U_CET, 0) };
        unsafe { msr::wrmsr(IA32_PL3_SSP, baseline.pl3_ssp) };
        unsafe { msr::wrmsr(IA32_U_CET, baseline.u_cet) };
        let mut cr4 = Cr4::read();
        cr4.set(
            Cr4Flags::CONTROL_FLOW_ENFORCEMENT,
            baseline.captured && baseline.cr4_cet,
        );
        // SAFETY: terminal handoff owns this CPU and no ordinary task will
        // resume after the restore.
        unsafe { Cr4::write(cr4) };
    }
    #[cfg(not(target_os = "none"))]
    let _ = baseline;
}

/// Leave no task-owned CET state behind on a lock-free crash handoff.
///
/// Unlike [`restore_user_cet_boot_baseline`], this never reads fleet-owned
/// storage and is therefore safe from a crash-stop IPI that interrupted a
/// lock holder.
#[inline]
pub fn disable_user_cet_for_terminal_handoff() {
    #[cfg(target_os = "none")]
    {
        if !user_shadow_stack_supported() {
            return;
        }
        unsafe { msr::wrmsr(IA32_U_CET, 0) };
        unsafe { msr::wrmsr(IA32_PL3_SSP, 0) };
        let mut cr4 = Cr4::read();
        cr4.remove(Cr4Flags::CONTROL_FLOW_ENFORCEMENT);
        // SAFETY: crash handoff has disabled interrupts and no task can
        // resume on this CPU.
        unsafe { Cr4::write(cr4) };
    }
}

/// Whether this CPU has enabled user shadow-stack support. Hosted builds must
/// always return false: touching privileged CET state there would be invalid.
#[inline]
pub fn user_shadow_stack_enabled() -> bool {
    #[cfg(target_os = "none")]
    {
        if !USER_SHADOW_STACK_FLEET_ACTIVE.load(Ordering::Acquire) {
            return false;
        }
        let cpuid = core::arch::x86_64::__cpuid_count(7, 0);
        if !cpuid_has_user_shadow_stack(cpuid.ecx) {
            return false;
        }
        // APs join the fleet during late boot, before the IPI broker exists.
        // The final fleet commit makes this lazy local CR4 write safe; it
        // guarantees that a CPU cannot expose CET before every peer probed.
        let mut cr4 = Cr4::read();
        if !cr4.contains(Cr4Flags::CONTROL_FLOW_ENFORCEMENT) {
            cr4.insert(Cr4Flags::CONTROL_FLOW_ENFORCEMENT);
            // SAFETY: every online CPU completed the CET capability probe
            // before the platform published the fleet-active gate.
            unsafe { Cr4::write(cr4) };
        }
        true
    }
    #[cfg(not(target_os = "none"))]
    false
}

/// Return whether the current CPU advertises user shadow stacks.
#[inline]
pub fn user_shadow_stack_supported() -> bool {
    #[cfg(target_os = "none")]
    {
        let cpuid = core::arch::x86_64::__cpuid_count(7, 0);
        cpuid_has_user_shadow_stack(cpuid.ecx)
    }
    #[cfg(not(target_os = "none"))]
    false
}

/// Publish or withdraw the platform's all-online-CPU CET decision.
///
/// This only gates user-CET availability.  Local CR4.CET is raised lazily by
/// [`user_shadow_stack_enabled`] after the positive fleet decision, so boot
/// never has a partial user-visible CET state.
#[inline]
pub fn set_user_shadow_stack_fleet_active(active: bool) {
    USER_SHADOW_STACK_FLEET_ACTIVE.store(active, Ordering::Release);
}

/// Reads the user CET MSRs when CET is active on this CPU.
#[inline]
pub fn read_user_cet_state() -> UserCetState {
    #[cfg(target_os = "none")]
    if user_shadow_stack_enabled() {
        return UserCetState {
            u_cet: unsafe { msr::rdmsr(IA32_U_CET) },
            pl3_ssp: unsafe { msr::rdmsr(IA32_PL3_SSP) },
            locked: 0,
        };
    }
    UserCetState::default()
}

/// Writes the user CET MSRs when CET is active on this CPU.
#[inline]
pub fn write_user_cet_state(state: UserCetState) {
    #[cfg(target_os = "none")]
    if user_shadow_stack_enabled() {
        program_user_cet_state(
            |msr_number, value| unsafe { msr::wrmsr(msr_number, value) },
            state,
        );
    }
    #[cfg(not(target_os = "none"))]
    let _ = state;
}

#[cfg(test)]
mod cet_tests {
    use super::*;

    #[test]
    fn cet_switch_disables_before_replacing_ssp() {
        let target = UserCetState {
            u_cet: 3,
            pl3_ssp: 0x1234_5000,
            locked: 0,
        };
        let mut writes = Vec::new();
        program_user_cet_state(|msr, value| writes.push((msr, value)), target);
        assert_eq!(
            writes,
            [
                (IA32_U_CET, 0),
                (IA32_PL3_SSP, target.pl3_ssp),
                (IA32_U_CET, target.u_cet),
            ]
        );
    }

    #[test]
    fn user_shadow_stack_capability_is_only_the_cpuid_shstk_bit() {
        assert!(!cpuid_has_user_shadow_stack(0));
        assert!(!cpuid_has_user_shadow_stack(1 << 6));
        assert!(cpuid_has_user_shadow_stack(CPUID_7_0_ECX_SHSTK));
        assert!(cpuid_has_user_shadow_stack(u32::MAX));
    }
}

/// The architectural PKRU value that permits access through every user key.
#[cfg(feature = "pkeys")]
pub const PKRU_DEFAULT: u32 = 0;

/// Mandatory user xfeatures selected for this kernel. This is an XCR0 mask, never
/// a host-toolchain target-feature mask.  Keep the base kernel contract to
/// architectural x87 and SSE; wider SIMD state is deliberately not enabled.
#[cfg(feature = "fp-simd")]
pub const XSAVE_REQUIRED_XFEATURES: u64 = (1 << 0) | (1 << 1);

#[cfg(any(feature = "fp-simd", feature = "pkeys"))]
const XSAVE_PKRU_XFEATURE: u64 = 1 << 9;

#[cfg(feature = "fp-simd")]
const fn selected_xsave_features(supported: u64, pkeys: bool) -> Option<u64> {
    if !xsave_has_required_components(supported) {
        return None;
    }
    let optional = if pkeys {
        supported & XSAVE_PKRU_XFEATURE
    } else {
        0
    };
    Some(XSAVE_REQUIRED_XFEATURES | optional)
}

#[cfg(feature = "pkeys")]
const fn pkey_state_supported(cpuid_pku: bool, saved_features: u64) -> bool {
    cpuid_pku && saved_features & XSAVE_PKRU_XFEATURE != 0
}

#[cfg(feature = "fp-simd")]
const fn xsave_has_required_components(supported: u64) -> bool {
    supported & XSAVE_REQUIRED_XFEATURES == XSAVE_REQUIRED_XFEATURES
}

/// Maximum standard-format XSAVE image accepted by the in-kernel task
/// context.  Current architectural user state (including AMX) is below this
/// bound; refusing a larger future layout is safer than truncating it.
pub const MAX_XSAVE_SIZE: usize = 32 * 1024;

/// The legacy FXSAVE image has a fixed architectural extent.  It remains a
/// safe hosted fallback when the host OS did not enable XCR0 access: unlike
/// XSAVE it neither reads nor changes XCR0.
#[cfg(feature = "fp-simd")]
const FXSAVE_SIZE: usize = 512;

const XSAVE_MXCSR_OFFSET: usize = 24;
const FXSAVE_MXCSR_MASK_OFFSET: usize = 28;
const XSAVE_HEADER_OFFSET: usize = 512;
const XSAVE_XSTATE_BV_OFFSET: usize = XSAVE_HEADER_OFFSET;
const XSAVE_XCOMP_BV_OFFSET: usize = XSAVE_HEADER_OFFSET + 8;
const XSAVE_HEADER_RESERVED_OFFSET: usize = XSAVE_HEADER_OFFSET + 16;
const XSAVE_HEADER_SIZE: usize = 64;
const DEFAULT_MXCSR_MASK: u32 = 0xffbf;

/// Returns the MXCSR bit mask accepted by this CPU's XRSTOR path.
///
/// CPUID deliberately does not describe this mask.  Intel specifies that it
/// is obtained from the FXSAVE legacy area; an all-zero value means the
/// architectural default.  Keep the probe in the architecture layer so an
/// untrusted signal image can never reach XRSTOR with reserved MXCSR bits.
#[cfg(feature = "fp-simd")]
fn supported_mxcsr_mask() -> u32 {
    #[cfg(target_os = "none")]
    {
        #[repr(C, align(16))]
        struct Fxsave([u8; 512]);
        let mut image = Fxsave([0; 512]);
        // SAFETY: CR4.OSFXSR is enabled before user tasks run and `image` has
        // the required 16-byte alignment and complete architectural extent.
        unsafe { core::arch::x86_64::_fxsave64(image.0.as_mut_ptr()) };
        let mask = u32::from_le_bytes(
            image.0[FXSAVE_MXCSR_MASK_OFFSET..FXSAVE_MXCSR_MASK_OFFSET + 4]
                .try_into()
                .expect("fixed MXCSR-mask slice"),
        );
        if mask == 0 { DEFAULT_MXCSR_MASK } else { mask }
    }
    #[cfg(not(target_os = "none"))]
    {
        DEFAULT_MXCSR_MASK
    }
}

/// Checks the legacy MXCSR field before it is passed to XRSTOR.
#[cfg(feature = "fp-simd")]
pub fn xsave_image_mxcsr_valid(image: &[u8]) -> bool {
    image.len() >= FXSAVE_MXCSR_MASK_OFFSET + 4
        && (u32::from_le_bytes(
            image[XSAVE_MXCSR_OFFSET..XSAVE_MXCSR_OFFSET + 4]
                .try_into()
                .expect("fixed MXCSR slice"),
        ) & !supported_mxcsr_mask())
            == 0
}

/// Standard-format layout for every XCR0-enabled user state component.
#[cfg(feature = "fp-simd")]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct XsaveLayout {
    /// Feature mask passed to XSAVE/XRSTOR.  A zero mask with a 512-byte
    /// extent denotes the explicit legacy FXSAVE/FXRSTOR fallback.
    pub xfeatures: u64,
    /// Number of bytes required for this standard XSAVE image.
    pub xstate_size: usize,
}

/// Why x87/SSE/PKRU standard XSAVE state cannot be used on this CPU.
#[cfg(feature = "fp-simd")]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum XsaveUnavailable {
    /// Hosted builds must not modify CR4 or XCR0.
    Hosted,
    /// CPUID does not advertise XSAVE support.
    MissingXsave,
    /// CPUID leaf `0xD` does not make every required state component available.
    MissingStateComponent,
    /// The CPU-described image is too large for the fixed task context.
    ImageTooLarge,
    /// XCR0 did not retain every required state component after enabling.
    Xcr0Rejected,
}

#[cfg(feature = "fp-simd")]
impl XsaveLayout {
    #[cfg(target_os = "none")]
    fn from_cpuid() -> Option<Self> {
        let leaf = core::arch::x86_64::__cpuid_count(0xD, 0);
        let supported = u64::from(leaf.eax) | (u64::from(leaf.edx) << 32);
        Some(Self {
            // Do not grow the kernel's XCR0 contract merely because the host
            // happens to implement AVX, AVX-512, or AMX.
            xfeatures: selected_xsave_features(supported, cfg!(feature = "pkeys"))?,
            xstate_size: 0,
        })
    }
}

#[cfg(feature = "fp-simd")]
const FXSAVE_LAYOUT: XsaveLayout = XsaveLayout {
    xfeatures: 0,
    xstate_size: FXSAVE_SIZE,
};

#[cfg(feature = "fp-simd")]
const fn uses_fxsave(layout: XsaveLayout) -> bool {
    layout.xfeatures == FXSAVE_LAYOUT.xfeatures && layout.xstate_size == FXSAVE_LAYOUT.xstate_size
}

/// Validates the standard XSAVE header before its bytes are passed to XRSTOR.
///
/// The legacy FXSAVE fallback has no XSAVE header.  Standard XSAVE images
/// must not claim a state component absent from this layout, use compacted
/// format, or carry nonzero reserved header bytes.  This check is deliberately
/// separate from MXCSR validation so every untrusted restore path can require
/// both invariants before executing a state-restoring instruction.
#[cfg(feature = "fp-simd")]
pub fn xsave_image_header_valid(layout: XsaveLayout, image: &[u8]) -> bool {
    if image.len() < layout.xstate_size {
        return false;
    }
    if uses_fxsave(layout) {
        return true;
    }
    if image.len() < XSAVE_HEADER_OFFSET + XSAVE_HEADER_SIZE {
        return false;
    }
    let xstate_bv = u64::from_le_bytes(
        image[XSAVE_XSTATE_BV_OFFSET..XSAVE_XSTATE_BV_OFFSET + 8]
            .try_into()
            .expect("fixed XSAVE XSTATE_BV slice"),
    );
    let xcomp_bv = u64::from_le_bytes(
        image[XSAVE_XCOMP_BV_OFFSET..XSAVE_XCOMP_BV_OFFSET + 8]
            .try_into()
            .expect("fixed XSAVE XCOMP_BV slice"),
    );
    xstate_bv & !layout.xfeatures == 0
        && xcomp_bv == 0
        && image[XSAVE_HEADER_RESERVED_OFFSET..XSAVE_HEADER_OFFSET + XSAVE_HEADER_SIZE]
            .iter()
            .all(|&byte| byte == 0)
}

#[cfg(all(feature = "fp-simd", target_os = "none"))]
fn publish_xsave_layout(layout: XsaveLayout) -> Result<XsaveLayout, XsaveUnavailable> {
    loop {
        match XSAVE_FLEET_STATE.load(Ordering::Acquire) {
            0 => {
                if XSAVE_FLEET_STATE
                    .compare_exchange(0, 1, Ordering::Acquire, Ordering::Acquire)
                    .is_ok()
                {
                    // State remains unpublished until both fields have been
                    // written, so an AP can never accept a mixed layout.
                    XSAVE_FLEET_FEATURES.store(layout.xfeatures, Ordering::Relaxed);
                    XSAVE_FLEET_SIZE.store(layout.xstate_size, Ordering::Relaxed);
                    XSAVE_FLEET_STATE.store(2, Ordering::Release);
                    return Ok(layout);
                }
            }
            1 => core::hint::spin_loop(),
            2 => {
                let fleet = XsaveLayout {
                    xfeatures: XSAVE_FLEET_FEATURES.load(Ordering::Relaxed),
                    xstate_size: XSAVE_FLEET_SIZE.load(Ordering::Relaxed),
                };
                return if fleet == layout {
                    Ok(layout)
                } else {
                    Err(XsaveUnavailable::Xcr0Rejected)
                };
            }
            _ => unreachable!("invalid XSAVE fleet publication state"),
        }
    }
}

#[cfg(all(feature = "fp-simd", target_os = "none"))]
fn published_xsave_layout() -> Option<XsaveLayout> {
    if XSAVE_FLEET_STATE.load(Ordering::Acquire) != 2 {
        return None;
    }
    Some(XsaveLayout {
        xfeatures: XSAVE_FLEET_FEATURES.load(Ordering::Relaxed),
        xstate_size: XSAVE_FLEET_SIZE.load(Ordering::Relaxed),
    })
}

/// Discovers the layout the hosted OS has already enabled.  A hosted process
/// must never write CR4 or XCR0, so a host without OSXSAVE uses the legacy
/// FXSAVE contract instead of attempting to manufacture an XSAVE policy.
#[cfg(all(feature = "fp-simd", not(target_os = "none")))]
fn hosted_xsave_layout() -> Result<XsaveLayout, XsaveUnavailable> {
    let features = core::arch::x86_64::__cpuid(1);
    if features.ecx & (1 << 26) == 0 || features.ecx & (1 << 27) == 0 {
        return Ok(FXSAVE_LAYOUT);
    }

    // XSAVE is advertised only with leaf 0xD, but retain the explicit bound
    // check before issuing the leaf on a virtual or unusual host.
    if core::arch::x86_64::__cpuid(0).eax < 0xD {
        return Ok(FXSAVE_LAYOUT);
    }
    let leaf = core::arch::x86_64::__cpuid_count(0xD, 0);
    let supported = u64::from(leaf.eax) | (u64::from(leaf.edx) << 32);
    if !xsave_has_required_components(supported) {
        return Err(XsaveUnavailable::MissingStateComponent);
    }
    // SAFETY: CPUID.OSXSAVE confirms that the host OS has enabled XGETBV.
    // This is strictly read-only; hosted code never writes CR4 or XCR0.
    let enabled = unsafe { core::arch::x86_64::_xgetbv(0) };
    if enabled & XSAVE_REQUIRED_XFEATURES != XSAVE_REQUIRED_XFEATURES {
        return Err(XsaveUnavailable::Xcr0Rejected);
    }
    let layout = XsaveLayout {
        // XCR0, not CPUID's superset, is the only state that XSAVE/XRSTOR may
        // touch in this process.  This also avoids assuming AMX/AVX-512 are
        // usable merely because the physical CPU implements them.
        xfeatures: enabled & supported,
        // CPUID.0D.0:EBX is the standard-format image size for this XCR0.
        xstate_size: leaf.ebx as usize,
    };
    if !(576..=MAX_XSAVE_SIZE).contains(&layout.xstate_size) {
        return Err(XsaveUnavailable::ImageTooLarge);
    }
    // Unlike the kernel target, hosted tests run as ordinary OS threads.  The
    // host may give different threads different XCR0 permissions (notably for
    // dynamically enabled AMX), so do not turn the first observed host layout
    // into a process-wide kernel fleet policy.  Save/restore revalidate the
    // current thread's layout and reject a mismatch before executing XSAVE.
    Ok(layout)
}

/// Enables the selected user XSAVE state and returns its
/// standard-format layout. Hosted builds do not touch CR4 or XCR0.
#[cfg(feature = "fp-simd")]
pub fn init_xsave_state() -> Result<XsaveLayout, XsaveUnavailable> {
    #[cfg(target_os = "none")]
    {
        let features = core::arch::x86_64::__cpuid(1);
        if features.ecx & (1 << 26) == 0 || core::arch::x86_64::__cpuid(0).eax < 0xD {
            return publish_xsave_layout(FXSAVE_LAYOUT);
        }
        let Some(mut layout) = XsaveLayout::from_cpuid() else {
            return publish_xsave_layout(FXSAVE_LAYOUT);
        };

        let mut cr4 = Cr4::read();
        cr4.insert(Cr4Flags::OSXSAVE);
        // SAFETY: CPUID verified XSAVE before enabling CR4.OSXSAVE.
        unsafe { Cr4::write(cr4) };

        // SAFETY: CR4.OSXSAVE is set above, so XGETBV/XSETBV access XCR0.
        unsafe { core::arch::x86_64::_xsetbv(0, layout.xfeatures) };
        // SAFETY: as above; this verifies that the processor retained the bits.
        let enabled = unsafe { core::arch::x86_64::_xgetbv(0) };
        if enabled != layout.xfeatures {
            return Err(XsaveUnavailable::Xcr0Rejected);
        }
        // CPUID.0D.0:EBX is the standard-format size for the active XCR0.
        layout.xstate_size = core::arch::x86_64::__cpuid_count(0xD, 0).ebx as usize;
        if !(576..=MAX_XSAVE_SIZE).contains(&layout.xstate_size) {
            return Err(XsaveUnavailable::ImageTooLarge);
        }
        publish_xsave_layout(layout)
    }
    #[cfg(not(target_os = "none"))]
    hosted_xsave_layout()
}

/// Returns the full enabled XSAVE layout without changing control registers.
#[cfg(feature = "fp-simd")]
pub fn xsave_layout() -> Result<XsaveLayout, XsaveUnavailable> {
    #[cfg(target_os = "none")]
    {
        if let Some(layout) = published_xsave_layout() {
            return Ok(layout);
        }
        let features = core::arch::x86_64::__cpuid(1);
        if features.ecx & (1 << 26) == 0 || core::arch::x86_64::__cpuid(0).eax < 0xD {
            return Ok(FXSAVE_LAYOUT);
        }
        let Some(mut layout) = XsaveLayout::from_cpuid() else {
            return Ok(FXSAVE_LAYOUT);
        };
        // SAFETY: only read XCR0 after the processor reports OSXSAVE.
        if features.ecx & (1 << 27) == 0 {
            return Ok(FXSAVE_LAYOUT);
        }
        let enabled = unsafe { core::arch::x86_64::_xgetbv(0) };
        if enabled & layout.xfeatures == layout.xfeatures {
            layout.xstate_size = core::arch::x86_64::__cpuid_count(0xD, 0).ebx as usize;
            if !(576..=MAX_XSAVE_SIZE).contains(&layout.xstate_size) {
                return Err(XsaveUnavailable::ImageTooLarge);
            }
            Ok(layout)
        } else {
            Err(XsaveUnavailable::Xcr0Rejected)
        }
    }
    #[cfg(not(target_os = "none"))]
    hosted_xsave_layout()
}

/// Saves every enabled user xfeature into a standard XSAVE image.
#[cfg(feature = "fp-simd")]
pub fn save_xsave(layout: XsaveLayout, image: &mut [u8]) -> bool {
    if image.len() < layout.xstate_size
        || (image.as_ptr() as usize) & 63 != 0
        || xsave_layout().ok() != Some(layout)
    {
        return false;
    }
    if uses_fxsave(layout) {
        // SAFETY: the layout was revalidated, and the caller provides a
        // complete, 64-byte-aligned legacy image (stronger than FXSAVE's
        // 16-byte alignment requirement).
        unsafe { core::arch::x86_64::_fxsave64(image.as_mut_ptr()) };
    } else {
        // SAFETY: the layout was revalidated for this CPU, the buffer is at
        // least the CPU-described size and its caller provides 64-byte
        // alignment.
        unsafe { core::arch::x86_64::_xsave64(image.as_mut_ptr(), layout.xfeatures) };
    }
    true
}

/// Restores every enabled user xfeature from a standard XSAVE image.
#[cfg(feature = "fp-simd")]
pub fn restore_xsave(layout: XsaveLayout, image: &[u8]) -> bool {
    if image.len() < layout.xstate_size
        || (image.as_ptr() as usize) & 63 != 0
        || xsave_layout().ok() != Some(layout)
        || !xsave_image_mxcsr_valid(image)
        || !xsave_image_header_valid(layout, image)
    {
        return false;
    }
    if uses_fxsave(layout) {
        // SAFETY: the validated legacy image is fully present and 64-byte
        // aligned, exceeding FXRSTOR's alignment requirement.
        unsafe { core::arch::x86_64::_fxrstor64(image.as_ptr()) };
    } else {
        // SAFETY: the layout was revalidated for this CPU, the buffer is at
        // least the CPU-described size and its caller provides 64-byte
        // alignment.
        unsafe { core::arch::x86_64::_xrstor64(image.as_ptr(), layout.xfeatures) };
    }
    true
}

/// Restores an XSAVE image after a caller has pinned the current CPU and
/// already verified this exact layout on that CPU.  Invalid images are
/// rejected without executing XRSTOR/FXRSTOR.
///
/// # Safety
///
/// `layout` must have been validated while the caller's CPU-pinning guard was
/// acquired and that guard must remain held. `image` must be 64-byte aligned,
/// exactly match the layout, and have been architecturally validated.
#[cfg(feature = "fp-simd")]
pub unsafe fn restore_xsave_pinned(layout: XsaveLayout, image: &[u8]) -> bool {
    if image.len() < layout.xstate_size
        || (image.as_ptr() as usize) & 63 != 0
        || !xsave_image_mxcsr_valid(image)
        || !xsave_image_header_valid(layout, image)
    {
        return false;
    }
    if uses_fxsave(layout) {
        // SAFETY: the caller upholds the documented CPU/layout/image contract.
        unsafe { core::arch::x86_64::_fxrstor64(image.as_ptr()) };
    } else {
        // SAFETY: the caller upholds the documented CPU/layout/image contract.
        unsafe { core::arch::x86_64::_xrstor64(image.as_ptr(), layout.xfeatures) };
    }
    true
}

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

/// Enables CR4.PKE only when the scheduler's XSAVE image preserves PKRU.
#[cfg(feature = "pkeys")]
pub fn init_pkeys() {
    #[cfg(target_os = "none")]
    {
        let capabilities = probe_pkey_capabilities();
        #[cfg(feature = "fp-simd")]
        let saved_features = xsave_layout().map_or(0, |layout| layout.xfeatures);
        #[cfg(not(feature = "fp-simd"))]
        let saved_features = 0;
        let enable = pkey_state_supported(capabilities.cpuid_pku, saved_features);
        if enable != capabilities.pke_enabled {
            let mut cr4 = Cr4::read();
            cr4.set(Cr4Flags::PROTECTION_KEY_USER, enable);
            // SAFETY: enabling requires CPUID PKU and saved PKRU state;
            // otherwise clear any bootloader PKE bit. Only CR4.PKE changes.
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
        probe_pkey_capabilities().usable()
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

// This identity belongs to the boot-owned address space, not the register
// which can currently name a reclaimable user address space.
static KERNEL_TASK_ROOT: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Publishes the permanent kernel address space for newly created tasks.
///
/// # Safety
/// The caller must retain this root and its kernel mappings for the entire
/// boot lifetime. Publication must precede scheduler/task initialization.
pub unsafe fn bind_kernel_task_page_table_root(root: PhysAddr) {
    let value = root.as_usize();
    assert!(
        value != 0 && value & 4095 == 0,
        "invalid permanent kernel root"
    );
    bind_kernel_root_slot(&KERNEL_TASK_ROOT, value);
}

fn bind_kernel_root_slot(slot: &core::sync::atomic::AtomicUsize, value: usize) {
    match slot.compare_exchange(0, value, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => {}
        Err(existing) => assert_eq!(existing, value, "permanent kernel root changed"),
    }
}

#[cfg(all(test, not(target_os = "none")))]
#[test]
fn permanent_kernel_root_binding_is_once_and_not_a_live_cr3() {
    let slot = core::sync::atomic::AtomicUsize::new(0);
    bind_kernel_root_slot(&slot, 0x1000);
    bind_kernel_root_slot(&slot, 0x1000);
    assert_eq!(slot.load(Ordering::Acquire), 0x1000);
    assert!(std::panic::catch_unwind(|| bind_kernel_root_slot(&slot, 0x9000)).is_err());
    assert_eq!(slot.load(Ordering::Acquire), 0x1000);
}

/// Returns the boot-owned root for a kernel task, independently of CR3.
/// Bare-metal task creation before boot publication is an invariant failure.
pub fn kernel_task_page_table_root() -> PhysAddr {
    let value = KERNEL_TASK_ROOT.load(Ordering::Acquire);
    #[cfg(target_os = "none")]
    assert!(value != 0, "kernel task root has not been published");
    // Host context-only tests have no hardware root or paging bootstrap.
    PhysAddr::from_usize(value)
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
///
/// # Safety
///
/// `base..base + bytes` must describe a valid LDT descriptor table for the
/// lifetime described above.
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

#[cfg(all(test, feature = "pkeys"))]
mod pkey_state_tests {
    #[test]
    fn pke_admission_requires_cpu_support_and_saved_pkru() {
        let pkru = super::XSAVE_PKRU_XFEATURE;
        assert!(!super::pkey_state_supported(true, 0)); // FXSAVE or no fp-simd.
        assert!(!super::pkey_state_supported(true, 3)); // x87/SSE cannot save PKRU.
        assert!(!super::pkey_state_supported(false, 3 | pkru));
        assert!(super::pkey_state_supported(true, 3 | pkru));
    }
}

#[cfg(all(test, feature = "fp-simd", not(target_os = "none")))]
mod hosted_xsave_tests {
    use super::{
        FXSAVE_LAYOUT, MAX_XSAVE_SIZE, XSAVE_HEADER_RESERVED_OFFSET, XSAVE_REQUIRED_XFEATURES,
        XSAVE_XCOMP_BV_OFFSET, XSAVE_XSTATE_BV_OFFSET, uses_fxsave, xsave_image_header_valid,
        xsave_layout,
    };

    #[repr(align(64))]
    struct Image([u8; MAX_XSAVE_SIZE]);

    #[test]
    fn kernel_xsave_selection_adds_only_supported_requested_pkru() {
        let base = XSAVE_REQUIRED_XFEATURES;
        let pkru = super::XSAVE_PKRU_XFEATURE;
        let wider_simd = (1 << 2) | (1 << 5) | (1 << 6) | (1 << 7) | (1 << 17) | (1 << 18);
        assert_eq!(
            super::selected_xsave_features(base | wider_simd, true),
            Some(base)
        );
        assert_eq!(
            super::selected_xsave_features(base | pkru | wider_simd, true),
            Some(base | pkru)
        );
        assert_eq!(
            super::selected_xsave_features(base | pkru | wider_simd, false),
            Some(base)
        );
        assert_eq!(super::selected_xsave_features(pkru, true), None);
        assert_eq!(super::selected_xsave_features((1 << 0) | pkru, true), None);
        assert_eq!(super::selected_xsave_features((1 << 1) | pkru, true), None);
    }

    #[test]
    fn layout_uses_only_host_enabled_state_or_legacy_fxsave() {
        let layout = xsave_layout().expect("hosted xsave backend must expose a safe layout");
        if uses_fxsave(layout) {
            assert_eq!(layout, FXSAVE_LAYOUT);
        } else {
            assert_eq!(
                layout.xfeatures & XSAVE_REQUIRED_XFEATURES,
                XSAVE_REQUIRED_XFEATURES
            );
            assert!((576..=MAX_XSAVE_SIZE).contains(&layout.xstate_size));
        }

        let mut image = Image([0; MAX_XSAVE_SIZE]);
        assert!(super::save_xsave(
            layout,
            &mut image.0[..layout.xstate_size]
        ));
    }

    #[test]
    fn missing_x87_or_sse_selects_the_legacy_contract() {
        assert!(!super::xsave_has_required_components(0));
        assert!(!super::xsave_has_required_components(1 << 0));
        assert!(!super::xsave_has_required_components(1 << 1));
        assert!(super::xsave_has_required_components(
            super::XSAVE_REQUIRED_XFEATURES
        ));
        assert!(super::xsave_has_required_components(
            super::XSAVE_REQUIRED_XFEATURES | (1 << 2)
        ));
    }

    #[test]
    fn hostile_standard_xsave_headers_are_rejected_before_restore() {
        let layout = xsave_layout().expect("hosted xsave backend must expose a safe layout");
        let mut image = Image([0; MAX_XSAVE_SIZE]);
        assert!(super::save_xsave(
            layout,
            &mut image.0[..layout.xstate_size]
        ));
        assert!(xsave_image_header_valid(
            layout,
            &image.0[..layout.xstate_size]
        ));
        if uses_fxsave(layout) {
            return;
        }

        let clean = image.0;
        // XSTATE_BV must not describe a component outside the XCR0 layout.
        image.0 = clean;
        image.0[XSAVE_XSTATE_BV_OFFSET + 7] |= 0x80;
        assert!(!xsave_image_header_valid(
            layout,
            &image.0[..layout.xstate_size]
        ));
        assert!(!super::restore_xsave(
            layout,
            &image.0[..layout.xstate_size]
        ));
        // SAFETY: the deliberately invalid header is rejected before either
        // state-restoring instruction can execute.
        assert!(!unsafe { super::restore_xsave_pinned(layout, &image.0[..layout.xstate_size]) });

        // Standard-format images may not set XCOMP_BV, including its compacted
        // format flag, because this kernel only provisions standard layouts.
        image.0 = clean;
        image.0[XSAVE_XCOMP_BV_OFFSET] = 1;
        assert!(!xsave_image_header_valid(
            layout,
            &image.0[..layout.xstate_size]
        ));
        assert!(!super::restore_xsave(
            layout,
            &image.0[..layout.xstate_size]
        ));

        image.0 = clean;
        image.0[XSAVE_HEADER_RESERVED_OFFSET] = 1;
        assert!(!xsave_image_header_valid(
            layout,
            &image.0[..layout.xstate_size]
        ));
        assert!(!super::restore_xsave(
            layout,
            &image.0[..layout.xstate_size]
        ));
    }
}
