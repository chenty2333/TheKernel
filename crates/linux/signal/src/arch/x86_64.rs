use crate::{SignalSet, SignalStack, arch::SignalContextError};
use alloc::vec::Vec;

/// Failure while constructing an owned dynamic signal XSAVE image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XsaveStateError {
    /// No heap storage was available for the dynamic signal image.
    Allocation,
    /// The architecture-owned image cannot be encoded by the Linux ABI.
    InvalidExtent,
}

/// Architecture-neutral ownership of the x86_64 register state that the
/// Linux signal ABI serializes into an `rt_sigframe`.
///
/// This is intentionally a data contract, not an execution context.  An
/// embedding kernel converts its trap frame at the signal boundary and keeps
/// trap-only state, TLS bases, and return-to-userspace mechanics private.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UserContext {
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
    pub vector: u64,
    pub error_code: u64,
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

impl UserContext {
    /// Creates a conventional x86_64 userspace context for ABI consumers and
    /// tests. Embedding kernels should convert their native context instead.
    pub fn new(entry: usize, stack_pointer: usize, arg0: usize) -> Self {
        Self {
            rdi: arg0 as u64,
            rip: entry as u64,
            // x86_64 long-mode user code/data selectors used by the native
            // Linux ABI. Signal restore additionally requires them to match
            // the embedding context, so these are never trusted blindly.
            cs: 0x23,
            rflags: 1 << 9,
            rsp: stack_pointer as u64,
            ss: 0x1b,
            ..Self::default()
        }
    }

    pub const fn arg0(&self) -> usize {
        self.rdi as usize
    }
    pub const fn set_arg0(&mut self, value: usize) {
        self.rdi = value as u64;
    }
    pub const fn arg1(&self) -> usize {
        self.rsi as usize
    }
    pub const fn set_arg1(&mut self, value: usize) {
        self.rsi = value as u64;
    }
    pub const fn arg2(&self) -> usize {
        self.rdx as usize
    }
    pub const fn set_arg2(&mut self, value: usize) {
        self.rdx = value as u64;
    }
    pub const fn ip(&self) -> usize {
        self.rip as usize
    }
    pub const fn set_ip(&mut self, value: usize) {
        self.rip = value as u64;
    }
    pub const fn sp(&self) -> usize {
        self.rsp as usize
    }
    pub const fn set_sp(&mut self, value: usize) {
        self.rsp = value as u64;
    }
    pub const fn retval(&self) -> usize {
        self.rax as usize
    }
    pub const fn set_retval(&mut self, value: usize) {
        self.rax = value as u64;
    }
}

/// Linux x86_64 signal FP payload for the exact XCR0 policy selected by the
/// embedding kernel.
///
/// XSAVE machines use Linux's extended FP state encoding: a standard XSAVE
/// image followed by `FP_XSTATE_MAGIC2`, with `FP_XSTATE_MAGIC1` metadata in
/// the legacy area's software-reserved bytes.  CPUs without XSAVE instead
/// expose the architectural 512-byte FXSAVE image directly.  That legacy ABI
/// has no XSAVE header or magic trailer; inventing one would make a signal
/// frame whose advertised extent disagrees with the FXRSTOR task layout.
#[derive(Clone, PartialEq, Eq)]
pub struct XsaveState64 {
    bytes: Vec<u8>,
    xfeatures: u64,
}

impl XsaveState64 {
    pub const LEGACY_SIZE: usize = 512;
    pub const HEADER_SIZE: usize = 64;
    pub const HEADER_OFFSET: usize = Self::LEGACY_SIZE;
    pub const TRAILER_SIZE: usize = 4;
    pub const MAGIC1: u32 = 0x4650_5853;
    pub const MAGIC2: u32 = 0x4650_5845;
    const SW_RESERVED_OFFSET: usize = 464;
    const SW_RESERVED_SIZE: usize = 48;
    const SW_RESERVED_METADATA_SIZE: usize = 20;

    /// Wrap an architecture-owned XSAVE or legacy FXSAVE image in the Linux
    /// signal ABI representation.
    pub fn try_from_xsave_prefix(prefix: &[u8], xfeatures: u64) -> Result<Self, XsaveStateError> {
        if prefix.len() > u32::MAX as usize {
            return Err(XsaveStateError::InvalidExtent);
        }
        if prefix.len() == Self::LEGACY_SIZE {
            if xfeatures != 0 {
                return Err(XsaveStateError::InvalidExtent);
            }
            let mut bytes = Vec::new();
            bytes
                .try_reserve_exact(Self::LEGACY_SIZE)
                .map_err(|_| XsaveStateError::Allocation)?;
            bytes.extend_from_slice(prefix);
            // FXSAVE leaves this software-reserved tail unspecified.  Linux's
            // non-XSAVE signal ABI publishes it as an ordinary legacy frame,
            // not as a malformed partial FP_XSTATE extension.
            bytes[Self::SW_RESERVED_OFFSET..Self::LEGACY_SIZE].fill(0);
            return Ok(Self { bytes, xfeatures });
        }
        if xfeatures == 0 || prefix.len() < Self::LEGACY_SIZE + Self::HEADER_SIZE {
            return Err(XsaveStateError::InvalidExtent);
        }
        let total = prefix
            .len()
            .checked_add(Self::TRAILER_SIZE)
            .ok_or(XsaveStateError::InvalidExtent)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(total)
            .map_err(|_| XsaveStateError::Allocation)?;
        bytes.extend_from_slice(prefix);
        bytes.extend_from_slice(&Self::MAGIC2.to_le_bytes());
        // FXSAVE/XSave do not define these Linux software-reserved bytes.
        // Never publish snapshot residue to userspace, and leave a fully
        // zeroed extension tail that sigreturn can validate before restore.
        bytes[Self::SW_RESERVED_OFFSET..Self::SW_RESERVED_OFFSET + Self::SW_RESERVED_SIZE].fill(0);
        let xsave_size = bytes.len() as u32;
        bytes[Self::SW_RESERVED_OFFSET..Self::SW_RESERVED_OFFSET + 4]
            .copy_from_slice(&Self::MAGIC1.to_le_bytes());
        bytes[Self::SW_RESERVED_OFFSET + 4..Self::SW_RESERVED_OFFSET + 8]
            .copy_from_slice(&xsave_size.to_le_bytes());
        bytes[Self::SW_RESERVED_OFFSET + 8..Self::SW_RESERVED_OFFSET + 16]
            .copy_from_slice(&xfeatures.to_le_bytes());
        bytes[Self::SW_RESERVED_OFFSET + 16..Self::SW_RESERVED_OFFSET + 20]
            .copy_from_slice(&(prefix.len() as u32).to_le_bytes());
        Ok(Self { bytes, xfeatures })
    }
    /// Takes an owned user frame after its extent was bounded by the current
    /// kernel layout; metadata remains unmodified for validation.
    pub fn from_signal_bytes(bytes: Vec<u8>, xfeatures: u64) -> Option<Self> {
        match bytes.len() {
            Self::LEGACY_SIZE if xfeatures == 0 => Some(Self { bytes, xfeatures }),
            len if xfeatures != 0
                && len >= Self::LEGACY_SIZE + Self::HEADER_SIZE + Self::TRAILER_SIZE =>
            {
                Some(Self { bytes, xfeatures })
            }
            _ => None,
        }
    }
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn xsave_prefix(&self) -> &[u8] {
        if self.xfeatures == 0 {
            &self.bytes
        } else {
            &self.bytes[..self.bytes.len() - Self::TRAILER_SIZE]
        }
    }
    pub const fn xfeatures(&self) -> u64 {
        self.xfeatures
    }
    pub fn validate(&self, xfeatures: u64, xstate_size: usize) -> bool {
        if xfeatures == 0 && xstate_size == Self::LEGACY_SIZE {
            return self.xfeatures == 0
                && self.bytes.len() == Self::LEGACY_SIZE
                && self.xsave_prefix().len() == Self::LEGACY_SIZE;
        }
        self.xfeatures == xfeatures
            && xfeatures != 0
            && self.xsave_prefix().len() == xstate_size
            && self.bytes.len() == xstate_size + Self::TRAILER_SIZE
            && u32::from_le_bytes(
                self.bytes[Self::SW_RESERVED_OFFSET..Self::SW_RESERVED_OFFSET + 4]
                    .try_into()
                    .unwrap(),
            ) == Self::MAGIC1
            && u32::from_le_bytes(
                self.bytes[Self::SW_RESERVED_OFFSET + 4..Self::SW_RESERVED_OFFSET + 8]
                    .try_into()
                    .unwrap(),
            ) as usize
                == self.bytes.len()
            && u64::from_le_bytes(
                self.bytes[Self::SW_RESERVED_OFFSET + 8..Self::SW_RESERVED_OFFSET + 16]
                    .try_into()
                    .unwrap(),
            ) == xfeatures
            && u32::from_le_bytes(
                self.bytes[Self::SW_RESERVED_OFFSET + 16..Self::SW_RESERVED_OFFSET + 20]
                    .try_into()
                    .unwrap(),
            ) as usize
                == xstate_size
            && self.bytes[Self::SW_RESERVED_OFFSET + Self::SW_RESERVED_METADATA_SIZE
                ..Self::SW_RESERVED_OFFSET + Self::SW_RESERVED_SIZE]
                .iter()
                .all(|byte| *byte == 0)
            && u32::from_le_bytes(self.bytes[xstate_size..].try_into().unwrap()) == Self::MAGIC2
            && u64::from_le_bytes(
                self.bytes[Self::HEADER_OFFSET..Self::HEADER_OFFSET + 8]
                    .try_into()
                    .unwrap(),
            ) & !xfeatures
                == 0
            && u64::from_le_bytes(
                self.bytes[Self::HEADER_OFFSET + 8..Self::HEADER_OFFSET + 16]
                    .try_into()
                    .unwrap(),
            ) == 0
            && self.bytes[Self::HEADER_OFFSET + 16..Self::HEADER_OFFSET + Self::HEADER_SIZE]
                .iter()
                .all(|byte| *byte == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::XsaveState64;

    #[test]
    fn legacy_fxsave_signal_frame_has_no_xstate_trailer() {
        let image = XsaveState64::try_from_xsave_prefix(&[0xa5; XsaveState64::LEGACY_SIZE], 0)
            .expect("the no-XSAVE ABI accepts a legacy FXSAVE image");
        assert_eq!(image.as_bytes().len(), XsaveState64::LEGACY_SIZE);
        assert_eq!(image.xsave_prefix().len(), XsaveState64::LEGACY_SIZE);
        assert!(image.validate(0, XsaveState64::LEGACY_SIZE));
        assert!(
            XsaveState64::from_signal_bytes(image.as_bytes().to_vec(), 0)
                .expect("legacy FXSAVE frame remains accepted for sigreturn")
                .validate(0, XsaveState64::LEGACY_SIZE)
        );
    }

    #[test]
    fn standard_xsave_requires_an_enabled_xfeature() {
        assert!(
            XsaveState64::try_from_xsave_prefix(
                &[0; XsaveState64::LEGACY_SIZE + XsaveState64::HEADER_SIZE],
                0,
            )
            .is_err()
        );
        let image = XsaveState64::try_from_xsave_prefix(
            &[0; XsaveState64::LEGACY_SIZE + XsaveState64::HEADER_SIZE],
            0b11,
        )
        .expect("x87/SSE XSAVE signal frame is valid");
        assert!(image.validate(0b11, XsaveState64::LEGACY_SIZE + XsaveState64::HEADER_SIZE,));
        assert!(
            image.as_bytes()[XsaveState64::SW_RESERVED_OFFSET
                ..XsaveState64::SW_RESERVED_OFFSET + XsaveState64::SW_RESERVED_SIZE]
                .iter()
                .skip(XsaveState64::SW_RESERVED_METADATA_SIZE)
                .all(|byte| *byte == 0)
        );

        let mut tampered = image.as_bytes().to_vec();
        tampered[XsaveState64::SW_RESERVED_OFFSET + XsaveState64::SW_RESERVED_METADATA_SIZE] = 1;
        let tampered = XsaveState64::from_signal_bytes(tampered, 0b11)
            .expect("the fixed-size frame reaches sigreturn validation");
        assert!(!tampered.validate(0b11, XsaveState64::LEGACY_SIZE + XsaveState64::HEADER_SIZE,));
    }
}

core::arch::global_asm!(
    "
.section .text
.code64
.balign 4096
.global thekernel_linux_signal_trampoline
thekernel_linux_signal_trampoline:
    mov rax, 0xf
    syscall

.fill 4096 - (. - thekernel_linux_signal_trampoline), 1, 0
"
);

#[repr(C)]
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
    ss: u16,
    err: usize,
    trapno: usize,
    oldmask: usize,
    cr2: usize,
    fpstate: usize,
    // Linux reserves these words for architecture extensions.  They are part
    // of the userspace ABI and must remain zero unless Linux assigns a public
    // extension to them; kernel-private CET state never belongs here.
    reserved1: [usize; 8],
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
            ss: uctx.ss as _,
            err: uctx.error_code as _,
            trapno: uctx.vector as _,
            oldmask: 0,
            cr2: 0,
            fpstate: 0,
            reserved1: [0; 8],
        }
    }

    /// Builds the context fields that are published in a signal delivery.
    /// `oldmask` is retained for the legacy sigcontext ABI while `fpstate`
    /// points at the separately published FXSAVE payload.
    pub(crate) fn for_delivery(uctx: &UserContext, sigmask: SignalSet, fpstate: usize) -> Self {
        let mut context = Self::new(uctx);
        context.oldmask = sigmask.bits() as _;
        context.fpstate = fpstate;
        context
    }

    pub(crate) fn prepare_restore(
        &self,
        current: &UserContext,
    ) -> Result<UserContext, SignalContextError> {
        // TheKernel currently supports only the native 64-bit userspace ABI.
        // Never copy a kernel or compatibility selector out of a user frame.
        if self.cs & 0b11 != 0b11 || self.cs as u64 != current.cs || self.ss as u64 != current.ss {
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

    /// Returns the saved user stack-segment selector.
    pub const fn stack_segment(&self) -> u16 {
        self.ss
    }

    /// Returns the legacy sigcontext blocked-mask field.
    pub const fn old_mask(&self) -> usize {
        self.oldmask
    }

    /// Returns the userspace address of the legacy FXSAVE image.
    pub const fn fpstate(&self) -> usize {
        self.fpstate
    }

    /// Replaces the userspace pointer to the legacy FXSAVE image.
    pub fn set_fpstate(&mut self, address: usize) {
        self.fpstate = address;
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
    pub fn new(uctx: &UserContext, sigmask: SignalSet, stack: SignalStack) -> Self {
        Self {
            flags: UC_SIGCONTEXT_SS | UC_STRICT_RESTORE_SS,
            link: 0,
            stack,
            mcontext: MContext::for_delivery(uctx, sigmask, 0),
            sigmask,
        }
    }

    /// Builds a context with its separately published legacy FXSAVE address.
    pub(crate) fn with_fpstate(
        uctx: &UserContext,
        sigmask: SignalSet,
        stack: SignalStack,
        fpstate: usize,
    ) -> Self {
        Self {
            flags: UC_SIGCONTEXT_SS | UC_STRICT_RESTORE_SS,
            link: 0,
            stack,
            mcontext: MContext::for_delivery(uctx, sigmask, fpstate),
            sigmask,
        }
    }
}

/// `ucontext_t.uc_flags` bit indicating that sigcontext contains `ss`.
pub const UC_SIGCONTEXT_SS: usize = 2;
/// `ucontext_t.uc_flags` bit requesting strict user `ss` restoration.
pub const UC_STRICT_RESTORE_SS: usize = 4;
/// The x86_64 Linux ABI does not advertise this flag for the legacy image.
pub const UC_FP_XSTATE: usize = 1;

const _: [(); 8] = [(); core::mem::align_of::<MContext>()];
const _: [(); 256] = [(); size_of::<MContext>()];
const _: [(); 150] = [(); core::mem::offset_of!(MContext, ss)];
const _: [(); 168] = [(); core::mem::offset_of!(MContext, oldmask)];
const _: [(); 184] = [(); core::mem::offset_of!(MContext, fpstate)];
const _: [(); 40] = [(); core::mem::offset_of!(UContext, mcontext)];
const _: [(); 296] = [(); core::mem::offset_of!(UContext, sigmask)];
const _: [(); 304] = [(); size_of::<UContext>()];
