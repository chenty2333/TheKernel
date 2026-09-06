use core::{ffi::c_ulong, mem};

use bitflags::bitflags;
use linux_raw_sys::general::{
    SA_NOCLDSTOP, SA_NOCLDWAIT, SA_NODEFER, SA_ONSTACK, SA_RESETHAND, SA_RESTART, SA_SIGINFO,
    kernel_sigaction,
};
use thekernel_linux_usercopy::{UserMemory, UserMemoryContext, VmMutPtr, VmPtr, VmResult};

use crate::SignalSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultSignalAction {
    /// Terminate the process.
    Terminate,
    /// Ignore the signal.
    Ignore,
    /// Terminate the process and generate a core dump.
    CoreDump,
    /// Stop the process.
    Stop,
    /// Continue the process if stopped.
    Continue,
}

/// Signal action that should be properly handled by the OS.
///
/// See [`ThreadSignalManager::check_signals`](crate::api::ThreadSignalManager::check_signals)
/// for details.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalOSAction {
    /// Terminate the process.
    Terminate,
    /// Generate a core dump and terminate the process.
    CoreDump,
    /// Stop the process.
    Stop,
    /// Continue the process if stopped.
    Continue,
    /// A signal handler is pushed into the signal stack. The OS doesn't need to
    /// do anything.
    Handler,
}

bitflags! {
    #[derive(Default, Debug, Clone, Copy)]
    pub struct SignalActionFlags: c_ulong {
        const SIGINFO = SA_SIGINFO as _;
        const NOCLDSTOP = SA_NOCLDSTOP as _;
        const NOCLDWAIT = SA_NOCLDWAIT as _;
        const NODEFER = SA_NODEFER as _;
        const RESETHAND = SA_RESETHAND as _;
        const RESTART = SA_RESTART as _;
        const ONSTACK = SA_ONSTACK as _;
        const RESTORER = 0x4000000;
    }
}

/// The byte-level Linux `rt_sigaction` record copied across the user boundary.
///
/// Function addresses deliberately remain integers here. Bindgen represents
/// them as `Option<extern "C" fn>`, for which arbitrary userspace bytes are not
/// a valid Rust value. This all-integer mirror can be initialized from any bit
/// pattern and is converted only after the complete record has been copied.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct RawSignalAction {
    /// `SIG_DFL` (0), `SIG_IGN` (1), or a userspace handler address.
    pub handler: usize,
    /// Linux `SA_*` flags.
    pub flags: c_ulong,
    /// Userspace restorer address in the x86_64 Linux `kernel_sigaction` ABI.
    pub restorer: usize,
    /// Signals blocked while the handler runs.
    pub mask: SignalSet,
}

const _: [(); mem::size_of::<kernel_sigaction>()] = [(); mem::size_of::<RawSignalAction>()];
const _: [(); mem::align_of::<kernel_sigaction>()] = [(); mem::align_of::<RawSignalAction>()];
const _: [(); mem::offset_of!(kernel_sigaction, sa_handler_kernel)] =
    [(); mem::offset_of!(RawSignalAction, handler)];
const _: [(); mem::offset_of!(kernel_sigaction, sa_flags)] =
    [(); mem::offset_of!(RawSignalAction, flags)];
const _: [(); mem::offset_of!(kernel_sigaction, sa_restorer)] =
    [(); mem::offset_of!(RawSignalAction, restorer)];
const _: [(); mem::offset_of!(kernel_sigaction, sa_mask)] =
    [(); mem::offset_of!(RawSignalAction, mask)];
const _: [(); mem::size_of::<RawSignalAction>()] =
    [(); mem::offset_of!(RawSignalAction, mask) + mem::size_of::<SignalSet>()];

impl RawSignalAction {
    /// Copies a complete raw action from userspace.
    pub fn read_from_user<M: UserMemory + ?Sized>(
        memory: &mut UserMemoryContext<'_, M>,
        ptr: *const Self,
    ) -> VmResult<Self> {
        let value = ptr.vm_read_uninit(memory)?;
        // SAFETY: UserMemory initialized every byte before returning `Ok` and this
        // repr(C) record contains only integer scalars plus SignalSet, which is
        // repr(transparent) over u64. Therefore every bit pattern is valid.
        Ok(unsafe { value.assume_init() })
    }

    /// Copies this raw action to userspace.
    #[allow(clippy::not_unsafe_ptr_arg_deref)] // The pointer is an opaque user address.
    pub fn write_to_user<M: UserMemory + ?Sized>(
        self,
        memory: &mut UserMemoryContext<'_, M>,
        ptr: *mut Self,
    ) -> VmResult {
        // SAFETY: the size/offset assertions above prove this record has no
        // implicit gaps or tail padding on the supported 64-bit ABIs. Every
        // field is initialized by safe construction or a complete user read.
        unsafe { ptr.vm_write_unchecked(memory, self) }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub enum SignalDisposition {
    #[default]
    /// Use the default signal action.
    Default,
    /// Ignore the signal.
    Ignore,
    /// Address of a custom userspace signal handler.
    Handler(usize),
}

/// Signal action. Corresponds to `struct sigaction` in libc.
#[derive(Debug, Clone, Copy, Default)]
pub struct SignalAction {
    pub flags: SignalActionFlags,
    pub mask: SignalSet,
    pub disposition: SignalDisposition,
    /// Optional userspace restorer address. `Some(0)` is distinct from using
    /// the kernel-provided default restorer.
    pub restorer: Option<usize>,
}

impl From<SignalAction> for RawSignalAction {
    fn from(value: SignalAction) -> Self {
        let handler = match value.disposition {
            SignalDisposition::Default => 0,
            SignalDisposition::Ignore => 1,
            SignalDisposition::Handler(handler) => handler,
        };
        let restorer = value.restorer.unwrap_or(0);

        Self {
            handler,
            flags: value.flags.bits(),
            restorer,
            mask: value.mask,
        }
    }
}

impl From<RawSignalAction> for SignalAction {
    fn from(value: RawSignalAction) -> Self {
        let flags = SignalActionFlags::from_bits_truncate(value.flags);
        let disposition = match value.handler {
            0 => SignalDisposition::Default,
            1 => SignalDisposition::Ignore,
            handler => SignalDisposition::Handler(handler),
        };

        let restorer = if flags.contains(SignalActionFlags::RESTORER) {
            Some(value.restorer)
        } else {
            None
        };

        SignalAction {
            flags,
            mask: value.mask,
            disposition,
            restorer,
        }
    }
}
