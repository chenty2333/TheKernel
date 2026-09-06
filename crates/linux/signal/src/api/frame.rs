//! x86_64 signal-frame data plane.
//!
//! This module deliberately has no signal-manager state.  It owns the Linux
//! visible frame layout, its userspace copy boundaries, and the transactional
//! preparation of a signal return.  A manager supplies its current context,
//! alternate-stack snapshot, and validation policy, then commits the returned
//! token in its own state domain.

use alloc::vec::Vec;
use core::{
    mem::MaybeUninit,
    mem::{self, offset_of},
};

use thekernel_linux_usercopy::{
    UserCopyError, UserMemory, UserMemoryContext, VmMutPtr, VmPtr, VmResult,
};

use crate::{
    SignalInfo, SignalSet, SignalStack, SignalStackRestoreError, Signo,
    arch::{SignalContextError, UContext, UserContext, XsaveState64},
};

#[derive(Clone)]
pub struct SignalFpState(XsaveState64);

impl SignalFpState {
    fn size(&self) -> usize {
        self.0.as_bytes().len()
    }

    fn alignment(&self) -> usize {
        64
    }
}

impl From<XsaveState64> for SignalFpState {
    fn from(image: XsaveState64) -> Self {
        Self(image)
    }
}

/// x86_64's red zone, which an asynchronous signal frame must not overwrite.
pub const SIGNAL_RED_ZONE: usize = 128;

/// Alignment required by the x86_64 Linux signal-frame data object.
pub const SIGNAL_FRAME_ALIGNMENT: usize = 16;

/// Size of the fixed Linux-visible signal frame, excluding the dynamic XSAVE
/// payload and the restorer word.
pub const SIGNAL_FIXED_FRAME_SIZE: usize = mem::size_of::<SignalFrame>();

/// Why a signal frame could not be placed on the selected stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalFrameLayoutError {
    /// A stack arithmetic operation wrapped the userspace address space.
    AddressOverflow,
    /// The selected alternate stack cannot contain the complete frame and
    /// restorer word (or the nested delivery's preserved red zone).
    OutsideAlternateStack,
    /// The owned dynamic XSAVE image could not be allocated before any frame
    /// data or handler context was published.
    Allocation,
    /// The embedding kernel could not snapshot the selected CPU XSAVE state.
    XsaveUnavailable,
}

/// Dynamic full-XSAVE restore error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XsaveRestoreError {
    Misaligned,
    Copyin(UserCopyError),
    InvalidMetadata,
    Allocation,
}

/// Copies and validates exactly the current kernel-selected standard XSAVE
/// signal frame. The initial 512-byte legacy prefix supplies the dynamic size
/// field; the complete payload is then copied once into owned memory.
pub fn copyin_xsave_plan_for_rt_sigreturn<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    fpstate: usize,
    xfeatures: u64,
    xstate_size: usize,
) -> Result<Option<XsaveState64>, XsaveRestoreError> {
    if fpstate == 0 {
        return Ok(None);
    }
    if fpstate & 63 != 0 {
        return Err(XsaveRestoreError::Misaligned);
    }
    // A no-XSAVE CPU's Linux signal frame is the 512-byte FXSAVE payload
    // itself.  Only the extended XSAVE ABI has FP_XSTATE_MAGIC2 after the
    // architecture-owned image.
    let total = if xfeatures == 0 && xstate_size == XsaveState64::LEGACY_SIZE {
        xstate_size
    } else {
        xstate_size
            .checked_add(XsaveState64::TRAILER_SIZE)
            .ok_or(XsaveRestoreError::InvalidMetadata)?
    };
    let mut raw: Vec<MaybeUninit<u8>> = Vec::new();
    raw.try_reserve_exact(total)
        .map_err(|_| XsaveRestoreError::Allocation)?;
    // SAFETY: MaybeUninit permits an uninitialized length. read_bytes below
    // initializes every byte before the allocation is reinterpreted as u8.
    unsafe { raw.set_len(total) };
    memory
        .read_bytes(fpstate, &mut raw)
        .map_err(XsaveRestoreError::Copyin)?;
    let mut raw = core::mem::ManuallyDrop::new(raw);
    // SAFETY: `MaybeUninit<u8>` has the same layout as u8 and read_bytes
    // initialized the exact len above. Transfer the allocation, no copy.
    let raw = unsafe { Vec::from_raw_parts(raw.as_mut_ptr().cast::<u8>(), total, raw.capacity()) };
    let image = XsaveState64::from_signal_bytes(raw, xfeatures)
        .ok_or(XsaveRestoreError::InvalidMetadata)?;
    if !image.validate(xfeatures, xstate_size) {
        return Err(XsaveRestoreError::InvalidMetadata);
    }
    Ok(Some(image))
}

/// Which stack origin is used for one signal delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalFrameStack {
    /// Deliver on the interrupted ordinary userspace stack, preserving its
    /// 128-byte red zone.
    Normal,
    /// Deliver on an alternate stack for the first time.  The frame starts
    /// from the exclusive top and does not reserve a red zone below that top.
    FreshAltStack,
    /// Deliver while already executing on the alternate stack, preserving the
    /// interrupted handler's 128-byte red zone.
    NestedAltStack,
}

/// The checked addresses occupied by a signal frame and its entry word.
///
/// The published user stack pointer points at the restorer word.  The frame
/// itself starts eight bytes above it, so the x86_64 handler entry invariant is
/// `rsp % 16 == 8` while `frame_start % 16 == 0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignalFrameLayout {
    frame_start: usize,
    published_sp: usize,
    siginfo: usize,
    ucontext: usize,
    fpstate: usize,
    fpstate_size: usize,
    stack: SignalFrameStack,
}

impl SignalFrameLayout {
    /// Computes a placement for an explicitly selected Linux fpstate extent.
    pub fn with_fpstate(
        interrupted_sp: usize,
        configured_stack: &SignalStack,
        stack: SignalFrameStack,
        fpstate_size: usize,
        fpstate_alignment: usize,
    ) -> Result<Self, SignalFrameLayoutError> {
        if !fpstate_alignment.is_power_of_two() || fpstate_alignment < SIGNAL_FRAME_ALIGNMENT {
            return Err(SignalFrameLayoutError::AddressOverflow);
        }
        let origin = match stack {
            SignalFrameStack::Normal | SignalFrameStack::NestedAltStack => interrupted_sp
                .checked_sub(SIGNAL_RED_ZONE)
                .ok_or(SignalFrameLayoutError::AddressOverflow)?,
            SignalFrameStack::FreshAltStack => configured_stack
                .checked_top()
                .ok_or(SignalFrameLayoutError::AddressOverflow)?,
        };

        // Reserve up to alignment - 1 bytes between the fixed frame and
        // fpstate. Linux requires an XSAVE buffer aligned to 64 bytes.
        let unaligned_start = origin
            .checked_sub(fpstate_size)
            .and_then(|address| address.checked_sub(fpstate_alignment - 1))
            .and_then(|address| address.checked_sub(SIGNAL_FIXED_FRAME_SIZE))
            .ok_or(SignalFrameLayoutError::AddressOverflow)?;
        let frame_start = unaligned_start & !(SIGNAL_FRAME_ALIGNMENT - 1);
        let fixed_end = frame_start
            .checked_add(SIGNAL_FIXED_FRAME_SIZE)
            .ok_or(SignalFrameLayoutError::AddressOverflow)?;
        let fpstate = fixed_end
            .checked_add(fpstate_alignment - 1)
            .map(|address| address & !(fpstate_alignment - 1))
            .ok_or(SignalFrameLayoutError::AddressOverflow)?;
        let published_sp = frame_start
            .checked_sub(mem::size_of::<usize>())
            .ok_or(SignalFrameLayoutError::AddressOverflow)?;
        let siginfo = frame_start
            .checked_add(offset_of!(SignalFrame, siginfo))
            .ok_or(SignalFrameLayoutError::AddressOverflow)?;
        let ucontext = frame_start
            .checked_add(offset_of!(SignalFrame, ucontext))
            .ok_or(SignalFrameLayoutError::AddressOverflow)?;

        // The restorer word, frame, and (for nested delivery) preserved red
        // zone must all remain in the configured alternate stack.  Fresh
        // delivery starts at the top, so the range ends at that top.  Nested
        // delivery ends at the interrupted stack pointer, covering the red
        // zone between the frame and the interrupted handler.
        if matches!(
            stack,
            SignalFrameStack::FreshAltStack | SignalFrameStack::NestedAltStack
        ) {
            let end = match stack {
                SignalFrameStack::FreshAltStack => configured_stack
                    .checked_top()
                    .ok_or(SignalFrameLayoutError::AddressOverflow)?,
                SignalFrameStack::NestedAltStack => interrupted_sp,
                SignalFrameStack::Normal => unreachable!(),
            };
            let span = end
                .checked_sub(published_sp)
                .ok_or(SignalFrameLayoutError::AddressOverflow)?;
            if !configured_stack.contains_range(published_sp, span) {
                return Err(SignalFrameLayoutError::OutsideAlternateStack);
            }
        }

        Ok(Self {
            frame_start,
            published_sp,
            siginfo,
            ucontext,
            fpstate,
            fpstate_size,
            stack,
        })
    }

    /// Returns the first byte of the ABI frame.
    pub const fn frame_start(&self) -> usize {
        self.frame_start
    }

    /// Returns the user stack pointer installed for handler entry.
    pub const fn published_sp(&self) -> usize {
        self.published_sp
    }

    /// Returns the user pointer passed as the handler's `siginfo` argument.
    pub const fn siginfo(&self) -> usize {
        self.siginfo
    }

    /// Returns the user pointer passed as the handler's `ucontext` argument.
    pub const fn ucontext(&self) -> usize {
        self.ucontext
    }

    /// Returns the first byte of the separately published XSAVE
    /// payload.
    pub const fn fpstate(&self) -> usize {
        self.fpstate
    }

    /// Alias for [`Self::fpstate`] emphasizing that this is a start address.
    pub const fn fpstate_start(&self) -> usize {
        self.fpstate
    }

    /// Returns the exclusive end of the fixed frame object.
    pub const fn fixed_frame_end(&self) -> usize {
        self.frame_start + SIGNAL_FIXED_FRAME_SIZE
    }

    /// Returns the exclusive end of the fixed frame and payload region.
    pub const fn payload_end(&self) -> usize {
        self.fpstate + self.fpstate_size
    }

    /// Returns the exclusive end of the XSAVE payload.
    pub const fn fpstate_end(&self) -> usize {
        self.payload_end()
    }

    /// Returns the stack origin used by this placement.
    pub const fn stack(&self) -> SignalFrameStack {
        self.stack
    }
}

/// The userspace ABI frame created for a signal handler.
///
/// This contains only Linux-visible signal state.  Kernel trap metadata is
/// not serialized into userspace and therefore cannot be forged by
/// `sigreturn`.  x86_64 enters the handler with the restorer word immediately
/// below this 16-byte-aligned object.
#[repr(C, align(16))]
#[derive(Clone)]
pub struct SignalFrame {
    ucontext: UContext,
    siginfo: SignalInfo,
}

const _: [(); SIGNAL_FRAME_ALIGNMENT] = [(); mem::align_of::<SignalFrame>()];
const _: [(); mem::size_of::<SignalFrame>()] =
    [(); offset_of!(SignalFrame, siginfo) + mem::size_of::<SignalInfo>()];

impl SignalFrame {
    pub(crate) fn new_with_fpstate(
        uctx: &UserContext,
        sigmask: SignalSet,
        stack: SignalStack,
        siginfo: SignalInfo,
        fpstate: usize,
    ) -> Self {
        Self {
            ucontext: UContext::with_fpstate(uctx, sigmask, stack, fpstate),
            siginfo,
        }
    }

    /// Returns the Linux-visible user context stored in this frame.
    pub fn ucontext(&self) -> &UContext {
        &self.ucontext
    }

    /// Returns a mutable Linux-visible user context, as a signal handler sees
    /// it.
    pub fn ucontext_mut(&mut self) -> &mut UContext {
        &mut self.ucontext
    }

    /// Copies a complete signal frame from userspace into an owned value.
    ///
    /// The userspace pointer is treated as an unaligned byte address.  The
    /// provider must initialize every byte on success; a faulting or partial
    /// read never yields an owned frame.
    pub fn read_from_user<M: UserMemory + ?Sized>(
        memory: &mut UserMemoryContext<'_, M>,
        ptr: *const Self,
    ) -> VmResult<Self> {
        let frame = ptr.vm_read_uninit(memory)?;
        // SAFETY: UserMemory returns `Ok` only after initializing every byte of
        // the destination.  SignalFrame and all nested ABI records contain
        // only initialized integer/byte storage; every ABI alignment hole is
        // represented by an explicit zeroed field.  Restoration validates the
        // machine fields before publication and never interprets siginfo.
        Ok(unsafe { frame.assume_init() })
    }

    /// Copies a complete frame to its userspace address.
    ///
    /// Construction of this type initializes all bytes, including the
    /// explicit ABI padding fields, so the unchecked object copy is bounded
    /// to the frame's exact representation.
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    pub fn write_to_user<M: UserMemory + ?Sized>(
        &self,
        memory: &mut UserMemoryContext<'_, M>,
        ptr: *mut Self,
    ) -> VmResult {
        // SAFETY: SignalFrame has no implicit outer padding and its nested
        // records initialize every ABI padding byte before construction.
        unsafe { ptr.vm_write_unchecked(memory, self.clone()) }
    }
}

/// A frame placement and its fully initialized contents, ready for one
/// userspace publication.
#[must_use = "publishing or dropping the prepared frame completes delivery"]
pub struct PreparedSignalFrame {
    layout: SignalFrameLayout,
    frame: SignalFrame,
    fpstate: SignalFpState,
    restorer: usize,
    handler: usize,
    signo: Signo,
    interrupted: UserContext,
}

impl PreparedSignalFrame {
    /// Returns the checked frame addresses before publication.
    pub const fn layout(&self) -> SignalFrameLayout {
        self.layout
    }

    /// Returns the fully initialized frame snapshot.
    pub fn frame(&self) -> &SignalFrame {
        &self.frame
    }

    /// Returns the owned complete XSAVE snapshot that will be published.
    pub fn fpstate(&self) -> &XsaveState64 {
        &self.fpstate.0
    }

    /// Copies the frame and restorer word to userspace exactly once.
    ///
    /// The returned published token owns the new machine context.  Callers
    /// must install it only after this method succeeds.  A copyout failure
    /// consumes the prepared token and leaves the caller's context untouched.
    pub fn publish<M: UserMemory + ?Sized>(
        self,
        memory: &mut UserMemoryContext<'_, M>,
    ) -> Result<PublishedSignalFrame, SignalFramePublishError> {
        let frame_ptr = self.layout.frame_start as *mut SignalFrame;
        // Publish from the highest object downward. This keeps the fixed
        // frame's user pointer valid before the restorer becomes reachable.
        // The XSAVE signal image is an initialized byte vector, and the
        // userspace pointer is treated as an opaque byte address.
        let fp_result = memory.write_bytes(self.layout.fpstate, self.fpstate.0.as_bytes());
        fp_result.map_err(SignalFramePublishError::Fpstate)?;
        self.frame
            .write_to_user(memory, frame_ptr)
            .map_err(SignalFramePublishError::Frame)?;
        (self.layout.published_sp as *mut usize)
            .vm_write(memory, self.restorer)
            .map_err(SignalFramePublishError::Restorer)?;

        let mut context = self.interrupted;
        context.set_ip(self.handler);
        context.set_sp(self.layout.published_sp);
        context.set_arg0(self.signo as _);
        context.set_arg1(self.layout.siginfo);
        context.set_arg2(self.layout.ucontext);

        Ok(PublishedSignalFrame {
            layout: self.layout,
            context,
        })
    }
}

/// Why one-time signal-frame publication failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalFramePublishError {
    /// The complete XSAVE payload could not be copied to userspace.
    Fpstate(UserCopyError),
    /// The frame object could not be copied to userspace.
    Frame(UserCopyError),
    /// The restorer word could not be copied to userspace.
    Restorer(UserCopyError),
}

/// A successfully copied signal frame whose new context can be installed once.
#[must_use = "installing or dropping the published frame completes delivery"]
pub struct PublishedSignalFrame {
    layout: SignalFrameLayout,
    context: UserContext,
}

impl PublishedSignalFrame {
    /// Returns the context that corresponds to the copied frame.
    pub const fn context(&self) -> &UserContext {
        &self.context
    }

    /// Returns the copied frame placement.
    pub const fn layout(&self) -> SignalFrameLayout {
        self.layout
    }

    /// Installs the published handler context exactly once.
    pub fn install(self, current: &mut UserContext) {
        *current = self.context;
    }

    /// Alias for [`Self::install`] useful to adapters that call publication a
    /// commit.
    pub fn commit(self, current: &mut UserContext) {
        self.install(current);
    }
}

/// Prepares a Linux x86_64 signal frame with a policy-selected fpstate image.
#[allow(clippy::too_many_arguments)]
pub fn prepare_signal_frame_with_fpstate_snapshot(
    interrupted: &UserContext,
    restore_blocked: SignalSet,
    configured_stack: SignalStack,
    stack: SignalFrameStack,
    siginfo: SignalInfo,
    handler: usize,
    restorer: usize,
    snapshot: impl FnOnce() -> Result<SignalFpState, SignalFrameLayoutError>,
) -> Result<PreparedSignalFrame, SignalFrameLayoutError> {
    let fpstate = snapshot()?;
    let layout = SignalFrameLayout::with_fpstate(
        interrupted.sp(),
        &configured_stack,
        stack,
        fpstate.size(),
        fpstate.alignment(),
    )?;
    prepare_signal_frame_from_fpstate(
        interrupted,
        restore_blocked,
        configured_stack,
        stack,
        siginfo,
        handler,
        restorer,
        layout,
        fpstate,
    )
}

#[allow(clippy::too_many_arguments)]
fn prepare_signal_frame_from_fpstate(
    interrupted: &UserContext,
    restore_blocked: SignalSet,
    configured_stack: SignalStack,
    _stack: SignalFrameStack,
    siginfo: SignalInfo,
    handler: usize,
    restorer: usize,
    layout: SignalFrameLayout,
    fpstate: SignalFpState,
) -> Result<PreparedSignalFrame, SignalFrameLayoutError> {
    let mut visible_stack = configured_stack;
    // rt_sigreturn must retain the AUTODISARM configuration even though the
    // live thread state is disabled for the handler's dynamic extent.
    if configured_stack.flags & (1 << 31) == 0 {
        visible_stack.flags = configured_stack.flags_at(interrupted.sp());
    }
    Ok(PreparedSignalFrame {
        layout,
        frame: SignalFrame::new_with_fpstate(
            interrupted,
            restore_blocked,
            visible_stack,
            siginfo.clone(),
            layout.fpstate(),
        ),
        fpstate,
        restorer,
        handler,
        signo: siginfo.signo(),
        interrupted: *interrupted,
    })
}

/// A fully validated signal return that can be committed without failure.
#[must_use = "committing or dropping the prepared restore completes sigreturn"]
pub struct PreparedSignalRestore {
    context: UserContext,
    blocked: SignalSet,
    stack: Option<SignalStack>,
    stack_error: Option<SignalStackRestoreError>,
    fpstate: usize,
}

impl PreparedSignalRestore {
    /// Returns the validated candidate user context.
    pub const fn context(&self) -> &UserContext {
        &self.context
    }

    /// Returns the validated alternate-stack update, if one will be applied.
    pub const fn stack(&self) -> Option<&SignalStack> {
        self.stack.as_ref()
    }

    /// Returns a Linux-compatible, squashed `restore_altstack()` error.
    pub const fn stack_error(&self) -> Option<SignalStackRestoreError> {
        self.stack_error
    }

    /// Returns the sanitized signal mask that will be committed.
    pub const fn blocked(&self) -> SignalSet {
        self.blocked
    }

    /// Returns the user fpstate address from the validated fixed frame.
    pub const fn fpstate_address(&self) -> usize {
        self.fpstate
    }

    /// Commits the prepared context to a caller-owned context exactly once.
    /// Manager-owned mask and alternate-stack state remain the caller's
    /// responsibility.
    pub fn commit_context(self, current: &mut UserContext) {
        *current = self.context;
    }

    /// Splits the one-shot token for a manager that owns mask and stack state.
    pub(crate) fn into_parts(self) -> (UserContext, SignalSet, Option<SignalStack>) {
        (self.context, self.blocked, self.stack)
    }
}

/// Prepares an owned signal frame for `rt_sigreturn` without publishing any
/// context, mask, or alternate-stack state.
///
/// The supplied predicates and callback keep address-space and policy checks
/// out of this reusable data plane.  Invalid alternate-stack restoration is
/// intentionally squashed into `stack_error`, matching Linux's non-copy
/// `restore_altstack()` behavior.
pub fn prepare_signal_restore(
    current: &UserContext,
    frame: SignalFrame,
    valid_program_counter: impl FnOnce(usize) -> bool,
    valid_stack_pointer: impl FnOnce(usize) -> bool,
    current_stack: SignalStack,
    validate_stack: impl FnOnce(
        &SignalStack,
        usize,
        &SignalStack,
    ) -> Result<(), SignalStackRestoreError>,
) -> Result<PreparedSignalRestore, SignalContextError> {
    let context = frame.ucontext.mcontext.prepare_restore(current)?;
    if !valid_program_counter(context.ip()) {
        return Err(SignalContextError::InvalidProgramCounter);
    }
    if !valid_stack_pointer(context.sp()) {
        return Err(SignalContextError::InvalidStackPointer);
    }

    let mut blocked = frame.ucontext.sigmask;
    blocked.remove(Signo::SIGKILL);
    blocked.remove(Signo::SIGSTOP);

    let candidate = frame.ucontext.stack.prepare_restore();
    let (stack, stack_error) = match candidate {
        Ok(candidate) => match validate_stack(&current_stack, current.sp(), &candidate) {
            Ok(()) => (Some(candidate), None),
            Err(error) => (None, Some(error)),
        },
        Err(error) => (None, Some(error)),
    };

    Ok(PreparedSignalRestore {
        context,
        blocked,
        stack,
        stack_error,
        fpstate: frame.ucontext.mcontext.fpstate(),
    })
}

/// Copies and prepares a complete signal return frame from userspace.
pub fn copyin_and_prepare_restore<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: *const SignalFrame,
    current: &UserContext,
    valid_program_counter: impl FnOnce(usize) -> bool,
    valid_stack_pointer: impl FnOnce(usize) -> bool,
    current_stack: SignalStack,
    validate_stack: impl FnOnce(
        &SignalStack,
        usize,
        &SignalStack,
    ) -> Result<(), SignalStackRestoreError>,
) -> Result<PreparedSignalRestore, SignalFrameRestoreError> {
    let frame =
        SignalFrame::read_from_user(memory, ptr).map_err(SignalFrameRestoreError::Copyin)?;
    prepare_signal_restore(
        current,
        frame,
        valid_program_counter,
        valid_stack_pointer,
        current_stack,
        validate_stack,
    )
    .map_err(SignalFrameRestoreError::Context)
}

/// Copies and prepares a signal return frame whose fixed frame starts at the
/// current user `rsp`. On x86_64 the restorer has already popped `pretcode`,
/// so `rsp` points directly at `ucontext`; callers must not add eight bytes.
pub fn copyin_and_prepare_restore_at_rsp<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    current_rsp: usize,
    current: &UserContext,
    valid_program_counter: impl FnOnce(usize) -> bool,
    valid_stack_pointer: impl FnOnce(usize) -> bool,
    current_stack: SignalStack,
    validate_stack: impl FnOnce(
        &SignalStack,
        usize,
        &SignalStack,
    ) -> Result<(), SignalStackRestoreError>,
) -> Result<PreparedSignalRestore, SignalFrameRestoreError> {
    copyin_and_prepare_restore(
        memory,
        current_rsp as *const SignalFrame,
        current,
        valid_program_counter,
        valid_stack_pointer,
        current_stack,
        validate_stack,
    )
}

/// Why copying or validating a signal return frame failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalFrameRestoreError {
    /// The complete frame could not be copied from userspace.
    Copyin(UserCopyError),
    /// The owned frame failed architectural or caller-supplied validation.
    Context(SignalContextError),
}
