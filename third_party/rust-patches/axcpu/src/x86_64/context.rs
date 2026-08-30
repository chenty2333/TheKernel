use core::{arch::naked_asm, fmt};

use memory_addr::VirtAddr;

/// Saved registers when a trap (interrupt or exception) occurs.
#[allow(missing_docs)]
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct TrapFrame {
    pub rax: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rbx: u64,
    pub rbp: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,

    // Pushed by `trap.S`
    pub vector: u64,
    pub error_code: u64,

    // Pushed by CPU
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

impl TrapFrame {
    /// Gets the 0th syscall argument.
    pub const fn arg0(&self) -> usize {
        self.rdi as _
    }

    /// Sets the 0th syscall argument.
    pub const fn set_arg0(&mut self, rdi: usize) {
        self.rdi = rdi as _;
    }

    /// Gets the 1st syscall argument.
    pub const fn arg1(&self) -> usize {
        self.rsi as _
    }

    /// Sets the 1st syscall argument.
    pub const fn set_arg1(&mut self, rsi: usize) {
        self.rsi = rsi as _;
    }

    /// Gets the 2nd syscall argument.
    pub const fn arg2(&self) -> usize {
        self.rdx as _
    }

    /// Sets the 2nd syscall argument.
    pub const fn set_arg2(&mut self, rdx: usize) {
        self.rdx = rdx as _;
    }

    /// Gets the 3rd syscall argument.
    pub const fn arg3(&self) -> usize {
        self.r10 as _
    }

    /// Sets the 3rd syscall argument.
    pub const fn set_arg3(&mut self, r10: usize) {
        self.r10 = r10 as _;
    }

    /// Gets the 4th syscall argument.
    pub const fn arg4(&self) -> usize {
        self.r8 as _
    }

    /// Sets the 4th syscall argument.
    pub const fn set_arg4(&mut self, r8: usize) {
        self.r8 = r8 as _;
    }

    /// Gets the 5th syscall argument.
    pub const fn arg5(&self) -> usize {
        self.r9 as _
    }

    /// Sets the 5th syscall argument.
    pub const fn set_arg5(&mut self, r9: usize) {
        self.r9 = r9 as _;
    }

    /// Gets the instruction pointer.
    pub const fn ip(&self) -> usize {
        self.rip as _
    }

    /// Sets the instruction pointer.
    pub const fn set_ip(&mut self, rip: usize) {
        self.rip = rip as _;
    }

    /// Gets the stack pointer.
    pub const fn sp(&self) -> usize {
        self.rsp as _
    }

    /// Sets the stack pointer.
    pub const fn set_sp(&mut self, rsp: usize) {
        self.rsp = rsp as _;
    }

    /// Gets the syscall number.
    pub const fn sysno(&self) -> usize {
        self.rax as usize
    }

    /// Sets the syscall number.
    pub const fn set_sysno(&mut self, rax: usize) {
        self.rax = rax as _;
    }

    /// Gets the return value register.
    pub const fn retval(&self) -> usize {
        self.rax as _
    }

    /// Sets the return value register.
    pub const fn set_retval(&mut self, rax: usize) {
        self.rax = rax as _;
    }

    /// Unwind the stack and get the backtrace.
    pub fn backtrace(&self) -> axbacktrace::Backtrace {
        axbacktrace::Backtrace::capture_trap(self.rbp as _, self.rip as _, 0)
    }
}

#[repr(C)]
#[derive(Debug, Default)]
struct ContextSwitchFrame {
    r15: u64,
    r14: u64,
    r13: u64,
    r12: u64,
    rbx: u64,
    rbp: u64,
    rip: u64,
}

/// A 512-byte memory region for the FXSAVE/FXRSTOR instruction to save and
/// restore the x87 FPU, MMX, XMM, and MXCSR registers.
///
/// See <https://www.felixcloutier.com/x86/fxsave> for more details.
#[allow(missing_docs)]
#[repr(C, align(16))]
#[derive(Debug)]
pub struct FxsaveArea {
    pub fcw: u16,
    pub fsw: u16,
    pub ftw: u16,
    pub fop: u16,
    pub fip: u64,
    pub fdp: u64,
    pub mxcsr: u32,
    pub mxcsr_mask: u32,
    pub st: [u64; 16],
    pub xmm: [u64; 32],
    _padding: [u64; 12],
}

static_assertions::const_assert_eq!(core::mem::size_of::<FxsaveArea>(), 512);

/// Extended state of a task, such as FP/SIMD states.
pub struct ExtendedState {
    /// Memory region for the FXSAVE/FXRSTOR instruction.
    pub fxsave_area: FxsaveArea,
}

#[cfg(feature = "fp-simd")]
impl ExtendedState {
    /// Saves the current extended states from CPU to this structure.
    #[inline]
    pub fn save(&mut self) {
        unsafe { core::arch::x86_64::_fxsave64(&mut self.fxsave_area as *mut _ as *mut u8) }
    }

    /// Restores the extended states from this structure to CPU.
    #[inline]
    pub fn restore(&self) {
        unsafe { core::arch::x86_64::_fxrstor64(&self.fxsave_area as *const _ as *const u8) }
    }

    /// Returns the extended state with initialized values.
    pub const fn default() -> Self {
        let mut area: FxsaveArea = unsafe { core::mem::MaybeUninit::zeroed().assume_init() };
        area.fcw = 0x37f;
        // FXSAVE stores the abridged tag word (one bit per non-empty x87
        // register), so an FNINIT-equivalent state has every bit clear. The
        // architectural x87 tag word's `0xffff` encoding must not be copied
        // into this field: FXRSTOR would interpret it as eight non-empty
        // registers.
        area.ftw = 0;
        area.mxcsr = 0x1f80;
        Self { fxsave_area: area }
    }

    /// Replaces the saved state and the live CPU state with the architectural
    /// reset values used by a newly created user image.
    ///
    /// The caller must invoke this only for the task currently executing on
    /// this CPU. `restore` updates the live FPU/SIMD registers as well as the
    /// saved image; a later context switch will therefore not save registers
    /// belonging to the old executable.
    pub fn reset(&mut self) {
        *self = Self::default();
        self.restore();
    }
}

impl fmt::Debug for ExtendedState {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("ExtendedState")
            .field("fxsave_area", &self.fxsave_area)
            .finish()
    }
}

#[cfg(all(feature = "uspace", feature = "asid-fast-switch"))]
#[inline]
fn legal_legacy_identity(
    root: usize,
    pcid: usize,
    generation: u64,
    fallback: crate::AddressSpaceFallbackReason,
) -> bool {
    pcid == 0
        && root & 0xfff == 0
        && root < (1usize << 52)
        && generation == 0
        && !matches!(fallback, crate::AddressSpaceFallbackReason::None)
}

#[cfg(all(feature = "uspace", feature = "asid-fast-switch"))]
#[inline]
#[allow(clippy::too_many_arguments)]
fn user_address_space_identity_changed(
    current_root: usize,
    current_pcid: usize,
    current_generation: u64,
    current_fallback: crate::AddressSpaceFallbackReason,
    next_root: usize,
    next_pcid: usize,
    next_generation: u64,
    next_fallback: crate::AddressSpaceFallbackReason,
) -> bool {
    let metadata_invalid = (!legal_legacy_identity(
        current_root,
        current_pcid,
        current_generation,
        current_fallback,
    ) && current_pcid == 0)
        || (current_pcid != 0
            && !crate::legal_nonzero_identity(
                current_root,
                current_pcid,
                current_generation,
                current_fallback,
            ))
        || (next_pcid != 0
            && !crate::legal_nonzero_identity(
                next_root,
                next_pcid,
                next_generation,
                next_fallback,
            ));
    current_root != next_root
        || current_pcid != next_pcid
        || current_generation != next_generation
        || current_fallback != next_fallback
        || metadata_invalid
}

/// Saved hardware states of a task.
///
/// The context usually includes:
///
/// - Callee-saved registers
/// - Stack pointer register
/// - Thread pointer register (for kernel-space thread-local storage)
/// - FP/SIMD registers
///
/// On context switch, current task saves its context from CPU to memory,
/// and the next task restores its context from memory to CPU.
///
/// On x86_64, callee-saved registers are saved to the kernel stack by the
/// `PUSH` instruction. So that [`rsp`] is the `RSP` after callee-saved
/// registers are pushed, and [`kstack_top`] is the top of the kernel stack
/// (`RSP` before any push).
///
/// [`rsp`]: TaskContext::rsp
/// [`kstack_top`]: TaskContext::kstack_top
#[derive(Debug)]
pub struct TaskContext {
    /// The kernel stack top of the task.
    pub kstack_top: VirtAddr,
    /// `RSP` after all callee-saved registers are pushed.
    pub rsp: u64,
    /// Thread pointer (FS segment base address)
    pub fs_base: usize,
    /// Per-task user CET MSR state, switched separately from PKRU.
    pub user_cet: crate::asm::UserCetState,
    /// Extended states, i.e., FP/SIMD states.
    #[cfg(feature = "fp-simd")]
    pub ext_state: ExtendedState,
    /// The task's user protection-key permissions.
    #[cfg(feature = "pkeys")]
    pkru: u32,
    /// The `CR3` register value, i.e., the page table root.
    #[cfg(feature = "uspace")]
    pub cr3: memory_addr::PhysAddr,
    /// The non-recycled PCID associated with [`Self::cr3`].
    #[cfg(all(feature = "uspace", feature = "asid-fast-switch"))]
    pub cr3_pcid: usize,
    /// The boot-scoped allocator generation associated with [`Self::cr3_pcid`].
    #[cfg(all(feature = "uspace", feature = "asid-fast-switch"))]
    pub cr3_generation: u64,
    /// Why [`Self::cr3_pcid`] is the conservative PCID-0 fallback.
    #[cfg(all(feature = "uspace", feature = "asid-fast-switch"))]
    pub cr3_fallback_reason: crate::AddressSpaceFallbackReason,
}

impl TaskContext {
    /// Creates a dummy context for a new task.
    ///
    /// Note the context is not initialized, it will be filled by [`switch_to`]
    /// (for initial tasks) and [`init`] (for regular tasks) methods.
    ///
    /// [`init`]: TaskContext::init
    /// [`switch_to`]: TaskContext::switch_to
    pub fn new() -> Self {
        Self {
            kstack_top: va!(0),
            rsp: 0,
            fs_base: 0,
            user_cet: crate::asm::UserCetState::default(),
            #[cfg(feature = "uspace")]
            cr3: crate::asm::read_kernel_page_table(),
            #[cfg(all(feature = "uspace", feature = "asid-fast-switch"))]
            cr3_pcid: 0,
            #[cfg(all(feature = "uspace", feature = "asid-fast-switch"))]
            cr3_generation: 0,
            #[cfg(all(feature = "uspace", feature = "asid-fast-switch"))]
            cr3_fallback_reason: crate::AddressSpaceFallbackReason::AsidZero,
            #[cfg(feature = "fp-simd")]
            ext_state: ExtendedState::default(),
            #[cfg(feature = "pkeys")]
            pkru: crate::asm::PKRU_DEFAULT,
        }
    }

    /// Initializes the context for a new task, with the given entry point and
    /// kernel stack.
    pub fn init(&mut self, entry: usize, kstack_top: VirtAddr, tls_area: VirtAddr) {
        unsafe {
            // x86_64 calling convention: the stack must be 16-byte aligned before
            // calling a function. That means when entering a new task (`ret` in `context_switch`
            // is executed), (stack pointer + 8) should be 16-byte aligned.
            let frame_ptr = (kstack_top.as_mut_ptr() as *mut u64).sub(1);
            let frame_ptr = (frame_ptr as *mut ContextSwitchFrame).sub(1);
            core::ptr::write(
                frame_ptr,
                ContextSwitchFrame {
                    rip: entry as _,
                    ..Default::default()
                },
            );
            self.rsp = frame_ptr as u64;
        }
        self.kstack_top = kstack_top;
        self.fs_base = tls_area.as_usize();
    }

    /// Resets this task's saved and live FP/SIMD state.
    ///
    /// The context must belong to the task currently executing on this CPU.
    /// This is used by `execve` after replacing a process image so the new
    /// image cannot observe registers from the old image.
    #[cfg(feature = "fp-simd")]
    pub fn reset_extended_state(&mut self) {
        self.ext_state.reset();
    }

    /// Returns this task's scheduler-saved PKRU value.
    #[cfg(feature = "pkeys")]
    #[inline]
    pub const fn saved_pkru(&self) -> u32 {
        self.pkru
    }

    /// Replaces this task's scheduler-saved PKRU value.
    ///
    /// This only updates the saved context. Call [`Self::set_current_pkru`]
    /// when this is the task currently executing on this CPU.
    #[cfg(feature = "pkeys")]
    #[inline]
    pub fn set_saved_pkru(&mut self, pkru: u32) {
        self.pkru = pkru;
    }

    /// Copies the PKRU state inherited by a newly created task.
    #[cfg(feature = "pkeys")]
    #[inline]
    pub fn inherit_pkru_from(&mut self, parent: &Self) {
        self.pkru = parent.pkru;
    }

    /// Snapshots live PKRU into this context when it is current.
    #[cfg(feature = "pkeys")]
    #[inline]
    pub fn save_current_pkru(&mut self) {
        if let Some(pkru) = crate::asm::read_pkru() {
            self.pkru = pkru;
        }
    }

    /// Changes both the saved and live PKRU state of the current task.
    #[cfg(feature = "pkeys")]
    #[inline]
    pub fn set_current_pkru(&mut self, pkru: u32) {
        self.pkru = pkru;
        let _ = crate::asm::write_pkru(pkru);
    }

    /// Restores the default permissions for the current task.
    #[cfg(feature = "pkeys")]
    #[inline]
    pub fn reset_current_pkru(&mut self) {
        self.set_current_pkru(crate::asm::PKRU_DEFAULT);
    }

    /// Replaces CET state for the running task and hardware CPU state.
    #[inline]
    pub fn set_current_user_cet_state(&mut self, state: crate::asm::UserCetState) {
        self.user_cet = state;
        crate::asm::write_user_cet_state(state);
    }

    /// Updates an unpublished task's scheduler image without touching the
    /// CET MSRs owned by the task currently running on this CPU.
    #[inline]
    pub fn set_saved_user_cet_state(&mut self, state: crate::asm::UserCetState) {
        self.user_cet = state;
    }

    /// Changes the page table root in this context.
    ///
    /// The hardware register for page table root (`CR3` for x86) will be
    /// updated to the next task's after [`Self::switch_to`].
    #[cfg(feature = "uspace")]
    pub fn set_page_table_root(&mut self, cr3: memory_addr::PhysAddr) {
        self.cr3 = cr3;
        #[cfg(feature = "asid-fast-switch")]
        {
            self.cr3_pcid = 0;
            self.cr3_generation = 0;
            self.cr3_fallback_reason = crate::AddressSpaceFallbackReason::AsidZero;
        }
    }

    /// Changes the user root and its bounded hardware PCID identity.
    ///
    /// # Safety
    ///
    /// The numeric PCID must identify this root for the entire boot and must
    /// not be recycled while a CPU can still refill the old identity.
    #[cfg(all(feature = "uspace", feature = "asid-fast-switch"))]
    pub unsafe fn set_page_table_root_with_asid(
        &mut self,
        cr3: memory_addr::PhysAddr,
        pcid: usize,
        generation: u64,
        fallback_reason: crate::AddressSpaceFallbackReason,
    ) {
        self.cr3 = cr3;
        self.cr3_pcid = pcid;
        self.cr3_generation = generation;
        self.cr3_fallback_reason = fallback_reason;
    }

    /// Switches to another task.
    ///
    /// It first saves the current task's context from CPU to this place, and then
    /// restores the next task's context from `next_ctx` to CPU.
    pub fn switch_to(&mut self, next_ctx: &Self) {
        let live_cet = crate::asm::read_user_cet_state();
        self.user_cet.u_cet = live_cet.u_cet;
        self.user_cet.pl3_ssp = live_cet.pl3_ssp;
        crate::asm::write_user_cet_state(next_ctx.user_cet);
        #[cfg(feature = "pkeys")]
        {
            self.save_current_pkru();
            let _ = crate::asm::write_pkru(next_ctx.pkru);
        }
        #[cfg(feature = "fp-simd")]
        {
            self.ext_state.save();
            next_ctx.ext_state.restore();
        }
        #[cfg(feature = "tls")]
        unsafe {
            self.fs_base = crate::asm::read_thread_pointer();
            crate::asm::write_thread_pointer(next_ctx.fs_base);
        }
        #[cfg(all(feature = "uspace", not(feature = "asid-fast-switch")))]
        unsafe {
            if next_ctx.cr3 != self.cr3 {
                crate::asm::write_user_page_table(next_ctx.cr3);
                // writing to CR3 has flushed the TLB
            }
        }
        #[cfg(all(feature = "uspace", feature = "asid-fast-switch"))]
        {
            // The numeric PCID/root pair is not the complete identity.  A
            // generation or fallback transition must still pass through the
            // classifier so a stale/invalid identity cannot take NOFLUSH.
            let identity_changed = user_address_space_identity_changed(
                self.cr3.as_usize(),
                self.cr3_pcid,
                self.cr3_generation,
                self.cr3_fallback_reason,
                next_ctx.cr3.as_usize(),
                next_ctx.cr3_pcid,
                next_ctx.cr3_generation,
                next_ctx.cr3_fallback_reason,
            );
            if identity_changed {
                let decision = if self.cr3_pcid == 0
                    && !legal_legacy_identity(
                        self.cr3.as_usize(),
                        self.cr3_pcid,
                        self.cr3_generation,
                        self.cr3_fallback_reason,
                    ) {
                    crate::TlbSwitchDecision::Flush(crate::AsidSwitchFallbackReason::InvalidWidth)
                } else {
                    crate::classify_user_tlb_switch(
                        self.cr3.as_usize(),
                        self.cr3_pcid,
                        self.cr3_generation,
                        self.cr3_fallback_reason,
                        next_ctx.cr3.as_usize(),
                        next_ctx.cr3_pcid,
                        next_ctx.cr3_generation,
                        next_ctx.cr3_fallback_reason,
                    )
                };
                let target_is_legal = crate::legal_nonzero_identity(
                    next_ctx.cr3.as_usize(),
                    next_ctx.cr3_pcid,
                    next_ctx.cr3_generation,
                    next_ctx.cr3_fallback_reason,
                );
                // A legal never-reused target can always use NOFLUSH,
                // including a transition from the kernel's PCID-0 context.
                // A defensive flush is used for an invalid current identity
                // or a same-PCID/root collision. PCID 0 is always entered by
                // a flushing CR3 write and never with bit 63 set.
                unsafe {
                    if target_is_legal {
                        if matches!(decision, crate::TlbSwitchDecision::Retain) {
                            crate::asm::write_user_page_table_with_asid(
                                next_ctx.cr3,
                                next_ctx.cr3_pcid,
                            );
                        } else {
                            crate::asm::write_user_page_table_with_asid_flush(
                                next_ctx.cr3,
                                next_ctx.cr3_pcid,
                            );
                        }
                    } else if next_ctx.cr3_pcid != 0 {
                        // Metadata is invalid, but a structurally valid
                        // nonzero PCID can still be flushed defensively.  The
                        // helper rejects malformed roots/PCIDs and falls back
                        // to the PCID-0 full-flush write in that case.
                        crate::asm::write_user_page_table_with_asid_flush(
                            next_ctx.cr3,
                            next_ctx.cr3_pcid,
                        );
                    } else {
                        crate::asm::write_user_page_table(next_ctx.cr3);
                    }
                }
                #[cfg(feature = "asid-switch-diagnostics")]
                crate::record_asid_switch_decision(decision);
            }
        }
        unsafe { context_switch(&mut self.rsp, &next_ctx.rsp) }
    }
}

#[unsafe(naked)]
unsafe extern "C" fn context_switch(_current_stack: &mut u64, _next_stack: &u64) {
    naked_asm!(
        "
        .code64
        push    rbp
        push    rbx
        push    r12
        push    r13
        push    r14
        push    r15
        mov     [rdi], rsp

        mov     rsp, [rsi]
        pop     r15
        pop     r14
        pop     r13
        pop     r12
        pop     rbx
        pop     rbp
        ret",
    )
}

#[cfg(all(test, feature = "fp-simd"))]
mod tests {
    use super::ExtendedState;

    #[test]
    fn reset_replaces_saved_and_live_fxsave_image() {
        let mut state = ExtendedState::default();
        state.fxsave_area.fcw = 0;
        state.fxsave_area.ftw = 0xff;
        state.fxsave_area.mxcsr = 0;
        state.fxsave_area.xmm[0] = u64::MAX;

        state.reset();

        assert_eq!(state.fxsave_area.fcw, 0x37f);
        assert_eq!(state.fxsave_area.ftw, 0);
        assert_eq!(state.fxsave_area.mxcsr, 0x1f80);
        assert_eq!(state.fxsave_area.xmm[0], 0);

        // Save the live CPU state after reset as well. This catches an
        // implementation that only rewrites the scheduler copy.
        state.save();
        assert_eq!(state.fxsave_area.fcw, 0x37f);
        assert_eq!(state.fxsave_area.ftw, 0);
        assert_eq!(state.fxsave_area.mxcsr, 0x1f80);
        assert_eq!(state.fxsave_area.xmm[0], 0);
    }
}

#[cfg(all(test, feature = "pkeys"))]
mod pkey_tests {
    use super::TaskContext;
    use crate::asm::PKRU_DEFAULT;

    #[test]
    fn new_context_starts_with_default_pkru() {
        assert_eq!(TaskContext::new().saved_pkru(), PKRU_DEFAULT);
    }

    #[test]
    fn inherited_pkru_is_a_snapshot() {
        let mut parent = TaskContext::new();
        parent.set_saved_pkru(0xa5a5_5a5a);
        let mut child = TaskContext::new();
        child.inherit_pkru_from(&parent);
        parent.set_saved_pkru(0);

        assert_eq!(child.saved_pkru(), 0xa5a5_5a5a);
    }
}

#[cfg(all(test, feature = "uspace", feature = "asid-fast-switch"))]
mod asid_tests {
    use super::{legal_legacy_identity, user_address_space_identity_changed};
    use crate::AddressSpaceFallbackReason;

    #[test]
    fn same_root_and_pcid_metadata_changes_enter_switch_path() {
        assert!(user_address_space_identity_changed(
            0x1000,
            7,
            1,
            AddressSpaceFallbackReason::None,
            0x1000,
            7,
            2,
            AddressSpaceFallbackReason::None,
        ));
        assert!(user_address_space_identity_changed(
            0x1000,
            7,
            1,
            AddressSpaceFallbackReason::None,
            0x1000,
            7,
            1,
            AddressSpaceFallbackReason::Exhausted,
        ));
    }

    #[test]
    fn identical_invalid_nonzero_metadata_still_enters_defensive_path() {
        assert!(user_address_space_identity_changed(
            0x1000,
            7,
            0,
            AddressSpaceFallbackReason::None,
            0x1000,
            7,
            0,
            AddressSpaceFallbackReason::None,
        ));
        assert!(!user_address_space_identity_changed(
            0x1000,
            7,
            1,
            AddressSpaceFallbackReason::None,
            0x1000,
            7,
            1,
            AddressSpaceFallbackReason::None,
        ));
    }

    #[test]
    fn legacy_identity_with_invalid_metadata_cannot_retain_a_target_pcid() {
        assert!(!legal_legacy_identity(
            0x1000,
            0,
            0,
            AddressSpaceFallbackReason::None,
        ));
        assert!(user_address_space_identity_changed(
            0x1000,
            0,
            0,
            AddressSpaceFallbackReason::None,
            0x2000,
            7,
            1,
            AddressSpaceFallbackReason::None,
        ));
    }
}
