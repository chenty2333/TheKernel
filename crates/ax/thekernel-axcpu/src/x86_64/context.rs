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

/// Full standard-format XSAVE storage.  The logical size and enabled XCR0
/// mask come from [`crate::asm::XsaveLayout`]; the fixed upper bound prevents
/// any CPUID result from overrunning a scheduler context.
#[repr(C, align(64))]
#[derive(Clone)]
pub struct ExtendedState {
    bytes: [u8; crate::asm::MAX_XSAVE_SIZE],
}

#[cfg(feature = "fp-simd")]
impl ExtendedState {
    /// Saves the current extended states from CPU to this structure.
    #[inline]
    pub fn save(&mut self) {
        let layout = crate::asm::xsave_layout().expect("XSAVE was not initialized on this CPU");
        assert!(crate::asm::save_xsave(layout, self.bytes_mut(layout)));
    }

    /// Restores the extended states from this structure to CPU.
    #[inline]
    pub fn restore(&self) {
        let layout = crate::asm::xsave_layout().expect("XSAVE was not initialized on this CPU");
        assert!(crate::asm::restore_xsave(layout, self.bytes(layout)));
    }

    /// Returns the extended state with initialized values.
    pub const fn default() -> Self {
        let mut bytes = [0; crate::asm::MAX_XSAVE_SIZE];
        // The legacy region remains part of every XSAVE image.  These are the
        // architectural FINIT/LDMXCSR initial values; absent components are
        // reset by XRSTOR rather than borrowed from a prior task.
        bytes[0] = 0x7f;
        bytes[1] = 0x03;
        bytes[24] = 0x80;
        bytes[25] = 0x1f;
        Self { bytes }
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
            .field("bytes", &"full XSAVE image")
            .finish()
    }
}

#[cfg(feature = "fp-simd")]
impl ExtendedState {
    /// View only the CPU-selected standard XSAVE extent.
    pub fn snapshot(&self) -> (&[u8], crate::asm::XsaveLayout) {
        let layout = crate::asm::xsave_layout().expect("XSAVE was not initialized on this CPU");
        (self.bytes(layout), layout)
    }

    /// Replaces the saved state after the caller validated exact layout and
    /// XSAVE header contents.  This never accepts a prefix or trailing bytes.
    pub fn replace_snapshot(&mut self, layout: crate::asm::XsaveLayout, image: &[u8]) -> bool {
        if crate::asm::xsave_layout().ok() != Some(layout) || image.len() != layout.xstate_size {
            return false;
        }
        self.bytes_mut(layout).copy_from_slice(image);
        true
    }

    fn bytes(&self, layout: crate::asm::XsaveLayout) -> &[u8] {
        &self.bytes[..layout.xstate_size]
    }

    fn bytes_mut(&mut self, layout: crate::asm::XsaveLayout) -> &mut [u8] {
        &mut self.bytes[..layout.xstate_size]
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
    /// Initializes a context directly in its final allocation without placing
    /// its extended-state image on the caller's stack.
    ///
    /// # Safety
    ///
    /// `out` must be aligned, writable storage for one `Self`, exclusively
    /// owned by the caller. Any previous value must already have been dropped.
    pub unsafe fn initialize_at(out: *mut Self) {
        unsafe {
            Self::initialize_at_with_kernel_root(
                out,
                #[cfg(feature = "uspace")]
                crate::asm::kernel_task_page_table_root(),
            );
        }
    }

    unsafe fn initialize_at_with_kernel_root(
        out: *mut Self,
        #[cfg(feature = "uspace")] kernel_root: memory_addr::PhysAddr,
    ) {
        // SAFETY: the caller supplies exclusive, aligned storage. Each field
        // is initialized through a raw pointer before any reference is made.
        unsafe {
            core::ptr::addr_of_mut!((*out).kstack_top).write(va!(0));
            core::ptr::addr_of_mut!((*out).rsp).write(0);
            core::ptr::addr_of_mut!((*out).fs_base).write(0);
            core::ptr::addr_of_mut!((*out).user_cet).write(crate::asm::UserCetState::default());
            #[cfg(feature = "uspace")]
            core::ptr::addr_of_mut!((*out).cr3).write(kernel_root);
            #[cfg(all(feature = "uspace", feature = "asid-fast-switch"))]
            {
                core::ptr::addr_of_mut!((*out).cr3_pcid).write(0);
                core::ptr::addr_of_mut!((*out).cr3_generation).write(0);
                core::ptr::addr_of_mut!((*out).cr3_fallback_reason)
                    .write(crate::AddressSpaceFallbackReason::AsidZero);
            }
            #[cfg(feature = "fp-simd")]
            {
                let bytes = core::ptr::addr_of_mut!((*out).ext_state.bytes).cast::<u8>();
                bytes.write_bytes(0, crate::asm::MAX_XSAVE_SIZE);
                bytes.write(0x7f);
                bytes.add(1).write(0x03);
                bytes.add(24).write(0x80);
                bytes.add(25).write(0x1f);
            }
            // Exhaustive patterns make newly added fields a compile error
            // here, so their initialization cannot silently be omitted.
            let Self {
                kstack_top: _,
                rsp: _,
                fs_base: _,
                user_cet: _,
                #[cfg(feature = "fp-simd")]
                    ext_state: ExtendedState { bytes: _ },
                #[cfg(feature = "uspace")]
                    cr3: _,
                #[cfg(all(feature = "uspace", feature = "asid-fast-switch"))]
                    cr3_pcid: _,
                #[cfg(all(feature = "uspace", feature = "asid-fast-switch"))]
                    cr3_generation: _,
                #[cfg(all(feature = "uspace", feature = "asid-fast-switch"))]
                    cr3_fallback_reason: _,
            } = &*out;
        }
    }

    /// Creates a dummy context for a new task.
    ///
    /// Note the context is not initialized, it will be filled by [`switch_to`]
    /// (for initial tasks) and [`init`] (for regular tasks) methods.
    ///
    /// [`init`]: TaskContext::init
    /// [`switch_to`]: TaskContext::switch_to
    pub fn new() -> Self {
        Self::with_kernel_root(
            #[cfg(feature = "uspace")]
            crate::asm::kernel_task_page_table_root(),
        )
    }

    fn with_kernel_root(#[cfg(feature = "uspace")] kernel_root: memory_addr::PhysAddr) -> Self {
        Self {
            kstack_top: va!(0),
            rsp: 0,
            fs_base: 0,
            user_cet: crate::asm::UserCetState::default(),
            #[cfg(feature = "uspace")]
            cr3: kernel_root,
            #[cfg(all(feature = "uspace", feature = "asid-fast-switch"))]
            cr3_pcid: 0,
            #[cfg(all(feature = "uspace", feature = "asid-fast-switch"))]
            cr3_generation: 0,
            #[cfg(all(feature = "uspace", feature = "asid-fast-switch"))]
            cr3_fallback_reason: crate::AddressSpaceFallbackReason::AsidZero,
            #[cfg(feature = "fp-simd")]
            ext_state: ExtendedState::default(),
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

    /// Copies a fully saved xstate image into an unpublished child context.
    #[cfg(feature = "fp-simd")]
    pub fn inherit_extended_state_from(&mut self, parent: &Self) {
        self.ext_state = parent.ext_state.clone();
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

#[cfg(all(test, not(target_os = "none")))]
mod initialization_tests {
    use super::TaskContext;

    #[cfg(feature = "uspace")]
    #[test]
    fn kernel_worker_context_keeps_boot_root_across_user_mm_retirement() {
        let kernel_root = memory_addr::PhysAddr::from_usize(0x1000);
        let user_root = memory_addr::PhysAddr::from_usize(0x9000);
        // Model a syscall running with a user CR3 and spawning deferred work.
        // The user context owns that identity; kernel construction consumes
        // only the separately published boot identity.
        let mut user = Box::new(TaskContext::with_kernel_root(kernel_root));
        user.set_page_table_root(user_root);
        let mut storage = Box::<TaskContext>::new_uninit();
        let worker = unsafe {
            TaskContext::initialize_at_with_kernel_root(storage.as_mut_ptr(), kernel_root);
            storage.assume_init()
        };
        assert_eq!(user.cr3, user_root);
        drop(user); // deferred work can outlive the originating address space
        assert_eq!(worker.cr3, kernel_root);
        assert_eq!(TaskContext::with_kernel_root(kernel_root).cr3, kernel_root);
        #[cfg(feature = "asid-fast-switch")]
        {
            assert_eq!(worker.cr3_pcid, 0);
            assert_eq!(worker.cr3_generation, 0);
            assert_eq!(
                worker.cr3_fallback_reason,
                crate::AddressSpaceFallbackReason::AsidZero
            );
        }
    }

    #[test]
    fn initializes_poisoned_heap_context_on_a_minimum_kernel_stack() {
        std::thread::Builder::new()
            .stack_size(16 * 1024)
            .spawn(|| {
                let mut storage = Box::<TaskContext>::new_uninit();
                let out = storage.as_mut_ptr();
                // SAFETY: the uninitialized box owns enough aligned storage.
                // Poison only bytes, then initialize all fields before use.
                let context = unsafe {
                    out.cast::<u8>()
                        .write_bytes(0xa5, core::mem::size_of::<TaskContext>());
                    TaskContext::initialize_at(out);
                    storage.assume_init()
                };
                assert_eq!(context.kstack_top.as_usize(), 0);
                assert_eq!(context.rsp, 0);
                assert_eq!(context.fs_base, 0);
                assert_eq!(context.user_cet, crate::asm::UserCetState::default());
                #[cfg(feature = "uspace")]
                assert_eq!(context.cr3, crate::asm::kernel_task_page_table_root());
                #[cfg(all(feature = "uspace", feature = "asid-fast-switch"))]
                {
                    assert_eq!(context.cr3_pcid, 0);
                    assert_eq!(context.cr3_generation, 0);
                    assert_eq!(
                        context.cr3_fallback_reason,
                        crate::AddressSpaceFallbackReason::AsidZero
                    );
                }
                #[cfg(feature = "fp-simd")]
                {
                    assert_eq!(core::ptr::addr_of!(context.ext_state) as usize % 64, 0);
                    for (offset, &byte) in context.ext_state.bytes.iter().enumerate() {
                        let expected = match offset {
                            0 => 0x7f,
                            1 => 0x03,
                            24 => 0x80,
                            25 => 0x1f,
                            _ => 0,
                        };
                        assert_eq!(byte, expected, "XSAVE byte {offset}");
                    }
                }
            })
            .unwrap()
            .join()
            .unwrap();
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
