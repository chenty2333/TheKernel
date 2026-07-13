use alloc::{alloc::AllocError, sync::Arc, vec::Vec};
use core::{
    alloc::Layout,
    mem::offset_of,
    sync::atomic::{AtomicBool, Ordering},
};

use axcpu::uspace::UserContext;
use kspin::SpinNoIrq;
use starry_vm::{VmMutPtr, VmPtr, VmResult};

use super::{ProcessSignalManager, RegisteredThread};
use crate::{
    DefaultSignalAction, DetachedSignal, PendingSignals, PreparedSignal, SignalAction,
    SignalActionFlags, SignalDisposition, SignalInfo, SignalOSAction, SignalSet, SignalStack,
    Signo,
    arch::{SignalContextError, UContext},
};

/// Result of publishing one thread-directed signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThreadSignalSendOutcome {
    pub published: bool,
    pub wake: bool,
}

/// Why a thread endpoint could not complete registry admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadRegistrationError {
    /// Allocating the registry entry or immutable replacement failed.
    NoMemory,
    /// This endpoint already owns a registration identity.
    AlreadyRegistered,
    /// Another live endpoint in the process already owns this thread ID.
    TidInUse,
    /// Retirement cancelled this admission before it could be committed.
    Cancelled,
}

impl From<AllocError> for ThreadRegistrationError {
    fn from(_: AllocError) -> Self {
        Self::NoMemory
    }
}

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
    /// Publication state for the exact private endpoint.
    ///
    /// This gate linearizes direct send with exit retirement. Registry state
    /// alone is insufficient because an exact task or pidfd sender may retain
    /// an `Arc<ThreadSignalManager>` after routing has been disabled.
    lifecycle: SpinNoIrq<u8>,
    /// The one registry identity currently admitted for this endpoint.
    registration: SpinNoIrq<Option<Arc<RegisteredThread>>>,
    /// The set of signals currently blocked from delivery.
    blocked: SpinNoIrq<SignalSet>,
    /// Temporarily preserved mask while a synchronous wait unblocks signals.
    real_blocked: SpinNoIrq<Option<SignalSet>>,
    /// The stack used by signal handlers
    stack: SpinNoIrq<SignalStack>,

    possibly_has_signal: AtomicBool,
}

/// Deactivates a newly registered endpoint if the owning thread fails to
/// finish construction. Successful lifecycle publication disarms the token.
///
/// Rollback deactivates only this token's exact registry entry and clears the
/// manager-owned identity only if it still points to that entry. Any final Arc
/// destruction happens after the IRQ-disabled lifecycle guards are released.
#[must_use = "dropping the token rolls back thread-signal registration"]
pub struct ThreadSignalRegistration {
    entry: Arc<RegisteredThread>,
    thread: Arc<ThreadSignalManager>,
    rollback: bool,
}

const ENDPOINT_PENDING: u8 = 0;
const ENDPOINT_ACTIVE: u8 = 1;
const ENDPOINT_RETAINED: u8 = 2;
const ENDPOINT_CANCELLED: u8 = 3;

#[derive(Clone, Copy)]
enum EndpointSendMode {
    Active,
    Retained,
}

impl EndpointSendMode {
    const fn accepts(self, state: u8) -> bool {
        matches!(
            (self, state),
            (Self::Active, ENDPOINT_ACTIVE) | (Self::Retained, ENDPOINT_RETAINED)
        )
    }
}

impl ThreadSignalRegistration {
    /// Activates this exact admitted identity unless retirement won first.
    pub fn commit(mut self) -> Result<(), ThreadRegistrationError> {
        let update = self.thread.proc.action_update.lock();
        let mut lifecycle = self.thread.lifecycle.lock();
        let still_admitted = self
            .thread
            .registration
            .lock()
            .as_ref()
            .is_some_and(|entry| Arc::ptr_eq(entry, &self.entry));
        if !still_admitted || *lifecycle != ENDPOINT_PENDING {
            drop(lifecycle);
            drop(update);
            return Err(ThreadRegistrationError::Cancelled);
        }
        *lifecycle = ENDPOINT_ACTIVE;
        self.entry.activate();
        self.rollback = false;
        drop(lifecycle);
        drop(update);
        Ok(())
    }
}

impl Drop for ThreadSignalRegistration {
    fn drop(&mut self) {
        if self.rollback {
            let mut lifecycle = self.thread.lifecycle.lock();
            self.entry.deactivate();
            let removed = {
                let mut registration = self.thread.registration.lock();
                if registration
                    .as_ref()
                    .is_some_and(|entry| Arc::ptr_eq(entry, &self.entry))
                {
                    *lifecycle = ENDPOINT_CANCELLED;
                    registration.take()
                } else {
                    None
                }
            };
            drop(lifecycle);
            // The final registry-entry owner may deallocate. Release it only
            // after every IRQ-disabled state guard has left scope.
            drop(removed);
        }
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
            lifecycle: SpinNoIrq::new(ENDPOINT_PENDING),
            registration: SpinNoIrq::new(None),
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
    ) -> Result<ThreadSignalRegistration, ThreadRegistrationError> {
        let update = self.proc.action_update.lock();
        if self.registration.lock().is_some() {
            return Err(ThreadRegistrationError::AlreadyRegistered);
        }
        let registry = self.proc.children_registry_snapshot();
        let mut live = 0usize;
        if let Some(registry) = registry.as_deref() {
            for registered in registry {
                if registered.is_live() {
                    if registered.claims_tid(tid) {
                        return Err(ThreadRegistrationError::TidInUse);
                    }
                    live += 1;
                }
            }
        }
        let capacity = live
            .checked_add(1)
            .ok_or(ThreadRegistrationError::NoMemory)?;
        let mut replacement = Vec::new();
        replacement
            .try_reserve_exact(capacity)
            .map_err(|_| ThreadRegistrationError::NoMemory)?;
        if let Some(registry) = registry.as_deref() {
            for registered in registry {
                if registered.is_live() {
                    replacement.push(registered.clone());
                }
            }
        }
        let entry = RegisteredThread::try_new(tid, self)?;
        replacement.push(entry.clone());
        let replacement =
            Arc::try_new(replacement).map_err(|_| ThreadRegistrationError::NoMemory)?;

        {
            let mut lifecycle = self.lifecycle.lock();
            let mut registration = self.registration.lock();
            debug_assert!(registration.is_none());
            *lifecycle = ENDPOINT_PENDING;
            *registration = Some(entry.clone());
        }

        let previous = {
            let mut children = self.proc.children.lock();
            children.replace(replacement)
        };

        // The immutable registry and all of its owned Arcs are allocated and
        // destroyed outside the publication spin lock. The shared update
        // mutex serializes this pointer swap with disposition transitions.
        drop(update);
        drop(previous);
        drop(registry);
        Ok(ThreadSignalRegistration {
            entry,
            thread: self.clone(),
            rollback: true,
        })
    }

    /// Removes an exited task from process-directed routing while optionally
    /// retaining its private pending queue for Linux's unreaped group-leader
    /// identity. Retained endpoints still participate in disposition-driven
    /// pending flushes, but can never be selected as a live wake target.
    pub fn retire_registration(&self, tid: u32, retain_private_pending: bool) {
        let update = self.proc.action_update.lock();
        let registry = self.proc.children_registry_snapshot();
        let entry = registry.as_deref().and_then(|registry| {
            registry
                .iter()
                .find(|entry| entry.matches(tid, self as *const Self))
        });
        let mut detached = None;
        if let Some(entry) = entry {
            let mut lifecycle = self.lifecycle.lock();
            if retain_private_pending {
                if *lifecycle == ENDPOINT_ACTIVE {
                    entry.retain_pending_only();
                    *lifecycle = ENDPOINT_RETAINED;
                }
            } else {
                entry.deactivate();
                *lifecycle = ENDPOINT_CANCELLED;
                detached = Some(self.pending.lock().take_all());
                self.possibly_has_signal.store(false, Ordering::Release);
            }
            drop(lifecycle);
        }
        drop(registry);
        drop(update);
        // Queue-account Arcs and RT nodes are destroyed only after the
        // lifecycle, registry, and action-update guards have been released.
        drop(detached);
    }

    /// Dequeues a signal from the thread's pending signals.
    #[must_use]
    pub fn dequeue_signal(&self, mask: &SignalSet) -> Option<SignalInfo> {
        self.dequeue_thread_signal(mask)
            .or_else(|| self.proc.dequeue_signal(mask))
    }

    fn dequeue_thread_signal(&self, mask: &SignalSet) -> Option<SignalInfo> {
        let signal = {
            let mut pending = self.pending.lock();
            let signal = pending.dequeue_signal(mask);
            if pending.set.is_empty() {
                self.possibly_has_signal.store(false, Ordering::Release);
            }
            signal
        };
        signal.map(|signal| signal.into_info())
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
                let stack = self.stack.lock().clone();
                let already_on_altstack = stack.contains_sp(uctx.sp());
                let use_altstack = action.flags.contains(SignalActionFlags::ONSTACK)
                    && !stack.disabled()
                    && !already_on_altstack;
                let sp = if use_altstack {
                    let Some(top) = stack.checked_top() else {
                        return Some(SignalOSAction::CoreDump);
                    };
                    top
                } else {
                    uctx.sp()
                };

                let Some(frame_start) = sp.checked_sub(layout.size()) else {
                    return Some(SignalOSAction::CoreDump);
                };
                let aligned_sp = frame_start & !(layout.align() - 1);
                let Some(siginfo_ptr) = aligned_sp.checked_add(offset_of!(SignalFrame, siginfo))
                else {
                    return Some(SignalOSAction::CoreDump);
                };
                let Some(ucontext_ptr) = aligned_sp.checked_add(offset_of!(SignalFrame, ucontext))
                else {
                    return Some(SignalOSAction::CoreDump);
                };

                #[cfg(target_arch = "x86_64")]
                let Some(published_sp) = aligned_sp.checked_sub(core::mem::size_of::<usize>())
                else {
                    return Some(SignalOSAction::CoreDump);
                };
                #[cfg(not(target_arch = "x86_64"))]
                let published_sp = aligned_sp;

                if use_altstack || already_on_altstack {
                    let Some(frame_span) = sp.checked_sub(published_sp) else {
                        return Some(SignalOSAction::CoreDump);
                    };
                    if !stack.contains_range(published_sp, frame_span) {
                        return Some(SignalOSAction::CoreDump);
                    }
                }

                let frame_ptr = aligned_sp as *mut SignalFrame;
                if frame_ptr
                    .vm_write(SignalFrame::new(uctx, restore_blocked, sig.clone()))
                    .is_err()
                {
                    return Some(SignalOSAction::CoreDump);
                }

                let restorer = action.restorer.unwrap_or(self.proc.default_restorer);
                #[cfg(target_arch = "x86_64")]
                {
                    if (published_sp as *mut usize).vm_write(restorer).is_err() {
                        return Some(SignalOSAction::CoreDump);
                    }
                }

                // Publish the new execution context only after every user
                // write has succeeded. A failed frame/restorer copy therefore
                // cannot leave a partially installed handler context.
                uctx.set_ip(handler);
                uctx.set_sp(published_sp);
                uctx.set_arg0(signo as _);
                uctx.set_arg1(siginfo_ptr);
                uctx.set_arg2(ucontext_ptr);
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
            let sig = self
                .dequeue_thread_signal(&mask)
                .or_else(|| self.proc.dequeue_signal(&mask))?;
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

    /// Sends a signal, preparing any queue record outside spin locks.
    ///
    /// Returns publication and wakeup state separately.
    ///
    /// The preparation closure is skipped for ignored signals and coalesced
    /// standard signals, and is never called under a pending/actions lock.
    #[must_use = "the caller must handle queue-admission failure"]
    pub fn try_send_signal_with<E>(
        &self,
        sig: SignalInfo,
        prepare: impl FnOnce(SignalInfo) -> Result<PreparedSignal, E>,
    ) -> Result<ThreadSignalSendOutcome, E> {
        self.try_send_signal_for_endpoint(EndpointSendMode::Active, sig, prepare)
    }

    /// Sends directly to an exited group leader's retained private endpoint.
    ///
    /// Normal exact-thread sends accept only an active endpoint. Keeping this
    /// operation separate prevents a stale task or pidfd `Arc` from publishing
    /// after ordinary thread retirement while preserving Linux's unreaped
    /// group-leader pending queue.
    #[must_use = "the caller must handle queue-admission failure"]
    pub fn try_send_retained_signal_with<E>(
        &self,
        sig: SignalInfo,
        prepare: impl FnOnce(SignalInfo) -> Result<PreparedSignal, E>,
    ) -> Result<ThreadSignalSendOutcome, E> {
        self.try_send_signal_for_endpoint(EndpointSendMode::Retained, sig, prepare)
    }

    fn try_send_signal_for_endpoint<E>(
        &self,
        mode: EndpointSendMode,
        sig: SignalInfo,
        prepare: impl FnOnce(SignalInfo) -> Result<PreparedSignal, E>,
    ) -> Result<ThreadSignalSendOutcome, E> {
        let signo = sig.signo();
        let inactive = || ThreadSignalSendOutcome {
            published: false,
            wake: false,
        };

        // Fast preflight avoids queue allocation for an inactive, ignored, or
        // already-coalesced endpoint. Exit retirement takes the same lifecycle
        // gate through state change and drain.
        {
            let mut generation_detached = DetachedSignal::empty();
            let generation = ProcessSignalManager::has_generation_effect(signo)
                .then(|| self.proc.action_update.lock());
            let mut lifecycle = self.lifecycle.lock();
            if !mode.accepts(*lifecycle) {
                return Ok(inactive());
            }
            if generation.is_some() {
                // Registration retirement takes action_update before this
                // endpoint gate, allowing the queue walk to run without any
                // outer spin guard while preserving exact endpoint state.
                drop(lifecycle);
                self.proc
                    .apply_generation_effect_locked(signo, &mut generation_detached);
                lifecycle = self.lifecycle.lock();
                if !mode.accepts(*lifecycle) {
                    return Ok(inactive());
                }
            }
            let actions = self.proc.actions.lock();
            let blocked = self.signal_blocked(signo);
            let ignored = match &actions[signo].disposition {
                SignalDisposition::Ignore => true,
                SignalDisposition::Default => {
                    matches!(signo.default_action(), DefaultSignalAction::Ignore)
                }
                SignalDisposition::Handler(_) => false,
            };
            if ignored && !blocked && !self.signal_real_blocked(signo) {
                return Ok(inactive());
            }
            if !signo.is_realtime() && self.pending.lock().set.has(signo) {
                self.possibly_has_signal.store(true, Ordering::Release);
                return Ok(ThreadSignalSendOutcome {
                    published: false,
                    wake: !blocked,
                });
            }
        }

        // Preparation is deliberately outside every spin guard. Retirement
        // may win here; the commit-side lifecycle recheck then rejects and
        // releases the prepared queue charge without publication.
        let mut prepared = Some(prepare(sig)?);
        let mut generation_detached = DetachedSignal::empty();
        let generation = ProcessSignalManager::has_generation_effect(signo)
            .then(|| self.proc.action_update.lock());
        let mut lifecycle = self.lifecycle.lock();
        if !mode.accepts(*lifecycle) {
            drop(lifecycle);
            drop(generation);
            drop(prepared);
            drop(generation_detached);
            return Ok(inactive());
        }
        if generation.is_some() {
            drop(lifecycle);
            self.proc
                .apply_generation_effect_locked(signo, &mut generation_detached);
            lifecycle = self.lifecycle.lock();
            if !mode.accepts(*lifecycle) {
                drop(lifecycle);
                drop(generation);
                drop(prepared);
                drop(generation_detached);
                return Ok(inactive());
            }
        }
        let actions = self.proc.actions.lock();
        let blocked = self.signal_blocked(signo);
        let ignored = match &actions[signo].disposition {
            SignalDisposition::Ignore => true,
            SignalDisposition::Default => {
                matches!(signo.default_action(), DefaultSignalAction::Ignore)
            }
            SignalDisposition::Handler(_) => false,
        };
        let ignored = ignored && !blocked && !self.signal_real_blocked(signo);
        let outcome = if ignored {
            None
        } else {
            let mut pending = self.pending.lock();
            if !signo.is_realtime() && pending.set.has(signo) {
                None
            } else {
                Some(pending.publish(prepared.take().unwrap()))
            }
        };
        drop(actions);
        drop(lifecycle);
        drop(generation);
        // Drop a node made obsolete by retirement, disposition transition, or
        // standard-signal coalescing only after every signal-state guard.
        drop(prepared);
        drop(generation_detached);
        let published = outcome.is_some_and(|outcome| outcome.finish());
        if !ignored {
            self.possibly_has_signal.store(true, Ordering::Release);
        }
        Ok(ThreadSignalSendOutcome {
            published,
            wake: !ignored && !blocked,
        })
    }

    /// Sends a signal through the allocation-free fallback path.
    #[must_use]
    pub fn send_unqueued_signal(&self, sig: SignalInfo) -> bool {
        match self.try_send_signal_with(sig, |sig| {
            Ok::<_, core::convert::Infallible>(PreparedSignal::unqueued(sig))
        }) {
            Ok(outcome) => outcome.wake,
            Err(error) => match error {},
        }
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

    /// Detaches all thread-private pending records under the lock and destroys
    /// them after releasing it.
    pub fn flush_pending(&self) {
        let detached = self.pending.lock().take_all();
        self.possibly_has_signal.store(false, Ordering::Release);
        drop(detached);
    }

    /// Detaches every thread-directed instance of one signal and releases
    /// queue ownership after dropping the pending lock.
    pub fn flush_signal(&self, signo: Signo) {
        let (detached, empty) = {
            let mut pending = self.pending.lock();
            let detached = pending.take_signal(signo);
            (detached, pending.set.is_empty())
        };
        if empty {
            self.possibly_has_signal.store(false, Ordering::Release);
        }
        drop(detached);
    }

    pub(crate) fn detach_signal_into(&self, signo: Signo, detached: &mut DetachedSignal) {
        let empty = {
            let mut pending = self.pending.lock();
            pending.detach_signal_into(signo, detached);
            pending.set.is_empty()
        };
        if empty {
            self.possibly_has_signal.store(false, Ordering::Release);
        }
    }
}
