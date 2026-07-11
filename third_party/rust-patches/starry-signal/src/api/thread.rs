use alloc::{
    alloc::AllocError,
    sync::{Arc, Weak},
};
use core::{
    alloc::Layout,
    mem::offset_of,
    sync::atomic::{AtomicBool, Ordering},
};

use axcpu::uspace::UserContext;
use kspin::SpinNoIrq;
use starry_vm::{VmMutPtr, VmPtr, VmResult};

use super::ProcessSignalManager;
use crate::{
    DefaultSignalAction, PendingSignals, SignalAction, SignalActionFlags, SignalDisposition,
    SignalInfo, SignalOSAction, SignalSet, SignalStack, Signo,
    arch::{SignalContextError, UContext},
};

/// The userspace ABI frame created for a signal handler.
///
/// This contains only Linux-visible signal state. Kernel trap metadata is not
/// serialized into userspace and therefore cannot be forged by `sigreturn`.
#[repr(C)]
#[derive(Clone)]
pub struct SignalFrame {
    ucontext: UContext,
    siginfo: SignalInfo,
}

impl SignalFrame {
    fn new(uctx: &UserContext, sigmask: SignalSet, siginfo: SignalInfo) -> Self {
        Self {
            ucontext: UContext::new(uctx, sigmask),
            siginfo,
        }
    }

    /// Returns the Linux-visible user context stored in this frame.
    pub fn ucontext(&self) -> &UContext {
        &self.ucontext
    }

    /// Returns a mutable Linux-visible user context, as a signal handler sees it.
    pub fn ucontext_mut(&mut self) -> &mut UContext {
        &mut self.ucontext
    }

    /// Copies a complete signal frame from userspace into an owned value.
    pub fn read_from_user(ptr: *const Self) -> VmResult<Self> {
        let frame = ptr.vm_read_uninit()?;
        // SAFETY: VmPtr returns `Ok` only after VmIo initialized every byte of
        // the destination. SignalFrame and every architecture's UContext and
        // MContext are repr(C) records made solely from integer scalars and
        // integer/byte arrays. SignalStack is {usize, u32, usize}, SignalSet is
        // a transparent u64, and SignalInfo wraps Linux's raw repr(C) siginfo
        // union; none contains bool, a Rust enum, a reference, or NonZero state.
        // Raw union/pointer storage accepts arbitrary user bits. Restoration
        // never interprets frame.siginfo (in particular, it never calls
        // SignalInfo::signo), and prepare_restore validates every machine field
        // that has architectural constraints before publication.
        Ok(unsafe { frame.assume_init() })
    }
}

/// A fully validated signal return that can be committed without failure.
pub struct PreparedSignalRestore {
    context: UserContext,
    blocked: SignalSet,
}

impl PreparedSignalRestore {
    /// Returns the validated candidate user context.
    pub fn context(&self) -> &UserContext {
        &self.context
    }
}

pub struct DeliveredSignal {
    pub info: SignalInfo,
    pub os_action: SignalOSAction,
    pub restartable_handler: bool,
}

/// Thread-level signal manager.
pub struct ThreadSignalManager {
    /// The process-level signal manager
    proc: Arc<ProcessSignalManager>,

    /// The pending signals
    pending: SpinNoIrq<PendingSignals>,
    /// The set of signals currently blocked from delivery.
    blocked: SpinNoIrq<SignalSet>,
    /// Temporarily preserved mask while a synchronous wait unblocks signals.
    real_blocked: SpinNoIrq<Option<SignalSet>>,
    /// The stack used by signal handlers
    stack: SpinNoIrq<SignalStack>,

    possibly_has_signal: AtomicBool,
}

/// Removes a newly registered weak endpoint if the owning thread fails to
/// finish construction. Successful lifecycle publication disarms the token.
#[must_use = "dropping the token rolls back thread-signal registration"]
pub struct ThreadSignalRegistration {
    manager: Arc<ThreadSignalManager>,
    tid: u32,
    active: bool,
}

impl ThreadSignalRegistration {
    pub fn commit(mut self) {
        self.active = false;
    }
}

impl Drop for ThreadSignalRegistration {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let expected = Arc::downgrade(&self.manager);
        self.manager
            .proc
            .children
            .lock()
            .retain(|(tid, thread)| *tid != self.tid || !thread.ptr_eq(&expected));
    }
}

impl ThreadSignalManager {
    /// Fallibly constructs an unregistered thread signal endpoint.
    /// Registration is separate so callers can finish building the owning
    /// thread object before making even a weak child entry observable.
    pub fn try_new(proc: Arc<ProcessSignalManager>) -> Result<Arc<Self>, AllocError> {
        Arc::try_new(Self {
            proc,

            pending: SpinNoIrq::new(PendingSignals::default()),
            blocked: SpinNoIrq::new(SignalSet::default()),
            real_blocked: SpinNoIrq::new(None),
            stack: SpinNoIrq::new(SignalStack::default()),

            possibly_has_signal: AtomicBool::new(false),
        })
    }

    /// Fallibly publishes this endpoint in its process signal registry.
    pub fn try_register(
        self: &Arc<Self>,
        tid: u32,
    ) -> Result<ThreadSignalRegistration, AllocError> {
        let mut children = self.proc.children.lock();
        children.try_reserve(1).map_err(|_| AllocError)?;
        children.push((tid, Arc::downgrade(self)));
        Ok(ThreadSignalRegistration {
            manager: self.clone(),
            tid,
            active: true,
        })
    }

    /// Constructs and registers a signal endpoint using the allocator's
    /// configured OOM handler. New kernel lifecycle paths should prefer the
    /// two fallible methods above.
    pub fn new(tid: u32, proc: Arc<ProcessSignalManager>) -> Arc<Self> {
        let this = Self::try_new(proc)
            .unwrap_or_else(|_| alloc::alloc::handle_alloc_error(Layout::new::<Self>()));
        let registration = this.try_register(tid).unwrap_or_else(|_| {
            alloc::alloc::handle_alloc_error(Layout::new::<(u32, Weak<Self>)>())
        });
        registration.commit();
        this
    }

    /// Dequeues a signal from the thread's pending signals.
    #[must_use]
    pub fn dequeue_signal(&self, mask: &SignalSet) -> Option<SignalInfo> {
        self.pending
            .lock()
            .dequeue_signal(mask)
            .or_else(|| self.proc.dequeue_signal(mask))
    }

    pub fn process(&self) -> &Arc<ProcessSignalManager> {
        &self.proc
    }

    pub fn handle_signal(
        &self,
        uctx: &mut UserContext,
        restore_blocked: SignalSet,
        sig: &SignalInfo,
        action: &SignalAction,
    ) -> Option<SignalOSAction> {
        let signo = sig.signo();
        debug!("Handle signal: {signo:?}");
        match action.disposition {
            SignalDisposition::Default => match signo.default_action() {
                DefaultSignalAction::Terminate => Some(SignalOSAction::Terminate),
                DefaultSignalAction::CoreDump => Some(SignalOSAction::CoreDump),
                DefaultSignalAction::Stop => Some(SignalOSAction::Stop),
                DefaultSignalAction::Ignore => None,
                DefaultSignalAction::Continue => Some(SignalOSAction::Continue),
            },
            SignalDisposition::Ignore => None,
            SignalDisposition::Handler(handler) => {
                let layout = Layout::new::<SignalFrame>();
                let stack = self.stack.lock();
                let sp = if stack.disabled() || !action.flags.contains(SignalActionFlags::ONSTACK) {
                    uctx.sp()
                } else {
                    stack.sp + stack.size
                };
                drop(stack);

                let aligned_sp = (sp - layout.size()) & !(layout.align() - 1);

                let frame_ptr = aligned_sp as *mut SignalFrame;
                if frame_ptr
                    .vm_write(SignalFrame::new(uctx, restore_blocked, sig.clone()))
                    .is_err()
                {
                    return Some(SignalOSAction::CoreDump);
                }

                uctx.set_ip(handler);
                uctx.set_sp(aligned_sp);
                uctx.set_arg0(signo as _);
                uctx.set_arg1(aligned_sp + offset_of!(SignalFrame, siginfo));
                uctx.set_arg2(aligned_sp + offset_of!(SignalFrame, ucontext));

                let restorer = action.restorer.unwrap_or(self.proc.default_restorer);
                #[cfg(target_arch = "x86_64")]
                {
                    let new_sp = uctx.sp() - 8;
                    if (new_sp as *mut usize).vm_write(restorer).is_err() {
                        return Some(SignalOSAction::CoreDump);
                    }
                    uctx.set_sp(new_sp);
                }
                #[cfg(not(target_arch = "x86_64"))]
                uctx.set_ra(restorer);

                let mut add_blocked = action.mask;
                if !action.flags.contains(SignalActionFlags::NODEFER) {
                    add_blocked.add(signo);
                }

                if action.flags.contains(SignalActionFlags::RESETHAND) {
                    self.proc.actions.lock()[signo] = SignalAction::default();
                }
                *self.blocked.lock() |= add_blocked;
                Some(SignalOSAction::Handler)
            }
        }
    }

    #[cold]
    fn check_signals_slow(
        &self,
        uctx: &mut UserContext,
        restore_blocked: Option<SignalSet>,
    ) -> Option<DeliveredSignal> {
        let blocked = self.blocked.lock();
        let mask = !*blocked;
        let restore_blocked = restore_blocked.unwrap_or_else(|| *blocked);
        drop(blocked);

        loop {
            let sig = match self.pending.lock().dequeue_signal(&mask) {
                Some(sig) => Some(sig),
                None => {
                    self.possibly_has_signal.store(false, Ordering::Release);
                    self.proc.dequeue_signal(&mask)
                }
            }?;
            let action = self.proc.actions.lock()[sig.signo()].clone();
            let restartable_handler = matches!(action.disposition, SignalDisposition::Handler(_))
                && action.flags.contains(SignalActionFlags::RESTART);

            if let Some(os_action) = self.handle_signal(uctx, restore_blocked, &sig, &action) {
                break Some(DeliveredSignal {
                    info: sig,
                    os_action,
                    restartable_handler,
                });
            }
        }
    }

    /// Checks pending signals and handle them.
    ///
    /// Returns the signal number and the action the OS should take, if any.
    pub fn check_signals(
        &self,
        uctx: &mut UserContext,
        restore_blocked: Option<SignalSet>,
    ) -> Option<DeliveredSignal> {
        // Fast path
        if !self.possibly_has_signal.load(Ordering::Acquire)
            && !self.proc.possibly_has_signal.load(Ordering::Acquire)
        {
            return None;
        }
        self.check_signals_slow(uctx, restore_blocked)
    }

    /// Validates an owned signal frame without publishing any state.
    ///
    /// The caller must copy the complete frame from userspace before calling
    /// this method. Address predicates keep kernel address-space policy out of
    /// this reusable signal crate.
    pub fn prepare_restore(
        &self,
        current: &UserContext,
        frame: SignalFrame,
        valid_program_counter: impl FnOnce(usize) -> bool,
        valid_stack_pointer: impl FnOnce(usize) -> bool,
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
        Ok(PreparedSignalRestore { context, blocked })
    }

    /// Commits a previously validated signal restore without failure.
    pub fn commit_restore(&self, uctx: &mut UserContext, prepared: PreparedSignalRestore) {
        *uctx = prepared.context;
        self.set_blocked(prepared.blocked);
    }

    /// Sends a signal to the thread.
    ///
    /// Returns `true` if the task was woken up by the signal (i.e. the signal
    /// was not blocked and not ignored).
    ///
    /// See [`ProcessSignalManager::send_signal`] for the process-level version.
    #[must_use]
    pub fn send_signal(&self, sig: SignalInfo) -> bool {
        let signo = sig.signo();
        let blocked = self.signal_blocked(signo);
        if self.proc.signal_ignored(signo) && !blocked && !self.signal_real_blocked(signo) {
            return false;
        }

        if self.pending.lock().put_signal(sig) {
            self.possibly_has_signal.store(true, Ordering::Release);
        }
        !blocked
    }

    /// Gets the blocked signals.
    pub fn blocked(&self) -> SignalSet {
        *self.blocked.lock()
    }

    /// Sets the blocked signals. Return the old value.
    pub fn set_blocked(&self, mut set: SignalSet) -> SignalSet {
        set.remove(Signo::SIGKILL);
        set.remove(Signo::SIGSTOP);
        self.possibly_has_signal.store(true, Ordering::Release);
        let mut guard = self.blocked.lock();
        let old = *guard;
        *guard = set;
        old
    }

    /// Checks if a signal is blocked.
    pub fn signal_blocked(&self, signo: Signo) -> bool {
        self.blocked.lock().has(signo)
    }

    pub fn signal_real_blocked(&self, signo: Signo) -> bool {
        self.real_blocked.lock().is_some_and(|set| set.has(signo))
    }

    pub fn set_real_blocked(&self, set: Option<SignalSet>) {
        *self.real_blocked.lock() = set;
    }

    /// Gets the signal stack.
    pub fn stack(&self) -> SignalStack {
        self.stack.lock().clone()
    }

    /// Sets the signal stack.
    pub fn set_stack(&self, stack: SignalStack) {
        *self.stack.lock() = stack;
    }

    /// Gets current pending signals.
    pub fn pending(&self) -> SignalSet {
        self.pending.lock().set | self.proc.pending()
    }

    pub fn pending_realtime_count(&self) -> usize {
        self.pending.lock().realtime_count() + self.proc.pending_realtime_count()
    }
}
