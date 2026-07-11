use axcpu::uspace::UserContext;

use crate::{SignalSet, SignalStack, arch::SignalContextError};

core::arch::global_asm!(
    "
.section .text
.code64
.balign 4096
.global signal_trampoline
signal_trampoline:
    mov rax, 0xf
    syscall

.fill 4096 - (. - signal_trampoline), 1, 0
"
);

#[repr(C, align(16))]
#[derive(Clone)]
pub struct MContext {
    r8: usize,
    r9: usize,
    r10: usize,
    r11: usize,
    r12: usize,
    r13: usize,
    r14: usize,
    r15: usize,
    rdi: usize,
    rsi: usize,
    rbp: usize,
    rbx: usize,
    rdx: usize,
    rax: usize,
    rcx: usize,
    rsp: usize,
    rip: usize,
    eflags: usize,
    cs: u16,
    gs: u16,
    fs: u16,
    _pad: u16,
    err: usize,
    trapno: usize,
    oldmask: usize,
    cr2: usize,
    fpstate: usize,
    _reserved1: [usize; 8],
}

impl MContext {
    pub fn new(uctx: &UserContext) -> Self {
        Self {
            r8: uctx.r8 as _,
            r9: uctx.r9 as _,
            r10: uctx.r10 as _,
            r11: uctx.r11 as _,
            r12: uctx.r12 as _,
            r13: uctx.r13 as _,
            r14: uctx.r14 as _,
            r15: uctx.r15 as _,
            rdi: uctx.rdi as _,
            rsi: uctx.rsi as _,
            rbp: uctx.rbp as _,
            rbx: uctx.rbx as _,
            rdx: uctx.rdx as _,
            rax: uctx.rax as _,
            rcx: uctx.rcx as _,
            rsp: uctx.rsp as _,
            rip: uctx.rip as _,
            eflags: uctx.rflags as _,
            cs: uctx.cs as _,
            gs: 0,
            fs: 0,
            _pad: 0,
            err: uctx.error_code as _,
            trapno: uctx.vector as _,
            oldmask: 0,
            cr2: 0,
            fpstate: 0,
            _reserved1: [0; 8],
        }
    }

    pub(crate) fn prepare_restore(
        &self,
        current: &UserContext,
    ) -> Result<UserContext, SignalContextError> {
        // TheKernel currently supports only the native 64-bit userspace ABI.
        // Never copy a kernel or compatibility selector out of a user frame.
        if self.cs & 0b11 != 0b11 || self.cs as u64 != current.cs {
            return Err(SignalContextError::InvalidProcessorState);
        }

        let mut restored = *current;
        restored.r8 = self.r8 as _;
        restored.r9 = self.r9 as _;
        restored.r10 = self.r10 as _;
        restored.r11 = self.r11 as _;
        restored.r12 = self.r12 as _;
        restored.r13 = self.r13 as _;
        restored.r14 = self.r14 as _;
        restored.r15 = self.r15 as _;
        restored.rdi = self.rdi as _;
        restored.rsi = self.rsi as _;
        restored.rbp = self.rbp as _;
        restored.rbx = self.rbx as _;
        restored.rdx = self.rdx as _;
        restored.rax = self.rax as _;
        restored.rcx = self.rcx as _;
        restored.rsp = self.rsp as _;
        restored.rip = self.rip as _;

        // Match Linux's FIX_EFLAGS model: condition/debug/alignment state is
        // user-restorable, while IOPL, IF and reserved bits remain trusted.
        const USER_RFLAGS_MASK: u64 = (1 << 0) // CF
            | (1 << 2) // PF
            | (1 << 4) // AF
            | (1 << 6) // ZF
            | (1 << 7) // SF
            | (1 << 8) // TF
            | (1 << 10) // DF
            | (1 << 11) // OF
            | (1 << 16) // RF
            | (1 << 18); // AC
        restored.rflags =
            (current.rflags & !USER_RFLAGS_MASK) | (self.eflags as u64 & USER_RFLAGS_MASK);

        // cs/ss, trap vector, error code and TLS bases are kernel-owned and
        // intentionally preserved from `current`.
        Ok(restored)
    }

    /// Replaces the saved instruction pointer.
    pub fn set_program_counter(&mut self, pc: usize) {
        self.rip = pc;
    }

    /// Replaces the saved stack pointer.
    pub fn set_stack_pointer(&mut self, sp: usize) {
        self.rsp = sp;
    }

    /// Replaces the saved RFLAGS value.
    pub fn set_processor_flags(&mut self, flags: usize) {
        self.eflags = flags;
    }

    /// Returns the saved RFLAGS value.
    pub fn processor_flags(&self) -> usize {
        self.eflags
    }

    /// Replaces the saved code segment selector.
    pub fn set_code_segment(&mut self, cs: u16) {
        self.cs = cs;
    }
}

#[repr(C)]
#[derive(Clone)]
pub struct UContext {
    pub flags: usize,
    pub link: usize,
    pub stack: SignalStack,
    pub mcontext: MContext,
    pub sigmask: SignalSet,
}

impl UContext {
    pub fn new(uctx: &UserContext, sigmask: SignalSet) -> Self {
        Self {
            flags: 0,
            link: 0,
            stack: SignalStack::default(),
            mcontext: MContext::new(uctx),
            sigmask,
        }
    }
}
