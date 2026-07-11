use axcpu::{GeneralRegisters, uspace::UserContext};

use crate::{SignalSet, SignalStack, arch::SignalContextError};

core::arch::global_asm!(
    "
.section .text
.balign 4096
.global signal_trampoline
signal_trampoline:
    li.w    $a7, 139
    syscall 0

.fill 4096 - (. - signal_trampoline), 1, 0
"
);

#[repr(C, align(16))]
#[derive(Clone)]
pub struct MContext {
    sc_pc: u64,
    sc_regs: GeneralRegisters,
    sc_flags: u32,
}

impl MContext {
    pub fn new(uctx: &UserContext) -> Self {
        Self {
            sc_pc: uctx.era as _,
            sc_regs: uctx.regs,
            sc_flags: 0,
        }
    }

    pub(crate) fn prepare_restore(
        &self,
        current: &UserContext,
    ) -> Result<UserContext, SignalContextError> {
        let mut restored = *current;
        restored.era = self.sc_pc as _;
        restored.regs = self.sc_regs;
        // PRMD privilege/interrupt state remains the value installed by the
        // kernel for the current user return.
        Ok(restored)
    }

    /// Replaces the saved instruction pointer.
    pub fn set_program_counter(&mut self, pc: usize) {
        self.sc_pc = pc as u64;
    }

    /// Replaces the saved stack pointer.
    pub fn set_stack_pointer(&mut self, sp: usize) {
        self.sc_regs.sp = sp;
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
