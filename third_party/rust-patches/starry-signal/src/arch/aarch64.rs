use axcpu::uspace::UserContext;

use crate::{SignalSet, SignalStack, arch::SignalContextError};

core::arch::global_asm!(
    "
.section .text
.balign 4096
.global signal_trampoline
signal_trampoline:
    mov x8, #139
    svc #0

.fill 4096 - (. - signal_trampoline), 1, 0
"
);

#[repr(C, align(16))]
#[derive(Clone)]
struct MContextPadding([u8; 4096]);

#[repr(C)]
#[derive(Clone)]
pub struct MContext {
    fault_address: u64,
    regs: [u64; 31],
    sp: u64,
    pc: u64,
    pstate: u64,
    __reserved: MContextPadding,
}

impl MContext {
    pub fn new(uctx: &UserContext) -> Self {
        Self {
            fault_address: 0,
            regs: uctx.x,
            sp: uctx.sp,
            pc: uctx.elr,
            pstate: uctx.spsr,
            __reserved: MContextPadding([0; 4096]),
        }
    }

    pub(crate) fn prepare_restore(
        &self,
        current: &UserContext,
    ) -> Result<UserContext, SignalContextError> {
        // The native ABI returns through EL0t. Apart from NZCV, privileged
        // PSTATE state must exactly match the kernel-created return context.
        const USER_PSTATE_MASK: u64 = 0xf000_0000;
        if self.pstate & !USER_PSTATE_MASK != current.spsr & !USER_PSTATE_MASK {
            return Err(SignalContextError::InvalidProcessorState);
        }

        let mut restored = *current;
        restored.x = self.regs;
        restored.sp = self.sp;
        restored.elr = self.pc;
        restored.spsr = (current.spsr & !USER_PSTATE_MASK) | (self.pstate & USER_PSTATE_MASK);
        Ok(restored)
    }

    /// Replaces the saved instruction pointer.
    pub fn set_program_counter(&mut self, pc: usize) {
        self.pc = pc as u64;
    }

    /// Replaces the saved stack pointer.
    pub fn set_stack_pointer(&mut self, sp: usize) {
        self.sp = sp as u64;
    }

    /// Replaces the saved PSTATE value.
    pub fn set_processor_state(&mut self, pstate: u64) {
        self.pstate = pstate;
    }
}

#[repr(C)]
#[derive(Clone)]
pub struct UContext {
    pub flags: usize,
    pub link: usize,
    pub stack: SignalStack,
    pub sigmask: SignalSet,
    __unused: [u8; 1024 / 8 - size_of::<SignalSet>()],
    pub mcontext: MContext,
}

impl UContext {
    pub fn new(uctx: &UserContext, sigmask: SignalSet) -> Self {
        Self {
            flags: 0,
            link: 0,
            stack: SignalStack::default(),
            sigmask,
            __unused: [0; 1024 / 8 - size_of::<SignalSet>()],
            mcontext: MContext::new(uctx),
        }
    }
}
